// SPDX-FileCopyrightText: 2026 Bryant Frankford <nathanielfrankford@gmail.com> and Contributors
// SPDX-License-Identifier: GPL-3.0-or-later OR LGPL-3.0-or-later
//! gfxstream, the renderer behind an accelerated GPU: the guest's graphics
//! commands are sent across to it and run on the host's GPU.
//!
//! gfxstream is a separate library (Apache-2.0, from the Android Open
//! Source Project), loaded at run time from a path the embedder gives. bvmm
//! builds and runs without it; only an accelerated display needs it.
//!
//! This file is the raw interface: the functions of gfxstream's
//! `virtio-gpu-gfxstream-renderer.h` (version 0.1.2), with the same names
//! less the `stream_renderer_` prefix. What the calls mean to a guest is
//! the display device's business.
//!
//! The library keeps one renderer per process, so there can be one
//! [`Gfxstream`] at a time.

use std::ffi::{c_char, c_int, c_void};
use std::path::Path;

use crate::{Error, Result};

/// Renderer flags (`enum RendererFlags`).
pub const USE_SURFACELESS: u64 = 1 << 3;
pub const USE_VULKAN: u64 = 1 << 5;

const PARAM_USER_DATA: u64 = 1;
const PARAM_RENDERER_FLAGS: u64 = 2;
const PARAM_FENCE_CALLBACK: u64 = 3;
const PARAM_WIN0_WIDTH: u64 = 4;
const PARAM_WIN0_HEIGHT: u64 = 5;
const PARAM_DEBUG_CALLBACK: u64 = 6;

/// `STREAM_RENDERER_FLAG_FENCE_RING_IDX`: the fence belongs to one ring of
/// one context, not to the device as a whole.
pub const FENCE_RING_IDX: u32 = 1 << 1;

#[repr(C)]
struct Param {
    key: u64,
    value: u64,
}

/// `struct stream_renderer_fence`.
#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Fence {
    pub flags: u32,
    pub fence_id: u64,
    pub ctx_id: u32,
    pub ring_idx: u8,
}

/// `struct stream_renderer_resource_create_args`.
#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
pub struct ResourceCreate {
    pub handle: u32,
    pub target: u32,
    pub format: u32,
    pub bind: u32,
    pub width: u32,
    pub height: u32,
    pub depth: u32,
    pub array_size: u32,
    pub last_level: u32,
    pub nr_samples: u32,
    pub flags: u32,
}

/// `struct stream_renderer_box`.
#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
pub struct Box3 {
    pub x: u32,
    pub y: u32,
    pub z: u32,
    pub w: u32,
    pub h: u32,
    pub d: u32,
}

/// What to copy between a resource's guest memory and the renderer.
#[derive(Debug, Clone, Copy, Default)]
pub struct Transfer {
    pub resource: u32,
    pub ctx: u32,
    pub level: u32,
    pub stride: u32,
    pub layer_stride: u32,
    pub area: Box3,
    pub offset: u64,
}

/// `struct iovec`: one stretch of host memory.
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct IoVec {
    pub base: *mut c_void,
    pub len: usize,
}

/// `struct stream_renderer_create_blob`.
#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
pub struct CreateBlob {
    pub blob_mem: u32,
    pub blob_flags: u32,
    pub blob_id: u64,
    pub size: u64,
}

#[repr(C)]
struct Command {
    ctx_id: u32,
    cmd_size: u32,
    cmd: *mut u8,
    num_in_fences: u32,
    fences: *mut c_void,
}

#[repr(C)]
struct Debug {
    debug_type: u32,
    message: *const c_char,
}

/// Called by the renderer, on one of its threads, when the work a fence
/// was put behind is done.
pub type FenceHandler = Box<dyn Fn(Fence) + Send + Sync>;

macro_rules! api {
    ($( $field:ident = $symbol:literal : fn( $($arg:ty),* ) $(-> $ret:ty)? ; )*) => {
        struct Api {
            $( $field: unsafe extern "C" fn($($arg),*) $(-> $ret)?, )*
        }
        impl Api {
            fn load(lib: &Library) -> Result<Api> {
                Ok(Api {
                    $( $field: {
                        let p = lib.symbol(concat!($symbol, "\0"))?;
                        // SAFETY: the symbol's signature is the one declared
                        // here, from gfxstream's public header.
                        unsafe { std::mem::transmute::<*mut c_void, unsafe extern "C" fn($($arg),*) $(-> $ret)?>(p) }
                    }, )*
                })
            }
        }
    };
}

