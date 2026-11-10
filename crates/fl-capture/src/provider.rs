//! Where the picture comes from. [`HeadsetCapture`](crate::HeadsetCapture)
//! only needs a Vulkan image to sample each frame; the OpenVR Headset View
//! ([`crate::openvr::OpenVrProvider`]) and a synthetic test pattern
//! ([`SyntheticProvider`], no SteamVR needed) both provide one.

use anyhow::{Context, Result};
use ash::vk;

use crate::openvr::{FrameTiming, TextureBounds};

/// The Vulkan objects a provider may need (all owned by the capture).
pub struct VkHandles<'a> {
    pub instance: &'a ash::Instance,
    pub device: &'a ash::Device,
    pub physical_device: vk::PhysicalDevice,
    pub queue: vk::Queue,
    pub queue_family_index: u32,
}

/// One frame to sample. The image must be in SHADER_READ_ONLY_OPTIMAL and
/// stay valid until the next `acquire`.
#[derive(Clone, Copy, Debug)]
pub struct ViewImage {
    pub image: vk::Image,
    pub format: vk::Format,
    pub width: u32,
    pub height: u32,
    pub sample_count: u32,
    /// Valid area, normalised.
    pub bounds: TextureBounds,
}

pub trait ViewProvider: Send {
    /// Instance extensions the provider needs.
    fn instance_extensions(&self) -> Vec<String> {
        Vec::new()
    }
    /// The physical device to use (`None`: the first one).
    fn physical_device(&self, _instance: vk::Instance) -> Option<vk::PhysicalDevice> {
        None
    }
    /// Device extensions the provider needs on `pd`.
    fn device_extensions(&self, _pd: vk::PhysicalDevice) -> Vec<String> {
        Vec::new()
    }
    /// Called once the device exists.
    fn attach(&mut self, vk: &VkHandles<'_>) -> Result<()>;
    /// The latest image, or `None` when there is none yet.
    fn acquire(&mut self) -> Result<Option<ViewImage>>;
    /// Called before the device is destroyed: free everything Vulkan.
    fn detach(&mut self, device: &ash::Device);
    /// True once the runtime wants us gone.
    fn quit_requested(&mut self) -> bool {
        false
    }
    /// Compositor frame timings, if the runtime has them (oldest first).
    fn frame_timings(&self, _n: u32) -> Vec<FrameTiming> {
        Vec::new()
    }
}

/// Moving test picture rendered by the GPU-side upload of a CPU pattern.
/// RGBA8 UNORM, so sampling at texel centres reproduces it exactly and the
/// NV12 that reaches the encoder can be checked byte for byte.
pub struct SyntheticProvider {
    width: u32,
    height: u32,
    frame: u32,
    st: Option<SynthState>,
}

struct SynthState {
    device: ash::Device,
    queue: vk::Queue,
    image: vk::Image,
    image_mem: vk::DeviceMemory,
    staging: vk::Buffer,
    staging_mem: vk::DeviceMemory,
    ptr: *mut u8,
    pool: vk::CommandPool,
    cb: vk::CommandBuffer,
    fence: vk::Fence,
}

unsafe impl Send for SyntheticProvider {}

impl SyntheticProvider {
    pub fn new(width: u32, height: u32) -> Self {
        Self {
            width,
            height,
            frame: 0,
            st: None,
        }
    }

    /// Frame number of the picture the last `acquire` returned.
    pub fn last_frame(&self) -> u32 {
        self.frame.wrapping_sub(1)
    }

