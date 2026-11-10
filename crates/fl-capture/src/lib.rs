//! SteamVR Headset View capture on the Steam Frame.
//!
//! The view is read with `IVROverlayView::AcquireOverlayView` on the
//! `system.HeadsetView` overlay (works while the overlay is hidden). One
//! compute dispatch (shaders/nv12.comp) samples it, scales it to the output
//! size (aspect kept, black bars) and writes BT.709 limited-range NV12 into a
//! host-visible buffer; one memcpy then moves the frame into the encoder's
//! input buffer.
//!
//! The compositor's image is only sampled, never transitioned: it stays in
//! SHADER_READ_ONLY_OPTIMAL. (A first version blitted it after moving it to
//! TRANSFER_SRC and back, as FrameCast does; on this device only the first
//! frame came out, every later one was black.)

pub mod openvr;
pub mod provider;

use std::ffi::CString;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use ash::vk::{self, Handle};
use fl_core::Nv12Layout;
pub use provider::{SyntheticProvider, ViewImage, ViewProvider, VkHandles};
use tracing::{debug, info, warn};

pub const HEADSET_VIEW: &str = "system.HeadsetView";

/// The shader writes whole 32-bit words: four luma bytes (or two CbCr pairs) per
/// invocation, so a row may be written up to the next multiple of four past the
/// picture width; the stride must cover that.
fn check_layout(l: &Nv12Layout) -> Result<()> {
    let ok = l.width > 0
        && l.height > 0
        && l.width.is_multiple_of(2)
        && l.height.is_multiple_of(2)
        && l.stride.is_multiple_of(4)
        && l.stride >= l.width.next_multiple_of(4)
        && l.rows >= l.height
        && l.uv_offset.is_multiple_of(4)
        && l.uv_offset as u64 >= l.stride as u64 * l.height as u64;
    if !ok {
        bail!("unusable encoder input layout {l:?}");
    }
    Ok(())
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
    /// Always 1: a missing view is filled black without the shader.
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
    /// False when SteamVR had no image (a black frame was written).
    pub had_image: bool,
    /// Submit to fence signalled (the compute pass, plus scheduling).
    pub gpu_time: Duration,
    /// The memcpy into the encoder's buffer.
    pub copy_time: Duration,
}

/// The host-visible buffer the compute pass writes.
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
    layout: Nv12Layout,
    staging: Option<Staging>,
    compute: Option<Compute>,
    /// Views of the compositor's images (it cycles through a few), by image handle,
    /// format and size.
    src_views: std::collections::HashMap<(u64, i32, u32, u32), vk::ImageView>,
    /// (width, height, format) of the view last logged.
    last_source: (u32, u32, i32),
    provider: Box<dyn ViewProvider>,
}