api! {
    init = "stream_renderer_init": fn(*mut Param, u64) -> c_int;
    teardown = "stream_renderer_teardown": fn();
    resource_create = "stream_renderer_resource_create": fn(*mut ResourceCreate, *mut IoVec, u32) -> c_int;
    resource_unref = "stream_renderer_resource_unref": fn(u32);
    context_create = "stream_renderer_context_create": fn(u32, u32, *const c_char, u32) -> c_int;
    context_destroy = "stream_renderer_context_destroy": fn(u32);
    submit_cmd = "stream_renderer_submit_cmd": fn(*mut Command) -> c_int;
    transfer_read_iov = "stream_renderer_transfer_read_iov": fn(u32, u32, u32, u32, u32, *mut Box3, u64, *mut IoVec, c_int) -> c_int;
    transfer_write_iov = "stream_renderer_transfer_write_iov": fn(u32, u32, c_int, u32, u32, *mut Box3, u64, *mut IoVec, u32) -> c_int;
    get_cap_set = "stream_renderer_get_cap_set": fn(u32, *mut u32, *mut u32);
    fill_caps = "stream_renderer_fill_caps": fn(u32, u32, *mut c_void);
    resource_attach_iov = "stream_renderer_resource_attach_iov": fn(c_int, *mut IoVec, c_int) -> c_int;
    resource_detach_iov = "stream_renderer_resource_detach_iov": fn(c_int, *mut *mut IoVec, *mut c_int);
    ctx_attach_resource = "stream_renderer_ctx_attach_resource": fn(c_int, c_int);
    ctx_detach_resource = "stream_renderer_ctx_detach_resource": fn(c_int, c_int);
    create_blob = "stream_renderer_create_blob": fn(u32, u32, *const CreateBlob, *const IoVec, u32, *const c_void) -> c_int;
    resource_map = "stream_renderer_resource_map": fn(u32, *mut *mut c_void, *mut u64) -> c_int;
    resource_unmap = "stream_renderer_resource_unmap": fn(u32) -> c_int;
    resource_map_info = "stream_renderer_resource_map_info": fn(u32, *mut u32) -> c_int;
    create_fence = "stream_renderer_create_fence": fn(*const Fence) -> c_int;
    flush = "stream_renderer_flush": fn(u32);
}

/// A running renderer.
pub struct Gfxstream {
    api: Api,
    _lib: Library,
}

// SAFETY: gfxstream's entry points take their own locks; the handle holds
// only function pointers.
unsafe impl Send for Gfxstream {}
unsafe impl Sync for Gfxstream {}

fn status(code: c_int, what: &str) -> Result<()> {
    if code == 0 {
        Ok(())
    } else {
        Err(Error::new(format!("gfxstream: {what} failed ({code})")))
    }
}

unsafe extern "C" fn on_fence(user: *mut c_void, fence: *mut Fence) {
    // SAFETY: `user` is the handler leaked in `start`; `fence` is valid for
    // the call.
    let handler = unsafe { &*(user as *const FenceHandler) };
    handler(unsafe { *fence });
}

unsafe extern "C" fn on_debug(_user: *mut c_void, debug: *mut Debug) {
    // SAFETY: valid for the call; the message is a C string or null.
    let d = unsafe { &*debug };
    // 1: error, 2: warning. The rest is chatter.
    if d.debug_type <= 2 && !d.message.is_null() {
        let text = unsafe { std::ffi::CStr::from_ptr(d.message) }.to_string_lossy();
        eprintln!("gfxstream: {}", text.trim_end());
    }
}

