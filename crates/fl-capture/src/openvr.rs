//! The handful of OpenVR calls framelight needs, through the C function
//! tables (`FnTable:<interface>`) of a dlopen'd `libopenvr_api.so`.
//!
//! Slot numbers are the method order of the interface versions named below
//! (openvr.h of SDK 2.x, as shipped with FrameCast); the function tables use
//! the same order as the C++ classes.

use std::ffi::{CStr, CString, c_char, c_void};

use anyhow::{Context, Result, bail};
use libloading::Library;

const LIB_PATH: &str = "/opt/steamvr/bin/linuxarm64/libopenvr_api.so";

const APP_OVERLAY: i32 = 2;
const TEXTURE_TYPE_VULKAN: i32 = 2;
const DEVICE_TYPE_VULKAN: i32 = 1;
const EVENT_QUIT: u32 = 700;

const SYSTEM: &str = "IVRSystem_026";
const SYSTEM_GET_OUTPUT_DEVICE: usize = 9;
const SYSTEM_POLL_NEXT_EVENT: usize = 30;

const COMPOSITOR: &str = "IVRCompositor_029";
const COMPOSITOR_VK_INSTANCE_EXTS: usize = 41;
const COMPOSITOR_VK_DEVICE_EXTS: usize = 42;

const OVERLAY: &str = "IVROverlay_028";
const OVERLAY_FIND: usize = 0;

const OVERLAY_VIEW: &str = "IVROverlayView_003";
const OVERLAY_VIEW_ACQUIRE: usize = 0;
const OVERLAY_VIEW_RELEASE: usize = 1;

/// sizeof(VREvent_t) with OpenVR's `#pragma pack(4)` on Linux.
const EVENT_SIZE: usize = 60;

#[repr(C)]
pub struct VrVulkanDevice {
    pub instance: *mut c_void,
    pub device: *mut c_void,
    pub physical_device: *mut c_void,
    pub queue: *mut c_void,
    pub queue_family_index: u32,
}

#[repr(C)]
pub struct VrNativeDevice {
    pub handle: *mut c_void,
    pub device_type: i32,
}

#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct TextureBounds {
    pub u_min: f32,
    pub v_min: f32,
    pub u_max: f32,
    pub v_max: f32,
}

#[repr(C)]
pub struct Texture {
    pub handle: *mut c_void,
    pub texture_type: i32,
    pub color_space: i32,
}

#[repr(C)]
pub struct OverlayView {
    pub overlay_handle: u64,
    pub texture: Texture,
    pub bounds: TextureBounds,
}

#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub struct VulkanTextureData {
    pub image: u64,
    pub device: *mut c_void,
    pub physical_device: *mut c_void,
    pub instance: *mut c_void,
    pub queue: *mut c_void,
    pub queue_family_index: u32,
    pub width: u32,
    pub height: u32,
    pub format: u32,
    pub sample_count: u32,
}

const _: () = {
    assert!(size_of::<OverlayView>() == 40);
    assert!(size_of::<VulkanTextureData>() == 64);
};

type Table = *const *const c_void;

pub struct OpenVr {
    system: Table,
    compositor: Table,
    overlay: Table,
    overlay_view: Table,
    /// Declared last: `VR_ShutdownInternal` runs after everything else is gone.
    _session: Session,
}

/// An initialised OpenVR client; shuts it down when dropped.
struct Session {
    shutdown: unsafe extern "C" fn(),
}

impl Drop for Session {
    fn drop(&mut self) {
        // SAFETY: paired with the successful VR_InitInternal2 that created this value.
        unsafe { (self.shutdown)() };
    }
}

/// `libopenvr_api.so`, loaded once and never unloaded: every stream reconnects, and
/// unloading a C++ library that has started threads is asking for a crash (each of
/// which SteamOS reports to Valve as a minidump).
fn library() -> Result<&'static Library> {
    static LIB: std::sync::OnceLock<std::result::Result<Library, String>> =
        std::sync::OnceLock::new();
    // SAFETY: loading Valve's client library; its initialisers are benign.
    LIB.get_or_init(|| unsafe { Library::new(LIB_PATH) }.map_err(|e| e.to_string()))
        .as_ref()
        .map_err(|e| anyhow::anyhow!("dlopen {LIB_PATH}: {e}"))
}

// SAFETY: the tables are process-global and OpenVR's client calls are IPC.
unsafe impl Send for OpenVr {}