// SAFETY: Vulkan handles and OpenVR tables are used from one thread at a time.
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
    /// `layout`: the encoder's input buffers, which every [`HeadsetCapture::capture`]
    /// fills. `provider`: where the picture comes from.
    pub fn new(layout: Nv12Layout, provider: Box<dyn ViewProvider>) -> Result<Self> {
        check_layout(&layout)?;
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
            layout,
            staging: None,
            compute: None,
            src_views: Default::default(),
            last_source: (0, 0, 0),
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
        cap.fence = unsafe {
            cap.device
                .create_fence(&vk::FenceCreateInfo::default(), None)
        }?;
        cap.provider.attach(&VkHandles {
            instance: &cap.instance,
            device: &cap.device,
            physical_device: pd,
            queue,
            queue_family_index: qfi,
        })?;
        cap.staging = Some(cap.create_staging()?);
        cap.compute = Some(cap.create_compute()?);
        Ok(cap)
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

    fn create_staging(&self) -> Result<Staging> {
        let size = self.layout.bytes() as u64;
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

    /// Converts the current view into `dst`, an encoder input buffer of the layout
    /// given to [`HeadsetCapture::new`]. Without a view (headset in standby) the frame
    /// is black.
    pub fn capture(&mut self, dst: &mut [u8]) -> Result<CaptureInfo> {
        let mut info = CaptureInfo::default();
        let src = self.provider.acquire()?;
        if let Some(t) = src {
            let key = (t.width, t.height, t.format.as_raw());
            if key != self.last_source {
                info!(
                    "headset view: {}x{} format {:?} samples {}",
                    t.width, t.height, t.format, t.sample_count
                );
                self.last_source = key;
            }
            info.had_image = true;
        }
        let (buffer, memory, ptr, size) = {
            let st = self.staging.as_ref().context("capture not initialised")?;
            (st.buffer, st.memory, st.ptr, st.size)
        };
        if dst.len() < size {
            bail!(
                "encoder buffer of {} bytes, the frame needs {size}",
                dst.len()
            );
        }
        // SAFETY: every handle belongs to `self.device` and is used from this thread
        // only; the command buffer is not pending (the previous submit was waited for).
        unsafe {
            self.device
                .reset_command_buffer(self.cb, vk::CommandBufferResetFlags::empty())?;
            self.device.begin_command_buffer(
                self.cb,
                &vk::CommandBufferBeginInfo::default()
                    .flags(vk::CommandBufferUsageFlags::ONE_TIME_SUBMIT),
            )?;
        }
        let stage = match src {
            Some(t) => self.record_nv12(buffer, &t)?,
            None => self.record_black(buffer),
        };
        let d = &self.device;
        // SAFETY: as above; `ptr` maps `size` bytes of `memory` for the device's life.
        unsafe {
            let done = vk::BufferMemoryBarrier::default()
                .src_access_mask(vk::AccessFlags::TRANSFER_WRITE | vk::AccessFlags::SHADER_WRITE)
                .dst_access_mask(vk::AccessFlags::HOST_READ | vk::AccessFlags::MEMORY_READ)
                .src_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
                .dst_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
                .buffer(buffer)
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
                .context("waiting for the GPU conversion")?;
            d.reset_fences(&[self.fence])?;
            info.gpu_time = t0.elapsed();

            let t0 = Instant::now();
            let range = vk::MappedMemoryRange::default()
                .memory(memory)
                .offset(0)
                .size(vk::WHOLE_SIZE);
            d.invalidate_mapped_memory_ranges(&[range])?;
            std::ptr::copy_nonoverlapping(ptr, dst.as_mut_ptr(), size);
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
        let key = (t.image.as_raw(), t.format.as_raw(), t.width, t.height);
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

    /// One compute pass samples the view (already in SHADER_READ_ONLY_OPTIMAL, so no
    /// layout changes on the compositor's image) and writes luma + chroma into the
    /// buffer. Returns the pipeline stage that wrote it.
    fn record_nv12(
        &mut self,
        dst_buffer: vk::Buffer,
        t: &ViewImage,
    ) -> Result<vk::PipelineStageFlags> {
        let (w, h) = (self.layout.width, self.layout.height);
        let view = self.source_view(t)?;
        let c = self.compute.as_ref().context("compute pipeline missing")?;
        let fmt = t.format;
        let b = t.bounds;
        let params = Nv12Params {
            uv_min: [b.u_min, b.v_min],
            uv_max: [b.u_max, b.v_max],
            dst_rect: fit_rect(t, w, h),
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

    /// A black frame: luma 16, chroma 128 over the whole buffer.
    fn record_black(&self, dst_buffer: vk::Buffer) -> vk::PipelineStageFlags {
        let uv = self.layout.uv_offset as u64;
        // SAFETY: recording into this capture's command buffer; the fills stay inside
        // the buffer (`uv_offset` is a multiple of four, checked in `new`).
        unsafe {
            self.device
                .cmd_fill_buffer(self.cb, dst_buffer, 0, uv, u32::from_ne_bytes([16; 4]));
            self.device.cmd_fill_buffer(
                self.cb,
                dst_buffer,
                uv,
                vk::WHOLE_SIZE,
                u32::from_ne_bytes([128; 4]),
            );
        }
        vk::PipelineStageFlags::TRANSFER
    }
}

impl Drop for HeadsetCapture {
    fn drop(&mut self) {
        unsafe {
            let _ = self.device.device_wait_idle();
            self.provider.detach(&self.device);
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

/// What the NV12 shader should produce for an RGBA8 UNORM picture of the
/// output size (no scaling, no bars): the CPU reference for the self-test.
/// Returns a whole buffer of layout `l`; bytes outside the picture are left 0.
pub fn expected_nv12(rgba: &[u8], l: &Nv12Layout) -> Vec<u8> {
    let (w, h, s) = (l.width as usize, l.height as usize, l.stride as usize);
    let uv_offset = l.uv_offset as usize;
    let mut out = vec![0u8; l.bytes()];
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
            let o = uv_offset + by * s + bx * 2;
            out[o] = q(128.0 + 224.0 * (c[2] - y) / 1.8556);
            out[o + 1] = q(128.0 + 224.0 * (c[0] - y) / 1.5748);
        }
    }
    out
}