impl Gfxstream {
    /// Load the library at `path` and start a renderer for a screen of the
    /// given size, drawing with Vulkan and with no window of its own.
    ///
    /// A start that cannot work (no Vulkan driver, say) has been seen to
    /// hang inside the library instead of returning; callers that cannot
    /// afford that should try it in a process they can abandon first.
    pub fn start(path: &Path, width: u32, height: u32, fence: FenceHandler) -> Result<Gfxstream> {
        let lib = Library::open(path)?;
        let api = Api::load(&lib)?;
        // Lives as long as the process: the renderer may call it from its
        // threads at any time until teardown, and after a failed start too.
        let handler: *mut FenceHandler = Box::into_raw(Box::new(fence));
        let mut params = [
            Param {
                key: PARAM_USER_DATA,
                value: handler as u64,
            },
            Param {
                key: PARAM_RENDERER_FLAGS,
                value: USE_VULKAN | USE_SURFACELESS,
            },
            Param {
                key: PARAM_FENCE_CALLBACK,
                value: on_fence as *const () as usize as u64,
            },
            Param {
                key: PARAM_WIN0_WIDTH,
                value: width as u64,
            },
            Param {
                key: PARAM_WIN0_HEIGHT,
                value: height as u64,
            },
            Param {
                key: PARAM_DEBUG_CALLBACK,
                value: on_debug as *const () as usize as u64,
            },
        ];
        // SAFETY: the array outlives the call; the callbacks are `extern "C"`
        // functions with the signatures the header gives.
        let code = unsafe { (api.init)(params.as_mut_ptr(), params.len() as u64) };
        status(code, "starting the renderer")?;
        Ok(Gfxstream { api, _lib: lib })
    }

    /// The newest version and the size of capability set `id`; a size of
    /// zero means the set is not offered.
    pub fn capset_info(&self, id: u32) -> (u32, u32) {
        let (mut version, mut size) = (0u32, 0u32);
        // SAFETY: two valid out-pointers.
        unsafe { (self.api.get_cap_set)(id, &mut version, &mut size) };
        (version, size)
    }

    /// The contents of capability set `id` at `version`.
    pub fn capset(&self, id: u32, version: u32) -> Vec<u8> {
        let (_, size) = self.capset_info(id);
        let mut caps = vec![0u8; size as usize];
        if size > 0 {
            // SAFETY: the buffer has the size the library reported.
            unsafe { (self.api.fill_caps)(id, version, caps.as_mut_ptr() as *mut c_void) };
        }
        caps
    }

    pub fn context_create(&self, ctx: u32, name: &str, context_init: u32) -> Result<()> {
        let name = std::ffi::CString::new(name.replace('\0', "")).unwrap_or_default();
        // SAFETY: a valid C string and its length.
        let code = unsafe {
            (self.api.context_create)(
                ctx,
                name.as_bytes().len() as u32,
                name.as_ptr(),
                context_init,
            )
        };
        status(code, "creating a context")
    }

    pub fn context_destroy(&self, ctx: u32) {
        // SAFETY: plain values.
        unsafe { (self.api.context_destroy)(ctx) }
    }

    /// Run a buffer of the guest's commands in context `ctx`.
    pub fn submit(&self, ctx: u32, commands: &mut [u8]) -> Result<()> {
        let mut command = Command {
            ctx_id: ctx,
            cmd_size: commands.len() as u32,
            cmd: commands.as_mut_ptr(),
            num_in_fences: 0,
            fences: std::ptr::null_mut(),
        };
        // SAFETY: the buffer outlives the call.
        status(
            unsafe { (self.api.submit_cmd)(&mut command) },
            "submitting commands",
        )
    }

    /// Ask to be told, through the fence handler, when the work queued so
    /// far (on the fence's ring) is done.
    pub fn create_fence(&self, fence: &Fence) -> Result<()> {
        // SAFETY: a valid struct for the call.
        status(
            unsafe { (self.api.create_fence)(fence) },
            "creating a fence",
        )
    }

    /// # Safety
    /// The memory `iov` names must stay valid while the resource has it.
    pub unsafe fn resource_create(&self, args: &ResourceCreate, iov: &mut [IoVec]) -> Result<()> {
        let mut args = *args;
        let code =
            unsafe { (self.api.resource_create)(&mut args, iov.as_mut_ptr(), iov.len() as u32) };
        status(code, "creating a resource")
    }

    pub fn resource_unref(&self, resource: u32) {
        // SAFETY: plain values.
        unsafe { (self.api.resource_unref)(resource) }
    }

    /// # Safety
    /// The memory `iov` names must stay valid until it is detached.
    pub unsafe fn resource_attach(&self, resource: u32, iov: &mut [IoVec]) -> Result<()> {
        let code = unsafe {
            (self.api.resource_attach_iov)(resource as c_int, iov.as_mut_ptr(), iov.len() as c_int)
        };
        status(code, "attaching memory to a resource")
    }

