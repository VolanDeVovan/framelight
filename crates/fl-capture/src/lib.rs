//! SteamVR Headset View capture on the Steam Frame.
//!
//! The view is read with `IVROverlayView::AcquireOverlayView` on the
//! `system.HeadsetView` overlay (works while the overlay is hidden). One
//! compute dispatch (shaders/nv12.comp) samples it, scales it to the output
//! size (aspect kept, black bars) and writes BT.709 limited-range NV12 into
//! the encoder's input buffer. In the default mode those input buffers are
//! the encoder's own MMAP buffers exported as dmabufs and imported into
//! Vulkan as linear storage buffers, so the CPU never touches a pixel. The
//! fallback mode writes a host-visible staging buffer and does one memcpy
//! into the encoder's mmap.
//!
//! The compositor's image is only sampled, never transitioned: it stays in
//! SHADER_READ_ONLY_OPTIMAL. (A first version blitted it after moving it to
//! TRANSFER_SRC and back, as FrameCast does; on this device only the first
//! frame came out, every later one was black.)

pub mod openvr;
pub mod provider;

use std::ffi::CString;
use std::os::fd::{AsRawFd, BorrowedFd, IntoRawFd, OwnedFd};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use ash::vk::{self, Handle};
pub use provider::{SyntheticProvider, ViewImage, ViewProvider, VkHandles};
use tracing::{debug, info, warn};

pub const HEADSET_VIEW: &str = "system.HeadsetView";

/// How a frame reaches the encoder input buffers.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TargetMode {
    /// GPU writes straight into dmabufs exported by the encoder.
    DmaBuf,
    /// GPU writes into host memory, CPU copies into the encoder's mmap.
    Mmap,
}

/// Layout of the encoder's single-plane NV12 input buffers.
#[derive(Clone, Copy, Debug)]
pub struct TargetLayout {
    pub width: u32,
    pub height: u32,
    /// Bytes per row of both planes.
    pub stride: u32,
    /// Luma rows in the buffer (the height, padded by the encoder).
    pub rows: u32,
    /// Byte offset of the chroma plane.
    pub uv_offset: u32,
}

impl TargetLayout {
    /// Bytes of the target buffer that may get written.
    pub fn bytes(&self) -> u64 {
        self.uv_offset as u64 + self.stride as u64 * (self.rows as u64).div_ceil(2)
    }

    /// The shader writes whole 32-bit words: four luma bytes (or two CbCr pairs) per
    /// invocation, so a row may be written up to the next multiple of four past the
    /// picture width; the stride must cover that.
    fn check(&self) -> Result<()> {
        let ok = self.width > 0
            && self.height > 0
            && self.width.is_multiple_of(2)
            && self.height.is_multiple_of(2)
            && self.stride.is_multiple_of(4)
            && self.stride >= self.width.next_multiple_of(4)
            && self.rows >= self.height
            && self.uv_offset.is_multiple_of(4)
            && self.uv_offset as u64 >= self.stride as u64 * self.height as u64;
        if !ok {
            bail!("unusable target layout {self:?}");
        }
        Ok(())
    }
}

/// Push constants of shaders/nv12.comp.
#[repr(C)]
#[derive(Clone, Copy)]
struct Nv12Params {
    uv_min: [f32; 2],
    uv_max: [f32; 2],
    dst_rect: [i32; 4],
    out_size: [u32; 2],
    stride: u32,
    uv_offset: u32,
    srgb_source: i32,
    has_source: i32,
}

struct Compute {
    dsl: vk::DescriptorSetLayout,
    layout: vk::PipelineLayout,
    pipeline: vk::Pipeline,
    sampler: vk::Sampler,
    pool: vk::DescriptorPool,
    set: vk::DescriptorSet,
}

static NV12_SPV: &[u8] = include_bytes!("../shaders/nv12.spv");

#[derive(Clone, Copy, Debug, Default)]
pub struct CaptureInfo {
    /// False when SteamVR had no image yet (a black frame was written).
    pub had_image: bool,
    /// Submit to fence signalled (GPU blit + copy, plus scheduling).
    pub gpu_time: Duration,
    /// CPU memcpy time (Mmap mode only).
    pub copy_time: Duration,
    pub source_width: u32,
    pub source_height: u32,
}

struct Target {
    buffer: vk::Buffer,
    memory: vk::DeviceMemory,
}

struct Staging {
    buffer: vk::Buffer,
    memory: vk::DeviceMemory,
    ptr: *const u8,
    size: usize,
}