    /// The RGBA picture of frame `n`: horizontal red ramp, vertical green
    /// ramp, blue stepping per frame, and a white bar moving to the right.
    pub fn pattern(width: u32, height: u32, n: u32) -> Vec<u8> {
        let mut v = vec![0u8; (width * height * 4) as usize];
        let bar = (n * 16) % width.max(1);
        for y in 0..height {
            for x in 0..width {
                let i = ((y * width + x) * 4) as usize;
                let on_bar = x >= bar && x < bar + 32;
                let px = if on_bar {
                    [255, 255, 255, 255]
                } else {
                    [
                        (x * 255 / (width - 1).max(1)) as u8,
                        (y * 255 / (height - 1).max(1)) as u8,
                        (n.wrapping_mul(8) & 0xff) as u8,
                        255,
                    ]
                };
                v[i..i + 4].copy_from_slice(&px);
            }
        }
        v
    }
}

fn mem_type(
    props: &vk::PhysicalDeviceMemoryProperties,
    bits: u32,
    want: vk::MemoryPropertyFlags,
) -> Option<u32> {
    (0..props.memory_type_count).find(|&i| {
        bits & (1 << i) != 0 && props.memory_types[i as usize].property_flags.contains(want)
    })
}

impl ViewProvider for SyntheticProvider {
    fn attach(&mut self, h: &VkHandles<'_>) -> Result<()> {
        let d = h.device;
        let props = unsafe {
            h.instance
                .get_physical_device_memory_properties(h.physical_device)
        };
        unsafe {
            let image = d.create_image(
                &vk::ImageCreateInfo::default()
                    .image_type(vk::ImageType::TYPE_2D)
                    .format(vk::Format::R8G8B8A8_UNORM)
                    .extent(vk::Extent3D {
                        width: self.width,
                        height: self.height,
                        depth: 1,
                    })
                    .mip_levels(1)
                    .array_layers(1)
                    .samples(vk::SampleCountFlags::TYPE_1)
                    .tiling(vk::ImageTiling::OPTIMAL)
                    .usage(vk::ImageUsageFlags::TRANSFER_DST | vk::ImageUsageFlags::SAMPLED)
                    .initial_layout(vk::ImageLayout::UNDEFINED),
                None,
            )?;
            let r = d.get_image_memory_requirements(image);
            let image_mem = d.allocate_memory(
                &vk::MemoryAllocateInfo::default()
                    .allocation_size(r.size)
                    .memory_type_index(
                        mem_type(
                            &props,
                            r.memory_type_bits,
                            vk::MemoryPropertyFlags::DEVICE_LOCAL,
                        )
                        .or_else(|| {
                            mem_type(&props, r.memory_type_bits, vk::MemoryPropertyFlags::empty())
                        })
                        .context("no memory for the test image")?,
                    ),
                None,
            )?;
            d.bind_image_memory(image, image_mem, 0)?;
            let size = (self.width * self.height * 4) as u64;
            let staging = d.create_buffer(
                &vk::BufferCreateInfo::default()
                    .size(size)
                    .usage(vk::BufferUsageFlags::TRANSFER_SRC),
                None,
            )?;
            let r = d.get_buffer_memory_requirements(staging);
            let staging_mem = d.allocate_memory(
                &vk::MemoryAllocateInfo::default()
                    .allocation_size(r.size)
                    .memory_type_index(
                        mem_type(
                            &props,
                            r.memory_type_bits,
                            vk::MemoryPropertyFlags::HOST_VISIBLE
                                | vk::MemoryPropertyFlags::HOST_COHERENT,
                        )
                        .context("no host-visible memory")?,
                    ),
                None,
            )?;
            d.bind_buffer_memory(staging, staging_mem, 0)?;
            let ptr = d.map_memory(staging_mem, 0, vk::WHOLE_SIZE, vk::MemoryMapFlags::empty())?
                as *mut u8;
            let pool = d.create_command_pool(
                &vk::CommandPoolCreateInfo::default()
                    .queue_family_index(h.queue_family_index)
                    .flags(vk::CommandPoolCreateFlags::RESET_COMMAND_BUFFER),
                None,
            )?;
            let cb = d.allocate_command_buffers(
                &vk::CommandBufferAllocateInfo::default()
                    .command_pool(pool)
                    .command_buffer_count(1),
            )?[0];
            let fence = d.create_fence(&vk::FenceCreateInfo::default(), None)?;
            self.st = Some(SynthState {
                device: d.clone(),
                queue: h.queue,
                image,
                image_mem,
                staging,
                staging_mem,
                ptr,
                pool,
                cb,
                fence,
            });
        }
        Ok(())
    }