    pub fn resource_detach(&self, resource: u32) {
        // SAFETY: null out-pointers are accepted: the caller owns the iovecs.
        unsafe {
            (self.api.resource_detach_iov)(
                resource as c_int,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
            )
        }
    }

    pub fn ctx_attach_resource(&self, ctx: u32, resource: u32) {
        // SAFETY: plain values.
        unsafe { (self.api.ctx_attach_resource)(ctx as c_int, resource as c_int) }
    }

    pub fn ctx_detach_resource(&self, ctx: u32, resource: u32) {
        // SAFETY: plain values.
        unsafe { (self.api.ctx_detach_resource)(ctx as c_int, resource as c_int) }
    }

    /// Copy part of a resource from the guest's memory to the renderer.
    pub fn transfer_write(&self, t: &Transfer) -> Result<()> {
        let mut area = t.area;
        // SAFETY: no iovecs: the resource's attached memory is used.
        let code = unsafe {
            (self.api.transfer_write_iov)(
                t.resource,
                t.ctx,
                t.level as c_int,
                t.stride,
                t.layer_stride,
                &mut area,
                t.offset,
                std::ptr::null_mut(),
                0,
            )
        };
        status(code, "transferring to the host")
    }

    /// Copy part of a resource from the renderer into `iov` (or, with no
    /// iovecs, into the resource's attached guest memory).
    ///
    /// # Safety
    /// The memory `iov` names must be valid for the call.
    pub unsafe fn transfer_read(&self, t: &Transfer, iov: &mut [IoVec]) -> Result<()> {
        let mut area = t.area;
        let ptr = if iov.is_empty() {
            std::ptr::null_mut()
        } else {
            iov.as_mut_ptr()
        };
        let code = unsafe {
            (self.api.transfer_read_iov)(
                t.resource,
                t.ctx,
                t.level,
                t.stride,
                t.layer_stride,
                &mut area,
                t.offset,
                ptr,
                iov.len() as c_int,
            )
        };
        status(code, "transferring from the host")
    }

    /// # Safety
    /// The memory `iov` names must stay valid while the resource exists.
    pub unsafe fn create_blob(
        &self,
        ctx: u32,
        resource: u32,
        blob: &CreateBlob,
        iov: &[IoVec],
    ) -> Result<()> {
        let ptr = if iov.is_empty() {
            std::ptr::null()
        } else {
            iov.as_ptr()
        };
        let code = unsafe {
            (self.api.create_blob)(ctx, resource, blob, ptr, iov.len() as u32, std::ptr::null())
        };
        status(code, "creating a blob resource")
    }

    /// The host memory behind a blob resource: its address and size. It
    /// stays put until [`Gfxstream::resource_unmap`].
    pub fn resource_map(&self, resource: u32) -> Result<(*mut u8, u64)> {
        let (mut at, mut size) = (std::ptr::null_mut::<c_void>(), 0u64);
        // SAFETY: two valid out-pointers.
        status(
            unsafe { (self.api.resource_map)(resource, &mut at, &mut size) },
            "mapping a resource",
        )?;
        if at.is_null() {
            return Err(Error::new("gfxstream: a mapped resource has no address"));
        }
        Ok((at as *mut u8, size))
    }

    pub fn resource_unmap(&self, resource: u32) -> Result<()> {
        // SAFETY: plain values.
        status(
            unsafe { (self.api.resource_unmap)(resource) },
            "unmapping a resource",
        )
    }

    /// How the guest should cache a mapped resource (`STREAM_RENDERER_MAP_CACHE_*`).
    pub fn resource_map_info(&self, resource: u32) -> Result<u32> {
        let mut info = 0u32;
        // SAFETY: one valid out-pointer.
        status(
            unsafe { (self.api.resource_map_info)(resource, &mut info) },
            "asking how to map a resource",
        )?;
        Ok(info)
    }

    /// The guest has finished drawing into `resource`.
    pub fn flush(&self, resource: u32) {
        // SAFETY: plain values.
        unsafe { (self.api.flush)(resource) }
    }
}

impl Drop for Gfxstream {
    fn drop(&mut self) {
        // SAFETY: the renderer was started by `start`.
        unsafe { (self.api.teardown)() }
    }
}

// --- loading a library ------------------------------------------------------