pub struct HeadsetCapture {
    _entry: ash::Entry,
    instance: ash::Instance,
    device: ash::Device,
    queue: vk::Queue,
    pool: vk::CommandPool,
    cb: vk::CommandBuffer,
    fence: vk::Fence,
    mem_props: vk::PhysicalDeviceMemoryProperties,
    /// Queue family of `queue`, and the family the encoder buffers are released to.
    qfi: u32,
    foreign_family: u32,
    layout: TargetLayout,
    mode: TargetMode,
    targets: Vec<Target>,
    staging: Option<Staging>,
    compute: Option<Compute>,
    /// Views of the compositor's images (it cycles through a few), by image handle
    /// and format.
    src_views: std::collections::HashMap<(u64, i32), vk::ImageView>,
    last_source: (u32, u32, u32),
    /// Write colour bars instead of the view (pipeline and colour check).
    pub test_pattern: bool,
    provider: Box<dyn ViewProvider>,
}

// Vulkan handles and OpenVR tables are used from one thread at a time.
unsafe impl Send for HeadsetCapture {}

/// Destroys the instance unless construction got far enough to hand it over.
struct InstanceGuard(Option<ash::Instance>);

impl Drop for InstanceGuard {
    fn drop(&mut self) {
        if let Some(i) = self.0.take() {
            // SAFETY: nothing was created from this instance that is still alive.
            unsafe { i.destroy_instance(None) };
        }
    }
}

fn is_unorm8(f: vk::Format) -> bool {
    matches!(
        f,
        vk::Format::R8G8B8A8_UNORM | vk::Format::B8G8R8A8_UNORM | vk::Format::A8B8G8R8_UNORM_PACK32
    )
}