    fn acquire(&mut self) -> Result<Option<ViewImage>> {
        let st = self
            .st
            .as_ref()
            .context("synthetic provider not attached")?;
        let pic = Self::pattern(self.width, self.height, self.frame);
        let d = &st.device;
        let range = vk::ImageSubresourceRange::default()
            .aspect_mask(vk::ImageAspectFlags::COLOR)
            .level_count(1)
            .layer_count(1);
        unsafe {
            std::ptr::copy_nonoverlapping(pic.as_ptr(), st.ptr, pic.len());
            d.reset_command_buffer(st.cb, vk::CommandBufferResetFlags::empty())?;
            d.begin_command_buffer(
                st.cb,
                &vk::CommandBufferBeginInfo::default()
                    .flags(vk::CommandBufferUsageFlags::ONE_TIME_SUBMIT),
            )?;
            let to_dst = vk::ImageMemoryBarrier::default()
                .old_layout(vk::ImageLayout::UNDEFINED)
                .new_layout(vk::ImageLayout::TRANSFER_DST_OPTIMAL)
                .dst_access_mask(vk::AccessFlags::TRANSFER_WRITE)
                .src_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
                .dst_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
                .image(st.image)
                .subresource_range(range);
            d.cmd_pipeline_barrier(
                st.cb,
                vk::PipelineStageFlags::ALL_COMMANDS,
                vk::PipelineStageFlags::TRANSFER,
                vk::DependencyFlags::empty(),
                &[],
                &[],
                &[to_dst],
            );
            let region = vk::BufferImageCopy::default()
                .image_subresource(
                    vk::ImageSubresourceLayers::default()
                        .aspect_mask(vk::ImageAspectFlags::COLOR)
                        .layer_count(1),
                )
                .image_extent(vk::Extent3D {
                    width: self.width,
                    height: self.height,
                    depth: 1,
                });
            d.cmd_copy_buffer_to_image(
                st.cb,
                st.staging,
                st.image,
                vk::ImageLayout::TRANSFER_DST_OPTIMAL,
                &[region],
            );
            let to_read = vk::ImageMemoryBarrier::default()
                .old_layout(vk::ImageLayout::TRANSFER_DST_OPTIMAL)
                .new_layout(vk::ImageLayout::SHADER_READ_ONLY_OPTIMAL)
                .src_access_mask(vk::AccessFlags::TRANSFER_WRITE)
                .dst_access_mask(vk::AccessFlags::SHADER_READ)
                .src_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
                .dst_queue_family_index(vk::QUEUE_FAMILY_IGNORED)
                .image(st.image)
                .subresource_range(range);
            d.cmd_pipeline_barrier(
                st.cb,
                vk::PipelineStageFlags::TRANSFER,
                vk::PipelineStageFlags::ALL_COMMANDS,
                vk::DependencyFlags::empty(),
                &[],
                &[],
                &[to_read],
            );
            d.end_command_buffer(st.cb)?;
            let cbs = [st.cb];
            d.queue_submit(
                st.queue,
                &[vk::SubmitInfo::default().command_buffers(&cbs)],
                st.fence,
            )?;
            d.wait_for_fences(&[st.fence], true, 1_000_000_000)?;
            d.reset_fences(&[st.fence])?;
        }
        self.frame = self.frame.wrapping_add(1);
        Ok(Some(ViewImage {
            image: st.image,
            format: vk::Format::R8G8B8A8_UNORM,
            width: self.width,
            height: self.height,
            sample_count: 1,
            bounds: TextureBounds {
                u_min: 0.0,
                v_min: 0.0,
                u_max: 1.0,
                v_max: 1.0,
            },
        }))
    }

    fn detach(&mut self, device: &ash::Device) {
        if let Some(st) = self.st.take() {
            unsafe {
                device.destroy_fence(st.fence, None);
                device.destroy_command_pool(st.pool, None);
                device.destroy_buffer(st.staging, None);
                device.free_memory(st.staging_mem, None);
                device.destroy_image(st.image, None);
                device.free_memory(st.image_mem, None);
            }
        }
    }
}