struct Library(*mut c_void);

#[cfg(unix)]
impl Library {
    fn open(path: &Path) -> Result<Library> {
        use std::os::unix::ffi::OsStrExt;
        let c = std::ffi::CString::new(path.as_os_str().as_bytes())
            .map_err(|_| Error::new("the library path contains a NUL byte"))?;
        // SAFETY: a valid C string. RTLD_NOW: fail here, not at first use.
        let handle = unsafe { libc::dlopen(c.as_ptr(), libc::RTLD_NOW | libc::RTLD_LOCAL) };
        if handle.is_null() {
            // SAFETY: dlerror returns a C string or null.
            let why = unsafe {
                let e = libc::dlerror();
                if e.is_null() {
                    "unknown error".into()
                } else {
                    std::ffi::CStr::from_ptr(e).to_string_lossy().into_owned()
                }
            };
            return Err(Error::new(format!("loading {}: {why}", path.display())));
        }
        Ok(Library(handle))
    }

    fn symbol(&self, name: &str) -> Result<*mut c_void> {
        // SAFETY: `name` ends with NUL (the callers pass literals that do).
        let p = unsafe { libc::dlsym(self.0, name.as_ptr() as *const c_char) };
        if p.is_null() {
            return Err(Error::new(format!(
                "the gfxstream library has no {}: it is not the version bvmm was written for",
                name.trim_end_matches('\0')
            )));
        }
        Ok(p)
    }
}

#[cfg(windows)]
#[link(name = "kernel32")]
extern "system" {
    fn LoadLibraryW(name: *const u16) -> *mut c_void;
    fn GetProcAddress(module: *mut c_void, name: *const u8) -> *mut c_void;
    fn GetLastError() -> u32;
}

#[cfg(windows)]
impl Library {
    fn open(path: &Path) -> Result<Library> {
        use std::os::windows::ffi::OsStrExt;
        let wide: Vec<u16> = path.as_os_str().encode_wide().chain([0]).collect();
        // SAFETY: a NUL-terminated wide string.
        let handle = unsafe { LoadLibraryW(wide.as_ptr()) };
        if handle.is_null() {
            // SAFETY: no arguments.
            let code = unsafe { GetLastError() };
            return Err(Error::new(format!(
                "loading {}: Windows error {code}",
                path.display()
            )));
        }
        Ok(Library(handle))
    }

    fn symbol(&self, name: &str) -> Result<*mut c_void> {
        // SAFETY: `name` ends with NUL (the callers pass literals that do).
        let p = unsafe { GetProcAddress(self.0, name.as_ptr()) };
        if p.is_null() {
            return Err(Error::new(format!(
                "the gfxstream library has no {}: it is not the version bvmm was written for",
                name.trim_end_matches('\0')
            )));
        }
        Ok(p)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_structures_have_the_layout_of_the_c_header() {
        use std::mem::{offset_of, size_of};
        // struct stream_renderer_fence { u32 flags; u64 fence_id; u32 ctx_id; u8 ring_idx; }
        assert_eq!(
            (
                offset_of!(Fence, fence_id),
                offset_of!(Fence, ctx_id),
                offset_of!(Fence, ring_idx)
            ),
            (8, 16, 20)
        );
        assert_eq!(size_of::<Fence>(), 24);
        assert_eq!(size_of::<ResourceCreate>(), 44);
        assert_eq!(size_of::<Box3>(), 24);
        assert_eq!(size_of::<CreateBlob>(), 24);
        assert_eq!(
            (
                offset_of!(CreateBlob, blob_id),
                offset_of!(CreateBlob, size)
            ),
            (8, 16)
        );
        assert_eq!(
            (
                offset_of!(Command, cmd),
                offset_of!(Command, num_in_fences),
                offset_of!(Command, fences)
            ),
            (8, 16, 24)
        );
        assert_eq!(size_of::<Param>(), 16);
        assert_eq!(size_of::<IoVec>(), 2 * size_of::<usize>());
    }

    #[test]
    fn a_missing_library_is_an_error_not_a_crash() {
        let e = Gfxstream::start(
            Path::new("/nonexistent/libgfxstream_backend.so"),
            720,
            1280,
            Box::new(|_| {}),
        )
        .err()
        .expect("no such library");
        assert!(e.to_string().contains("loading"), "{e}");
    }
}