impl HeadsetCapture {
    /// `dmabufs`: the encoder's input buffers (DmaBuf mode) or `None` for the
    /// Mmap fallback. Fails if the dmabuf import fails; the caller decides
    /// whether to fall back.
    pub fn new(
        layout: TargetLayout,
        dmabufs: Option<&[BorrowedFd<'_>]>,
        provider: Box<dyn ViewProvider>,
    ) -> Result<Self> {
        layout.check()?;
        // SAFETY: loads libvulkan.so.1 at run time.
        let entry = unsafe { ash::Entry::load() }.context("loading libvulkan.so.1")?;
        let mut iexts: Vec<CString> = provider
            .instance_extensions()
            .into_iter()
            .filter_map(|s| CString::new(s).ok())
            .collect();
        let available: Vec<String> =
            unsafe { entry.enumerate_instance_extension_properties(None)? }
                .iter()
                .map(|e| {
                    e.extension_name_as_c_str()
                        .unwrap_or(c"")
                        .to_string_lossy()
                        .into_owned()
                })
                .collect();
        iexts.retain(|e| {
            let ok = available.iter().any(|a| a.as_bytes() == e.as_bytes());
            if !ok {
                warn!("instance extension {e:?} wanted by the view provider is missing");
            }
            ok
        });
        let iext_ptrs: Vec<_> = iexts.iter().map(|e| e.as_ptr()).collect();
        let app = vk::ApplicationInfo::default()
            .application_name(c"framelight")
            .api_version(vk::make_api_version(0, 1, 1, 0));
        let instance = unsafe {
            entry.create_instance(
                &vk::InstanceCreateInfo::default()
                    .application_info(&app)
                    .enabled_extension_names(&iext_ptrs),
                None,
            )
        }
        .context("vkCreateInstance")?;
        let mut guard = InstanceGuard(Some(instance));
        let instance = guard.0.as_ref().expect("just set");

        let pd = match provider.physical_device(instance.handle()) {
            Some(pd) => pd,
            None => *unsafe { instance.enumerate_physical_devices()? }
                .first()
                .context("no Vulkan device")?,
        };
        let props = unsafe { instance.get_physical_device_properties(pd) };
        info!(
            "vulkan: {} (api {}.{}.{})",
            props
                .device_name_as_c_str()
                .unwrap_or(c"?")
                .to_string_lossy(),
            vk::api_version_major(props.api_version),
            vk::api_version_minor(props.api_version),
            vk::api_version_patch(props.api_version)
        );
        let mem_props = unsafe { instance.get_physical_device_memory_properties(pd) };

        let dev_available: Vec<String> =
            unsafe { instance.enumerate_device_extension_properties(pd)? }
                .iter()
                .map(|e| {
                    e.extension_name_as_c_str()
                        .unwrap_or(c"")
                        .to_string_lossy()
                        .into_owned()
                })
                .collect();
        let mut dexts: Vec<String> = provider.device_extensions(pd);
        debug!("view provider wants device extensions {dexts:?}");
        if dmabufs.is_some() {
            for e in [
                ash::khr::external_memory_fd::NAME,
                ash::ext::external_memory_dma_buf::NAME,
            ] {
                let e = e.to_string_lossy().into_owned();
                if !dev_available.contains(&e) {
                    bail!("Vulkan device lacks {e}: dmabuf import impossible");
                }
                dexts.push(e);
            }
            // Lets the release barrier hand the buffer to the encoder (a foreign
            // agent), which is what makes the driver flush its caches for it.
            let foreign = ash::ext::queue_family_foreign::NAME.to_string_lossy().into_owned();
            if dev_available.contains(&foreign) {
                dexts.push(foreign);
            }
        }
        dexts.sort();
        dexts.dedup();
        dexts.retain(|e| {
            let ok = dev_available.contains(e);
            if !ok {
                warn!("device extension {e} wanted by the view provider is missing");
            }
            ok
        });
        let dexts: Vec<CString> = dexts
            .into_iter()
            .filter_map(|s| CString::new(s).ok())
            .collect();
        let dext_ptrs: Vec<_> = dexts.iter().map(|e| e.as_ptr()).collect();
        let foreign_family = if dexts
            .iter()
            .any(|e| e.as_c_str() == ash::ext::queue_family_foreign::NAME)
        {
            vk::QUEUE_FAMILY_FOREIGN_EXT
        } else {
            vk::QUEUE_FAMILY_EXTERNAL
        };

        let qfs = unsafe { instance.get_physical_device_queue_family_properties(pd) };
        let qfi = qfs
            .iter()
            .position(|q| q.queue_flags.contains(vk::QueueFlags::GRAPHICS))
            .context("no graphics queue")? as u32;
        // One queue, so its priority is only relative to itself. No
        // VK_KHR_global_priority LOW either: SFRecorder's notes say such a queue never
        // got a turn on the Frame while a game ran.
        let prio = [0.0f32];
        let qci = [vk::DeviceQueueCreateInfo::default()
            .queue_family_index(qfi)
            .queue_priorities(&prio)];
        let device = unsafe {
            instance.create_device(
                pd,
                &vk::DeviceCreateInfo::default()
                    .queue_create_infos(&qci)
                    .enabled_extension_names(&dext_ptrs),
                None,
            )
        }
        .context("vkCreateDevice")?;
        let queue = unsafe { device.get_device_queue(qfi, 0) };

        // From here on `Drop` cleans up after any failure (null handles are no-ops).
        let mut cap = Self {
            _entry: entry,
            instance: guard.0.take().expect("instance still guarded"),
            device,
            queue,
            pool: vk::CommandPool::null(),
            cb: vk::CommandBuffer::null(),
            fence: vk::Fence::null(),
            mem_props,
            qfi,
            foreign_family,
            layout,
            mode: if dmabufs.is_some() {
                TargetMode::DmaBuf
            } else {
                TargetMode::Mmap
            },
            targets: Vec::new(),
            staging: None,
            compute: None,
            src_views: Default::default(),
            last_source: (0, 0, 0),
            test_pattern: false,
            provider,
        };
        cap.pool = unsafe {
            cap.device.create_command_pool(
                &vk::CommandPoolCreateInfo::default()
                    .queue_family_index(qfi)
                    .flags(vk::CommandPoolCreateFlags::RESET_COMMAND_BUFFER),
                None,
            )
        }?;
        cap.cb = unsafe {
            cap.device.allocate_command_buffers(
                &vk::CommandBufferAllocateInfo::default()
                    .command_pool(cap.pool)
                    .level(vk::CommandBufferLevel::PRIMARY)
                    .command_buffer_count(1),
            )
        }?[0];
        cap.fence = unsafe { cap.device.create_fence(&vk::FenceCreateInfo::default(), None) }?;
        cap.provider.attach(&VkHandles {
            instance: &cap.instance,
            device: &cap.device,
            physical_device: pd,
            queue,
            queue_family_index: qfi,
        })?;
        match dmabufs {
            Some(fds) => {
                let fd_ext = ash::khr::external_memory_fd::Device::new(&cap.instance, &cap.device);
                for (i, fd) in fds.iter().enumerate() {
                    let t = cap
                        .import_dmabuf(&fd_ext, *fd)
                        .with_context(|| format!("importing encoder buffer {i}"))?;
                    cap.targets.push(t);
                }
                info!(
                    "capture: {} encoder buffers imported as dmabuf (zero-copy)",
                    cap.targets.len()
                );
            }
            None => {
                cap.staging = Some(cap.create_staging()?);
                info!("capture: mmap fallback (one CPU copy per frame)");
            }
        }
        cap.compute = Some(cap.create_compute()?);
        Ok(cap)
    }

    pub fn mode(&self) -> TargetMode {
        self.mode
    }

    pub fn provider(&self) -> &dyn ViewProvider {
        &*self.provider
    }

    pub fn provider_mut(&mut self) -> &mut dyn ViewProvider {
        &mut *self.provider
    }

    fn mem_type(&self, bits: u32, want: vk::MemoryPropertyFlags) -> Option<u32> {
        (0..self.mem_props.memory_type_count).find(|&i| {
            bits & (1 << i) != 0
                && self.mem_props.memory_types[i as usize]
                    .property_flags
                    .contains(want)
        })
    }

    fn import_dmabuf(
        &self,
        fd_ext: &ash::khr::external_memory_fd::Device,
        fd: BorrowedFd<'_>,
    ) -> Result<Target> {
        let size = unsafe { libc::lseek(fd.as_raw_fd(), 0, libc::SEEK_END) };
        if size <= 0 {
            bail!("dmabuf has no size");
        }
        let needed = self.layout.bytes();
        if (size as u64) < needed {
            bail!("dmabuf is {size} bytes, need {needed}");
        }
        let dmabuf = vk::ExternalMemoryHandleTypeFlags::DMA_BUF_EXT;
        unsafe {
            let mut ext = vk::ExternalMemoryBufferCreateInfo::default().handle_types(dmabuf);
            let buffer = self.device.create_buffer(
                &vk::BufferCreateInfo::default()
                    .size(size as u64)
                    .usage(
                        vk::BufferUsageFlags::TRANSFER_DST | vk::BufferUsageFlags::STORAGE_BUFFER,
                    )
                    .sharing_mode(vk::SharingMode::EXCLUSIVE)
                    .push_next(&mut ext),
                None,
            )?;
            let reqs = self.device.get_buffer_memory_requirements(buffer);
            let mut fdp = vk::MemoryFdPropertiesKHR::default();
            fd_ext
                .get_memory_fd_properties(dmabuf, fd.as_raw_fd(), &mut fdp)
                .context("vkGetMemoryFdPropertiesKHR")?;
            let bits = reqs.memory_type_bits & fdp.memory_type_bits;
            if bits == 0 {
                self.device.destroy_buffer(buffer, None);
                bail!(
                    "no memory type fits (buffer {:#x}, dmabuf {:#x})",
                    reqs.memory_type_bits,
                    fdp.memory_type_bits
                );
            }
            // Vulkan owns the fd once the import succeeds, so hand it a dup.
            let dup: OwnedFd = fd.try_clone_to_owned()?;
            let raw = dup.into_raw_fd();
            let mut import = vk::ImportMemoryFdInfoKHR::default()
                .handle_type(dmabuf)
                .fd(raw);
            let mut dedicated = vk::MemoryDedicatedAllocateInfo::default().buffer(buffer);
            let info = vk::MemoryAllocateInfo::default()
                .allocation_size(reqs.size.max(size as u64))
                .memory_type_index(bits.trailing_zeros())
                .push_next(&mut import)
                .push_next(&mut dedicated);
            let memory = match self.device.allocate_memory(&info, None) {
                Ok(m) => m,
                Err(e) => {
                    libc::close(raw);
                    self.device.destroy_buffer(buffer, None);
                    return Err(e).context("vkAllocateMemory (dmabuf import)");
                }
            };
            if let Err(e) = self.device.bind_buffer_memory(buffer, memory, 0) {
                self.device.destroy_buffer(buffer, None);
                self.device.free_memory(memory, None);
                return Err(e).context("vkBindBufferMemory (dmabuf import)");
            }
            debug!(
                "imported dmabuf of {size} bytes, memory type {} ({:?})",
                bits.trailing_zeros(),
                self.mem_props.memory_types[bits.trailing_zeros() as usize].property_flags
            );
            Ok(Target { buffer, memory })
        }
    }

    fn create_staging(&self) -> Result<Staging> {
        let size = self.layout.bytes();
        unsafe {
            let buffer = self.device.create_buffer(
                &vk::BufferCreateInfo::default().size(size).usage(
                    vk::BufferUsageFlags::TRANSFER_DST | vk::BufferUsageFlags::STORAGE_BUFFER,
                ),
                None,
            )?;
            let reqs = self.device.get_buffer_memory_requirements(buffer);
            let idx = self
                .mem_type(
                    reqs.memory_type_bits,
                    vk::MemoryPropertyFlags::HOST_VISIBLE | vk::MemoryPropertyFlags::HOST_CACHED,
                )
                .or_else(|| {
                    self.mem_type(reqs.memory_type_bits, vk::MemoryPropertyFlags::HOST_VISIBLE)
                })
                .context("no host-visible memory")?;
            let memory = self.device.allocate_memory(
                &vk::MemoryAllocateInfo::default()
                    .allocation_size(reqs.size)
                    .memory_type_index(idx),
                None,
            )?;
            self.device.bind_buffer_memory(buffer, memory, 0)?;
            let ptr =
                self.device
                    .map_memory(memory, 0, vk::WHOLE_SIZE, vk::MemoryMapFlags::empty())?
                    as *const u8;
            Ok(Staging {
                buffer,
                memory,
                ptr,
                size: size as usize,
            })
        }
    }

    /// Captures the current Headset View into target buffer `index`
    /// (DmaBuf mode), or into `dst` (Mmap mode: the encoder's mapping).
    pub fn capture(&mut self, index: usize, dst: Option<&mut [u8]>) -> Result<CaptureInfo> {
        let mut info = CaptureInfo::default();
        let src = if self.test_pattern {
            None
        } else {
            self.provider.acquire()?
        };
        if let Some(t) = src {
            let key = (t.width, t.height, t.format.as_raw() as u32);
            if key != self.last_source {
                info!(
                    "headset view: {}x{} format {:?} samples {}",
                    t.width, t.height, t.format, t.sample_count
                );
                self.last_source = key;
            }
            info.had_image = true;
            info.source_width = t.width;
            info.source_height = t.height;
        }
        let dst_buffer = match self.mode {
            TargetMode::DmaBuf => self.targets.get(index).context("bad target index")?.buffer,
            TargetMode::Mmap => self.staging.as_ref().unwrap().buffer,
        };
        unsafe {
            self.device
                .reset_command_buffer(self.cb, vk::CommandBufferResetFlags::empty())?;
            self.device.begin_command_buffer(
                self.cb,
                &vk::CommandBufferBeginInfo::default()
                    .flags(vk::CommandBufferUsageFlags::ONE_TIME_SUBMIT),
            )?;
            if self.mode == TargetMode::DmaBuf {
                // Take the buffer back from the encoder before overwriting it.
                let acquire = vk::BufferMemoryBarrier::default()
                    .src_access_mask(vk::AccessFlags::empty())
                    .dst_access_mask(vk::AccessFlags::TRANSFER_WRITE | vk::AccessFlags::SHADER_WRITE)
                    .src_queue_family_index(self.foreign_family)
                    .dst_queue_family_index(self.qfi)
                    .buffer(dst_buffer)
                    .offset(0)
                    .size(vk::WHOLE_SIZE);
                self.device.cmd_pipeline_barrier(
                    self.cb,
                    vk::PipelineStageFlags::TOP_OF_PIPE,
                    vk::PipelineStageFlags::TRANSFER | vk::PipelineStageFlags::COMPUTE_SHADER,
                    vk::DependencyFlags::empty(),
                    &[],
                    &[acquire],
                    &[],
                );
            }
        }
        let stage = if self.test_pattern {
            self.record_test_pattern(dst_buffer);
            vk::PipelineStageFlags::TRANSFER
        } else {
            self.record_nv12(dst_buffer, src)?
        };
        let d = &self.device;
        // An encoder buffer is released to the encoder after every write; a plain
        // barrier left GPU cache lines unflushed when it read the frame.
        let (src_family, dst_family) = match self.mode {
            TargetMode::DmaBuf => (self.qfi, self.foreign_family),
            TargetMode::Mmap => (vk::QUEUE_FAMILY_IGNORED, vk::QUEUE_FAMILY_IGNORED),
        };
        unsafe {
            let done = vk::BufferMemoryBarrier::default()
                .src_access_mask(vk::AccessFlags::TRANSFER_WRITE | vk::AccessFlags::SHADER_WRITE)
                .dst_access_mask(vk::AccessFlags::HOST_READ | vk::AccessFlags::MEMORY_READ)
                .src_queue_family_index(src_family)
                .dst_queue_family_index(dst_family)
                .buffer(dst_buffer)
                .offset(0)
                .size(vk::WHOLE_SIZE);
            d.cmd_pipeline_barrier(
                self.cb,
                stage,
                vk::PipelineStageFlags::HOST | vk::PipelineStageFlags::BOTTOM_OF_PIPE,
                vk::DependencyFlags::empty(),
                &[],
                &[done],
                &[],
            );
            d.end_command_buffer(self.cb)?;

            let t0 = Instant::now();
            let cbs = [self.cb];
            d.queue_submit(
                self.queue,
                &[vk::SubmitInfo::default().command_buffers(&cbs)],
                self.fence,
            )
            .context("vkQueueSubmit")?;
            d.wait_for_fences(&[self.fence], true, 1_000_000_000)
                .context("waiting for the capture copy")?;
            d.reset_fences(&[self.fence])?;
            info.gpu_time = t0.elapsed();
        }

        if self.mode == TargetMode::Mmap {
            let st = self.staging.as_ref().unwrap();
            let dst = dst.context("Mmap mode needs a destination")?;
            let t0 = Instant::now();
            unsafe {
                let range = vk::MappedMemoryRange::default()
                    .memory(st.memory)
                    .offset(0)
                    .size(vk::WHOLE_SIZE);
                self.device.invalidate_mapped_memory_ranges(&[range])?;
                let n = st.size.min(dst.len());
                std::ptr::copy_nonoverlapping(st.ptr, dst.as_mut_ptr(), n);
            }
            info.copy_time = t0.elapsed();
        }
        Ok(info)
    }

    fn create_compute(&self) -> Result<Compute> {
        let d = &self.device;
        unsafe {
            let code = ash::util::read_spv(&mut std::io::Cursor::new(NV12_SPV))?;
            let module =
                d.create_shader_module(&vk::ShaderModuleCreateInfo::default().code(&code), None)?;
            let binds = [
                vk::DescriptorSetLayoutBinding::default()
                    .binding(0)
                    .descriptor_type(vk::DescriptorType::COMBINED_IMAGE_SAMPLER)
                    .descriptor_count(1)
                    .stage_flags(vk::ShaderStageFlags::COMPUTE),
                vk::DescriptorSetLayoutBinding::default()
                    .binding(1)
                    .descriptor_type(vk::DescriptorType::STORAGE_BUFFER)
                    .descriptor_count(1)
                    .stage_flags(vk::ShaderStageFlags::COMPUTE),
            ];
            let dsl = d.create_descriptor_set_layout(
                &vk::DescriptorSetLayoutCreateInfo::default().bindings(&binds),
                None,
            )?;
            let pcr = [vk::PushConstantRange::default()
                .stage_flags(vk::ShaderStageFlags::COMPUTE)
                .size(size_of::<Nv12Params>() as u32)];
            let dsls = [dsl];
            let layout = d.create_pipeline_layout(
                &vk::PipelineLayoutCreateInfo::default()
                    .set_layouts(&dsls)
                    .push_constant_ranges(&pcr),
                None,
            )?;
            let stage = vk::PipelineShaderStageCreateInfo::default()
                .stage(vk::ShaderStageFlags::COMPUTE)
                .module(module)
                .name(c"main");
            let pipeline = d
                .create_compute_pipelines(
                    vk::PipelineCache::null(),
                    &[vk::ComputePipelineCreateInfo::default()
                        .stage(stage)
                        .layout(layout)],
                    None,
                )
                .map_err(|(_, e)| e)?[0];
            d.destroy_shader_module(module, None);
            let sampler = d.create_sampler(
                &vk::SamplerCreateInfo::default()
                    .mag_filter(vk::Filter::LINEAR)
                    .min_filter(vk::Filter::LINEAR)
                    .address_mode_u(vk::SamplerAddressMode::CLAMP_TO_EDGE)
                    .address_mode_v(vk::SamplerAddressMode::CLAMP_TO_EDGE)
                    .address_mode_w(vk::SamplerAddressMode::CLAMP_TO_EDGE),
                None,
            )?;
            let sizes = [
                vk::DescriptorPoolSize {
                    ty: vk::DescriptorType::COMBINED_IMAGE_SAMPLER,
                    descriptor_count: 1,
                },
                vk::DescriptorPoolSize {
                    ty: vk::DescriptorType::STORAGE_BUFFER,
                    descriptor_count: 1,
                },
            ];
            let pool = d.create_descriptor_pool(
                &vk::DescriptorPoolCreateInfo::default()
                    .max_sets(1)
                    .pool_sizes(&sizes),
                None,
            )?;
            let set = d.allocate_descriptor_sets(
                &vk::DescriptorSetAllocateInfo::default()
                    .descriptor_pool(pool)
                    .set_layouts(&dsls),
            )?[0];
            Ok(Compute {
                dsl,
                layout,
                pipeline,
                sampler,
                pool,
                set,
            })
        }
    }

    fn source_view(&mut self, t: &ViewImage) -> Result<vk::ImageView> {
        let key = (t.image.as_raw(), t.format.as_raw());
        if let Some(v) = self.src_views.get(&key) {
            return Ok(*v);
        }
        if self.src_views.len() > 8 {
            // Old images were freed and their handles may come back: start over.
            unsafe {
                self.device.device_wait_idle()?;
                for (_, v) in self.src_views.drain() {
                    self.device.destroy_image_view(v, None);
                }
            }
        }
        let range = vk::ImageSubresourceRange::default()
            .aspect_mask(vk::ImageAspectFlags::COLOR)
            .level_count(1)
            .layer_count(1);
        let v = unsafe {
            self.device.create_image_view(
                &vk::ImageViewCreateInfo::default()
                    .image(t.image)
                    .view_type(vk::ImageViewType::TYPE_2D)
                    .format(t.format)
                    .subresource_range(range),
                None,
            )
        }?;
        self.src_views.insert(key, v);
        Ok(v)
    }

    /// NV12 target: one compute pass samples the view (already in
    /// SHADER_READ_ONLY_OPTIMAL, so no layout changes on the compositor's
    /// image) and writes luma + chroma into the buffer.
    fn record_nv12(
        &mut self,
        dst_buffer: vk::Buffer,
        src: Option<ViewImage>,
    ) -> Result<vk::PipelineStageFlags> {
        let (w, h) = (self.layout.width, self.layout.height);
        let Some(t) = src else {
            self.record_fill_nv12(dst_buffer, &[(0, h, [16, 128, 128])]);
            return Ok(vk::PipelineStageFlags::TRANSFER);
        };
        let view = self.source_view(&t)?;
        let c = self.compute.as_ref().context("compute pipeline missing")?;
        let fmt = t.format;
        let b = t.bounds;
        let params = Nv12Params {
            uv_min: [b.u_min, b.v_min],
            uv_max: [b.u_max, b.v_max],
            dst_rect: fit_rect(&t, w, h),
            out_size: [w, h],
            stride: self.layout.stride,
            uv_offset: self.layout.uv_offset,
            srgb_source: if is_unorm8(fmt) { 0 } else { 1 },
            has_source: 1,
        };
        let d = &self.device;
        unsafe {
            let img = [vk::DescriptorImageInfo::default()
                .sampler(c.sampler)
                .image_view(view)
                .image_layout(vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL)];
            let buf = [vk::DescriptorBufferInfo::default()
                .buffer(dst_buffer)
                .offset(0)
                .range(vk::WHOLE_SIZE)];
            let writes = [
                vk::WriteDescriptorSet::default()
                    .dst_set(c.set)
                    .dst_binding(0)
                    .descriptor_type(vk::DescriptorType::COMBINED_IMAGE_SAMPLER)
                    .image_info(&img),
                vk::WriteDescriptorSet::default()
                    .dst_set(c.set)
                    .dst_binding(1)
                    .descriptor_type(vk::DescriptorType::STORAGE_BUFFER)
                    .buffer_info(&buf),
            ];
            d.update_descriptor_sets(&writes, &[]);
            // Make the compositor's writes to the view visible to the shader.
            let mb = [vk::MemoryBarrier::default()
                .src_access_mask(vk::AccessFlags::MEMORY_WRITE)
                .dst_access_mask(vk::AccessFlags::SHADER_READ)];
            d.cmd_pipeline_barrier(
                self.cb,
                vk::PipelineStageFlags::ALL_COMMANDS,
                vk::PipelineStageFlags::COMPUTE_SHADER,
                vk::DependencyFlags::empty(),
                &mb,
                &[],
                &[],
            );
            d.cmd_bind_pipeline(self.cb, vk::PipelineBindPoint::COMPUTE, c.pipeline);
            d.cmd_bind_descriptor_sets(
                self.cb,
                vk::PipelineBindPoint::COMPUTE,
                c.layout,
                0,
                &[c.set],
                &[],
            );
            let bytes = std::slice::from_raw_parts(
                &params as *const Nv12Params as *const u8,
                size_of::<Nv12Params>(),
            );
            d.cmd_push_constants(self.cb, c.layout, vk::ShaderStageFlags::COMPUTE, 0, bytes);
            d.cmd_dispatch(self.cb, w.div_ceil(4).div_ceil(8), (h / 2).div_ceil(8), 1);
        }
        Ok(vk::PipelineStageFlags::COMPUTE_SHADER)
    }

    /// Fills row bands of an NV12 target with flat colours (Y, Cb, Cr).
    fn record_fill_nv12(&self, dst_buffer: vk::Buffer, bands: &[(u32, u32, [u8; 3])]) {
        let stride = self.layout.stride as u64;
        let uv = self.layout.uv_offset as u64;
        for &(r0, r1, [y, cb, cr]) in bands {
            let (r0, r1) = (r0 as u64 & !1, (r1 as u64 + 1) & !1);
            let yw = u32::from_ne_bytes([y; 4]);
            let cw = u32::from_ne_bytes([cb, cr, cb, cr]);
            unsafe {
                self.device.cmd_fill_buffer(
                    self.cb,
                    dst_buffer,
                    r0 * stride,
                    (r1 - r0) * stride,
                    yw,
                );
                self.device.cmd_fill_buffer(
                    self.cb,
                    dst_buffer,
                    uv + r0 / 2 * stride,
                    (r1 - r0) / 2 * stride,
                    cw,
                );
            }
        }
    }

    /// Eight horizontal bars: white yellow cyan green magenta red blue black.
    fn record_test_pattern(&self, dst_buffer: vk::Buffer) {
        const BARS: [[u8; 3]; 8] = [
            [255, 255, 255],
            [255, 255, 0],
            [0, 255, 255],
            [0, 255, 0],
            [255, 0, 255],
            [255, 0, 0],
            [0, 0, 255],
            [0, 0, 0],
        ];
        let h = self.layout.height;
        let bands: Vec<_> = BARS
            .iter()
            .enumerate()
            .map(|(k, c)| (h * k as u32 / 8, h * (k as u32 + 1) / 8, rgb_to_ycbcr709(*c)))
            .collect();
        self.record_fill_nv12(dst_buffer, &bands);
    }
}

impl Drop for HeadsetCapture {
    fn drop(&mut self) {
        unsafe {
            let _ = self.device.device_wait_idle();
            self.provider.detach(&self.device);
            for t in self.targets.drain(..) {
                self.device.destroy_buffer(t.buffer, None);
                self.device.free_memory(t.memory, None);
            }
            if let Some(s) = self.staging.take() {
                self.device.destroy_buffer(s.buffer, None);
                self.device.free_memory(s.memory, None);
            }
            for (_, v) in self.src_views.drain() {
                self.device.destroy_image_view(v, None);
            }
            if let Some(c) = self.compute.take() {
                self.device.destroy_pipeline(c.pipeline, None);
                self.device.destroy_pipeline_layout(c.layout, None);
                self.device.destroy_descriptor_pool(c.pool, None);
                self.device.destroy_descriptor_set_layout(c.dsl, None);
                self.device.destroy_sampler(c.sampler, None);
            }
            self.device.destroy_fence(self.fence, None);
            self.device.destroy_command_pool(self.pool, None);
            self.device.destroy_device(None);
            self.instance.destroy_instance(None);
        }
        // The provider (OpenVR: VR_Shutdown) drops after this.
    }
}

/// Where the view lands in a `w` x `h` output, keeping its aspect ratio.
fn fit_rect(t: &ViewImage, w: u32, h: u32) -> [i32; 4] {
    let b = &t.bounds;
    let sw = ((b.u_max - b.u_min).abs() * t.width as f32).max(1.0);
    let sh = ((b.v_max - b.v_min).abs() * t.height as f32).max(1.0);
    let scale = (w as f32 / sw).min(h as f32 / sh);
    let (dw, dh) = ((sw * scale).round() as i32, (sh * scale).round() as i32);
    [(w as i32 - dw) / 2, (h as i32 - dh) / 2, dw, dh]
}

/// BT.709 limited-range Y, Cb, Cr of an 8-bit (gamma-encoded) RGB colour.
pub fn rgb_to_ycbcr709(c: [u8; 3]) -> [u8; 3] {
    let [r, g, b] = c.map(|v| v as f32 / 255.0);
    let y = 0.2126 * r + 0.7152 * g + 0.0722 * b;
    let q = |v: f32| v.round().clamp(0.0, 255.0) as u8;
    [
        q(16.0 + 219.0 * y),
        q(128.0 + 224.0 * (b - y) / 1.8556),
        q(128.0 + 224.0 * (r - y) / 1.5748),
    ]
}

/// What the NV12 shader should produce for an RGBA8 UNORM picture of the
/// output size (no scaling, no bars): the CPU reference for self-tests.
/// Returns the buffer image (`uv_offset + stride * rows / 2` bytes); bytes
/// outside the picture are left 0.
pub fn expected_nv12(rgba: &[u8], width: u32, height: u32, stride: u32, uv_offset: u32) -> Vec<u8> {
    let (w, h, s) = (width as usize, height as usize, stride as usize);
    let mut out = vec![0u8; uv_offset as usize + s * h.div_ceil(2)];
    let px = |x: usize, y: usize| -> [f32; 3] {
        let i = (y * w + x) * 4;
        [
            rgba[i] as f32 / 255.0,
            rgba[i + 1] as f32 / 255.0,
            rgba[i + 2] as f32 / 255.0,
        ]
    };
    let luma = |c: [f32; 3]| 0.2126 * c[0] + 0.7152 * c[1] + 0.0722 * c[2];
    let q = |v: f32| v.round().clamp(0.0, 255.0) as u8;
    for y in 0..h {
        for x in 0..w {
            out[y * s + x] = q(16.0 + 219.0 * luma(px(x, y)));
        }
    }
    for by in 0..h / 2 {
        for bx in 0..w / 2 {
            let mut c = [0f32; 3];
            for (dx, dy) in [(0, 0), (1, 0), (0, 1), (1, 1)] {
                let p = px(bx * 2 + dx, by * 2 + dy);
                for k in 0..3 {
                    c[k] += p[k] * 0.25;
                }
            }
            let y = luma(c);
            let o = uv_offset as usize + by * s + bx * 2;
            out[o] = q(128.0 + 224.0 * (c[2] - y) / 1.8556);
            out[o + 1] = q(128.0 + 224.0 * (c[0] - y) / 1.5748);
        }
    }
    out
}