/// Is SteamVR's compositor running? Connecting as an overlay app must never
/// be what starts SteamVR.
pub fn steamvr_running() -> bool {
    let Ok(dir) = std::fs::read_dir("/proc") else {
        return false;
    };
    dir.flatten().any(|e| {
        std::fs::read_to_string(e.path().join("comm")).is_ok_and(|c| c.trim() == "vrcompositor")
    })
}

impl OpenVr {
    pub fn connect() -> Result<Self> {
        if !steamvr_running() {
            bail!("SteamVR is not running (no vrcompositor process)");
        }
        let lib = library()?;
        // SAFETY: the signatures are those of openvr_capi.h for these exports.
        unsafe {
            let init: unsafe extern "C" fn(*mut i32, i32, *const c_char) -> u32 =
                *lib.get(b"VR_InitInternal2\0").context("VR_InitInternal2")?;
            let get: unsafe extern "C" fn(*const c_char, *mut i32) -> *const c_void = *lib
                .get(b"VR_GetGenericInterface\0")
                .context("VR_GetGenericInterface")?;
            let shutdown: unsafe extern "C" fn() = *lib
                .get(b"VR_ShutdownInternal\0")
                .context("VR_ShutdownInternal")?;
            let mut err = 0;
            init(&mut err, APP_OVERLAY, std::ptr::null());
            if err != 0 {
                bail!("VR_Init failed: OpenVR init error {err}");
            }
            // Shuts down again if an interface below is missing.
            let session = Session { shutdown };
            let table = |name: &str| -> Result<Table> {
                let n = CString::new(format!("FnTable:{name}"))?;
                let mut err = 0;
                let t = get(n.as_ptr(), &mut err);
                if err != 0 || t.is_null() {
                    bail!("SteamVR does not offer {name} (error {err})");
                }
                Ok(t as Table)
            };
            Ok(Self {
                system: table(SYSTEM)?,
                compositor: table(COMPOSITOR)?,
                overlay: table(OVERLAY)?,
                overlay_view: table(OVERLAY_VIEW)?,
                _session: session,
            })
        }
    }

    unsafe fn slot<F: Copy>(table: Table, i: usize) -> F {
        // SAFETY: caller passes the right signature for slot i.
        unsafe {
            let p = *table.add(i);
            assert!(!p.is_null(), "OpenVR function table slot {i} is empty");
            std::mem::transmute_copy::<*const c_void, F>(&p)
        }
    }

    pub fn vulkan_instance_extensions(&self) -> Vec<String> {
        let f: unsafe extern "C" fn(*mut c_char, u32) -> u32 =
            unsafe { Self::slot(self.compositor, COMPOSITOR_VK_INSTANCE_EXTS) };
        let mut buf = vec![0u8; 4096];
        unsafe { f(buf.as_mut_ptr() as *mut c_char, buf.len() as u32) };
        split_list(&buf)
    }

    // The pointer is an opaque Vulkan handle handed to SteamVR, never dereferenced here.
    #[allow(clippy::not_unsafe_ptr_arg_deref)]
    pub fn vulkan_device_extensions(&self, physical_device: *mut c_void) -> Vec<String> {
        let f: unsafe extern "C" fn(*mut c_void, *mut c_char, u32) -> u32 =
            unsafe { Self::slot(self.compositor, COMPOSITOR_VK_DEVICE_EXTS) };
        let mut buf = vec![0u8; 4096];
        unsafe {
            f(
                physical_device,
                buf.as_mut_ptr() as *mut c_char,
                buf.len() as u32,
            )
        };
        split_list(&buf)
    }

    /// The VkPhysicalDevice SteamVR renders on (null if it doesn't say).
    // The pointer is an opaque Vulkan handle handed to SteamVR, never dereferenced here.
    #[allow(clippy::not_unsafe_ptr_arg_deref)]
    pub fn output_device(&self, instance: *mut c_void) -> *mut c_void {
        let f: unsafe extern "C" fn(*mut u64, i32, *mut c_void) =
            unsafe { Self::slot(self.system, SYSTEM_GET_OUTPUT_DEVICE) };
        let mut dev = 0u64;
        unsafe { f(&mut dev, TEXTURE_TYPE_VULKAN, instance) };
        dev as usize as *mut c_void
    }

    /// Drains pending events; returns true if SteamVR asked us to quit.
    pub fn poll_quit(&self) -> bool {
        let f: unsafe extern "C" fn(*mut u8, u32) -> bool =
            unsafe { Self::slot(self.system, SYSTEM_POLL_NEXT_EVENT) };
        let mut ev = [0u8; EVENT_SIZE];
        let mut quit = false;
        while unsafe { f(ev.as_mut_ptr(), EVENT_SIZE as u32) } {
            if u32::from_ne_bytes(ev[0..4].try_into().unwrap()) == EVENT_QUIT {
                quit = true;
            }
        }
        quit
    }

    pub fn find_overlay(&self, key: &str) -> Result<u64> {
        let f: unsafe extern "C" fn(*const c_char, *mut u64) -> i32 =
            unsafe { Self::slot(self.overlay, OVERLAY_FIND) };
        let k = CString::new(key)?;
        let mut h = 0u64;
        let e = unsafe { f(k.as_ptr(), &mut h) };
        if e != 0 {
            bail!("overlay {key} not found (overlay error {e})");
        }
        Ok(h)
    }

    /// # Safety
    /// `device` and `view` must stay valid; reuse the same `view` each call.
    pub unsafe fn acquire_overlay_view(
        &self,
        overlay: u64,
        device: *mut VrNativeDevice,
        view: *mut OverlayView,
    ) -> i32 {
        let f: unsafe extern "C" fn(u64, *mut VrNativeDevice, *mut OverlayView, u32) -> i32 =
            unsafe { Self::slot(self.overlay_view, OVERLAY_VIEW_ACQUIRE) };
        unsafe { f(overlay, device, view, size_of::<OverlayView>() as u32) }
    }

    /// # Safety
    /// `view` must have been passed to `acquire_overlay_view`.
    pub unsafe fn release_overlay_view(&self, view: *mut OverlayView) -> i32 {
        let f: unsafe extern "C" fn(*mut OverlayView) -> i32 =
            unsafe { Self::slot(self.overlay_view, OVERLAY_VIEW_RELEASE) };
        unsafe { f(view) }
    }
}

fn split_list(buf: &[u8]) -> Vec<String> {
    let s = CStr::from_bytes_until_nul(buf)
        .map(|c| c.to_string_lossy().into_owned())
        .unwrap_or_default();
    s.split_whitespace().map(str::to_owned).collect()
}

/// The Headset View as a [`ViewProvider`](crate::provider::ViewProvider).
pub struct OpenVrProvider {
    vr: OpenVr,
    overlay: u64,
    // OpenVR keeps pointers into these between calls: boxed, never moved.
    vk_device: Box<VrVulkanDevice>,
    native: Box<VrNativeDevice>,
    view: Box<OverlayView>,
    view_acquired: bool,
}

// SAFETY: the boxed structs are only touched through `&mut self`.
unsafe impl Send for OpenVrProvider {}

impl OpenVrProvider {
    /// Connects to the running SteamVR and finds `overlay_key`
    /// (normally [`crate::HEADSET_VIEW`]).
    pub fn connect(overlay_key: &str) -> Result<Self> {
        let vr = OpenVr::connect()?;
        let overlay = vr.find_overlay(overlay_key)?;
        Ok(Self {
            vr,
            overlay,
            vk_device: Box::new(VrVulkanDevice {
                instance: std::ptr::null_mut(),
                device: std::ptr::null_mut(),
                physical_device: std::ptr::null_mut(),
                queue: std::ptr::null_mut(),
                queue_family_index: 0,
            }),
            native: Box::new(VrNativeDevice {
                handle: std::ptr::null_mut(),
                device_type: DEVICE_TYPE_VULKAN,
            }),
            // SAFETY: plain C struct, all-zero is its "empty" state.
            view: Box::new(unsafe { std::mem::zeroed() }),
            view_acquired: false,
        })
    }
}

impl crate::provider::ViewProvider for OpenVrProvider {
    fn instance_extensions(&self) -> Vec<String> {
        self.vr.vulkan_instance_extensions()
    }

    fn physical_device(&self, instance: ash::vk::Instance) -> Option<ash::vk::PhysicalDevice> {
        use ash::vk::Handle;
        let p = self
            .vr
            .output_device(instance.as_raw() as usize as *mut c_void);
        (!p.is_null()).then(|| ash::vk::PhysicalDevice::from_raw(p as u64))
    }

    fn device_extensions(&self, pd: ash::vk::PhysicalDevice) -> Vec<String> {
        use ash::vk::Handle;
        self.vr
            .vulkan_device_extensions(pd.as_raw() as usize as *mut c_void)
    }

    fn attach(&mut self, h: &crate::provider::VkHandles<'_>) -> Result<()> {
        use ash::vk::Handle;
        *self.vk_device = VrVulkanDevice {
            instance: h.instance.handle().as_raw() as usize as *mut c_void,
            device: h.device.handle().as_raw() as usize as *mut c_void,
            physical_device: h.physical_device.as_raw() as usize as *mut c_void,
            queue: h.queue.as_raw() as usize as *mut c_void,
            queue_family_index: h.queue_family_index,
        };
        self.native.handle = &*self.vk_device as *const VrVulkanDevice as *mut c_void;
        Ok(())
    }

    fn acquire(&mut self) -> Result<Option<crate::provider::ViewImage>> {
        let err = unsafe {
            self.vr
                .acquire_overlay_view(self.overlay, &mut *self.native, &mut *self.view)
        };
        if err != 0 {
            tracing::debug!("AcquireOverlayView: overlay error {err}");
            return Ok(None);
        }
        self.view_acquired = true;
        if self.view.texture.handle.is_null() {
            return Ok(None);
        }
        // SAFETY: for a Vulkan overlay view the handle is a VRVulkanTextureData_t.
        let t = unsafe { *(self.view.texture.handle as *const VulkanTextureData) };
        Ok(Some(crate::provider::ViewImage {
            image: ash::vk::Handle::from_raw(t.image),
            format: ash::vk::Format::from_raw(t.format as i32),
            width: t.width,
            height: t.height,
            sample_count: t.sample_count,
            bounds: self.view.bounds,
        }))
    }

    fn detach(&mut self, _device: &ash::Device) {
        if self.view_acquired {
            unsafe { self.vr.release_overlay_view(&mut *self.view) };
            self.view_acquired = false;
        }
    }

    fn quit_requested(&mut self) -> bool {
        self.vr.poll_quit()
    }
}

#[cfg(test)]
mod tests {
    //! Checks the slot numbers and interface versions above against an
    //! openvr.h (SDK 2.x): `FL_OPENVR_H=/path/to/openvr.h cargo test -p fl-capture`.
    use super::*;

    /// Method order of `class <name>` (virtual methods in declaration order).
    fn slots(header: &str, class: &str) -> Vec<String> {
        let start = header
            .find(&format!("class {class}\n"))
            .unwrap_or_else(|| panic!("class {class} not in header"));
        let rest = &header[start..];
        let end = rest.find("_Version = ").unwrap();
        rest[..end]
            .lines()
            .filter(|l| l.trim_start().starts_with("virtual "))
            .map(|l| {
                let head = l.split('(').next().unwrap();
                head.split_whitespace()
                    .last()
                    .unwrap()
                    .trim_start_matches('*')
                    .to_owned()
            })
            .collect()
    }

    #[test]
    fn slots_match_openvr_h() {
        let Ok(path) = std::env::var("FL_OPENVR_H") else {
            eprintln!("FL_OPENVR_H not set, skipping");
            return;
        };
        let h = std::fs::read_to_string(path).unwrap();
        for (class, version) in [
            ("IVRSystem", SYSTEM),
            ("IVRCompositor", COMPOSITOR),
            ("IVROverlay", OVERLAY),
            ("IVROverlayView", OVERLAY_VIEW),
        ] {
            assert!(
                h.contains(&format!("{class}_Version = \"{version}\"")),
                "{class} is not {version} in this header"
            );
        }
        let sys = slots(&h, "IVRSystem");
        assert_eq!(sys[SYSTEM_GET_OUTPUT_DEVICE], "GetOutputDevice");
        assert_eq!(sys[SYSTEM_POLL_NEXT_EVENT], "PollNextEvent");
        let c = slots(&h, "IVRCompositor");
        assert_eq!(
            c[COMPOSITOR_VK_INSTANCE_EXTS],
            "GetVulkanInstanceExtensionsRequired"
        );
        assert_eq!(
            c[COMPOSITOR_VK_DEVICE_EXTS],
            "GetVulkanDeviceExtensionsRequired"
        );
        assert_eq!(slots(&h, "IVROverlay")[OVERLAY_FIND], "FindOverlay");
        let v = slots(&h, "IVROverlayView");
        assert_eq!(v[OVERLAY_VIEW_ACQUIRE], "AcquireOverlayView");
        assert_eq!(v[OVERLAY_VIEW_RELEASE], "ReleaseOverlayView");
    }
}
