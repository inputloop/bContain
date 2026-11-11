// SPDX-FileCopyrightText: 2026 Bryant Frankford <nathanielfrankford@gmail.com> and Contributors
// SPDX-License-Identifier: GPL-3.0-or-later OR LGPL-3.0-or-later
//! virtio-gpu with a renderer behind it: the guest's graphics commands are
//! handed to a [`Renderer`] (gfxstream) that runs them on the host's GPU.
//!
//! The guest side is the virtio-gpu 3D protocol (Virtio 1.2, section 5.7,
//! and Linux's `virtio_gpu.h`). This device carries commands and memory;
//! what the command streams mean is between the guest's drivers and the
//! renderer. Three things are this device's own work:
//!
//! - **Fences.** A command may ask to be answered only when the renderer
//!   has caught up. Its answer is written at once but handed back when the
//!   renderer says so, possibly from another thread.
//! - **Blobs.** Host memory the renderer owns (the command ring, Vulkan
//!   memory) is placed in the guest's address space, inside the device's
//!   shared-memory window.
//! - **The picture.** The guest's finished frames live on the host; on each
//!   flush the frame is read back for the [`Display`].
//!
//! Written from the specification in gameDroid's docs/engine/E8-device-spec.md.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, OnceLock};

use super::gpu::{u32_at, u64_at, Display, Format};
use super::queue::Queue;
use super::{VirtioDevice, TYPE_GPU};
use crate::gfxstream::{Box3, CreateBlob, Fence, IoVec, ResourceCreate, Transfer, FENCE_RING_IDX};
use crate::memory::{GuestMemory, MapperSlot};
use crate::Result;

const F_VIRGL: u64 = 1 << 0;
const F_RESOURCE_BLOB: u64 = 1 << 3;
const F_CONTEXT_INIT: u64 = 1 << 4;

const CMD_GET_DISPLAY_INFO: u32 = 0x0100;
const CMD_RESOURCE_CREATE_2D: u32 = 0x0101;
const CMD_RESOURCE_UNREF: u32 = 0x0102;
const CMD_SET_SCANOUT: u32 = 0x0103;
const CMD_RESOURCE_FLUSH: u32 = 0x0104;
const CMD_TRANSFER_TO_HOST_2D: u32 = 0x0105;
const CMD_RESOURCE_ATTACH_BACKING: u32 = 0x0106;
const CMD_RESOURCE_DETACH_BACKING: u32 = 0x0107;
const CMD_GET_CAPSET_INFO: u32 = 0x0108;
const CMD_GET_CAPSET: u32 = 0x0109;
const CMD_RESOURCE_ASSIGN_UUID: u32 = 0x010b;
const CMD_RESOURCE_CREATE_BLOB: u32 = 0x010c;
const CMD_SET_SCANOUT_BLOB: u32 = 0x010d;
const CMD_CTX_CREATE: u32 = 0x0200;
const CMD_CTX_DESTROY: u32 = 0x0201;
const CMD_CTX_ATTACH_RESOURCE: u32 = 0x0202;
const CMD_CTX_DETACH_RESOURCE: u32 = 0x0203;
const CMD_RESOURCE_CREATE_3D: u32 = 0x0204;
const CMD_TRANSFER_TO_HOST_3D: u32 = 0x0205;
const CMD_TRANSFER_FROM_HOST_3D: u32 = 0x0206;
const CMD_SUBMIT_3D: u32 = 0x0207;
const CMD_RESOURCE_MAP_BLOB: u32 = 0x0208;
const CMD_RESOURCE_UNMAP_BLOB: u32 = 0x0209;

const RESP_OK_NODATA: u32 = 0x1100;
const RESP_OK_DISPLAY_INFO: u32 = 0x1101;
const RESP_OK_CAPSET_INFO: u32 = 0x1102;
const RESP_OK_CAPSET: u32 = 0x1103;
const RESP_OK_RESOURCE_UUID: u32 = 0x1105;
const RESP_OK_MAP_INFO: u32 = 0x1106;
const RESP_ERR_UNSPEC: u32 = 0x1200;
const RESP_ERR_INVALID_RESOURCE_ID: u32 = 0x1203;
const RESP_ERR_INVALID_PARAMETER: u32 = 0x1205;

const FLAG_FENCE: u32 = 1 << 0;
const FLAG_INFO_RING_IDX: u32 = 1 << 1;
const HDR: usize = 24;
const MAX_SCANOUTS: usize = 16;
const MAX_BACKING_ENTRIES: u32 = 16384;
/// The largest request taken: a command buffer of up to 16 MiB.
const MAX_REQUEST: usize = 16 << 20;
const BLOB_MEM_GUEST: u32 = 1;
const PAGE: u64 = 4096;

/// The shared-memory window: id 1 ("host visible"), 8 GiB of guest
/// address space. Only what is mapped into it uses memory.
const SHM_ID: u8 = 1;
const SHM_SIZE: u64 = 8 << 30;

/// Capability sets offered, in index order: gfxstream's Vulkan, and its
/// composer.
const CAPSETS: [u32; 2] = [3, 9];

/// What the device needs from a renderer. [`crate::gfxstream::Gfxstream`]
/// is the real one.
pub trait Renderer: Send + Sync {
    fn capset_info(&self, id: u32) -> (u32, u32);
    fn capset(&self, id: u32, version: u32) -> Vec<u8>;
    fn context_create(&self, ctx: u32, name: &str, context_init: u32) -> Result<()>;
    fn context_destroy(&self, ctx: u32);
    fn submit(&self, ctx: u32, commands: &mut [u8]) -> Result<()>;
    fn create_fence(&self, fence: &Fence) -> Result<()>;
    fn resource_create(&self, args: &ResourceCreate) -> Result<()>;
    fn resource_unref(&self, resource: u32);
    /// # Safety
    /// The memory must stay valid until `resource_detach`.
    unsafe fn resource_attach(&self, resource: u32, iov: &mut [IoVec]) -> Result<()>;
    fn resource_detach(&self, resource: u32);
    fn ctx_attach_resource(&self, ctx: u32, resource: u32);
    fn ctx_detach_resource(&self, ctx: u32, resource: u32);
    fn transfer_write(&self, t: &Transfer) -> Result<()>;
    fn transfer_read(&self, t: &Transfer) -> Result<()>;
    /// Read a resource's pixels into `out`.
    fn read_pixels(&self, t: &Transfer, out: &mut [u8]) -> Result<()>;
    fn create_blob(&self, ctx: u32, resource: u32, blob: &CreateBlob) -> Result<()>;
    fn resource_map(&self, resource: u32) -> Result<(*mut u8, u64)>;
    fn resource_unmap(&self, resource: u32) -> Result<()>;
    fn resource_map_info(&self, resource: u32) -> Result<u32>;
}

impl Renderer for crate::gfxstream::Gfxstream {
    fn capset_info(&self, id: u32) -> (u32, u32) {
        self.capset_info(id)
    }
    fn capset(&self, id: u32, version: u32) -> Vec<u8> {
        self.capset(id, version)
    }
    fn context_create(&self, ctx: u32, name: &str, context_init: u32) -> Result<()> {
        self.context_create(ctx, name, context_init)
    }
    fn context_destroy(&self, ctx: u32) {
        self.context_destroy(ctx)
    }
    fn submit(&self, ctx: u32, commands: &mut [u8]) -> Result<()> {
        self.submit(ctx, commands)
    }
    fn create_fence(&self, fence: &Fence) -> Result<()> {
        self.create_fence(fence)
    }
    fn resource_create(&self, args: &ResourceCreate) -> Result<()> {
        // SAFETY: no memory is handed over.
        unsafe { self.resource_create(args, &mut []) }
    }
    fn resource_unref(&self, resource: u32) {
        self.resource_unref(resource)
    }
    unsafe fn resource_attach(&self, resource: u32, iov: &mut [IoVec]) -> Result<()> {
        unsafe { self.resource_attach(resource, iov) }
    }
    fn resource_detach(&self, resource: u32) {
        self.resource_detach(resource)
    }
    fn ctx_attach_resource(&self, ctx: u32, resource: u32) {
        self.ctx_attach_resource(ctx, resource)
    }
    fn ctx_detach_resource(&self, ctx: u32, resource: u32) {
        self.ctx_detach_resource(ctx, resource)
    }
    fn transfer_write(&self, t: &Transfer) -> Result<()> {
        self.transfer_write(t)
    }
    fn transfer_read(&self, t: &Transfer) -> Result<()> {
        // SAFETY: no iovecs: the resource's attached memory is used.
        unsafe { self.transfer_read(t, &mut []) }
    }
    fn read_pixels(&self, t: &Transfer, out: &mut [u8]) -> Result<()> {
        let mut iov = [IoVec {
            base: out.as_mut_ptr() as *mut std::ffi::c_void,
            len: out.len(),
        }];
        // SAFETY: the one iovec is `out`, valid for the call.
        unsafe { self.transfer_read(t, &mut iov) }
    }
    fn create_blob(&self, ctx: u32, resource: u32, blob: &CreateBlob) -> Result<()> {
        // SAFETY: no memory is handed over; gfxstream ignores iovecs here.
        unsafe { self.create_blob(ctx, resource, blob, &[]) }
    }
    fn resource_map(&self, resource: u32) -> Result<(*mut u8, u64)> {
        self.resource_map(resource)
    }
    fn resource_unmap(&self, resource: u32) -> Result<()> {
        self.resource_unmap(resource)
    }
    fn resource_map_info(&self, resource: u32) -> Result<u32> {
        self.resource_map_info(resource)
    }
}

/// A fence's timeline: the whole device, or one ring of one context.
type Ring = Option<(u32, u8)>;

fn ring_of(fence: &Fence) -> Ring {
    (fence.flags & FENCE_RING_IDX != 0).then_some((fence.ctx_id, fence.ring_idx))
}

/// What the renderer's threads and the device share.
#[derive(Default)]
pub struct Fences {
    /// The newest fence the renderer has finished, per timeline.
    completed: Mutex<HashMap<Ring, u64>>,
    /// Asks the machine to call the device's `poll`.
    wake: OnceLock<Box<dyn Fn() + Send + Sync>>,
    /// True while the device is inside a guest request: it will look at
    /// `completed` itself before returning, and waking it would deadlock.
    busy: std::sync::atomic::AtomicBool,
}

impl Fences {
    /// Have `wake` called (to get the device's `poll` run) when a fence
    /// finishes while the device is idle.
    pub(crate) fn set_wake(&self, wake: Box<dyn Fn() + Send + Sync>) {
        let _ = self.wake.set(wake);
    }

    /// The renderer's fence handler: note the fence done, and have the
    /// device hand back the answers that waited on it.
    pub fn completed(&self, fence: Fence) {
        {
            let mut done = self.completed.lock().unwrap();
            let newest = done.entry(ring_of(&fence)).or_insert(0);
            *newest = (*newest).max(fence.fence_id);
        }
        if !self.busy.load(std::sync::atomic::Ordering::SeqCst) {
            if let Some(wake) = self.wake.get() {
                wake();
            }
        }
    }
}

/// An answer written into the guest's buffer but not yet handed back.
struct Held {
    ring: Ring,
    fence_id: u64,
    head: u16,
    written: u32,
}

enum Resource {
    /// A texture or buffer with a size and format.
    Image {
        width: u32,
        height: u32,
        format: u32,
        attached: bool,
    },
    /// A blob: bytes without a shape. `mapped` is where in the guest.
    Blob {
        renderer_has_it: bool,
        mapped: Option<(u64, u64)>,
    },
}

pub struct Gpu3d {
    display: Display,
    renderer: Arc<dyn Renderer>,
    fences: Arc<Fences>,
    mapper: MapperSlot,
    shm_base: u64,
    resources: HashMap<u32, Resource>,
    /// Resource shown on scanout 0, and the part of it that shows.
    scanout: Option<(u32, (u32, u32, u32, u32))>,
    held: Vec<Held>,
}

impl Gpu3d {
    /// A display device that draws with `renderer`. Give the renderer
    /// `fences` (see [`Gpu3d::fences`]) to report finished fences to.
    pub fn new(
        display: Display,
        renderer: Arc<dyn Renderer>,
        fences: Arc<Fences>,
        mapper: MapperSlot,
    ) -> Gpu3d {
        Gpu3d {
            display,
            renderer,
            fences,
            mapper,
            shm_base: 0,
            resources: HashMap::new(),
            scanout: None,
            held: Vec::new(),
        }
    }

    /// The host addresses of a list of guest (address, length) pieces.
    fn iovecs(req: &[u8], at: usize, count: u32, mem: &GuestMemory) -> Option<Vec<IoVec>> {
        if count > MAX_BACKING_ENTRIES {
            return None;
        }
        let mut out = Vec::with_capacity(count as usize);
        for i in 0..count as usize {
            let addr = u64_at(req, at + i * 16)?;
            let len = u32_at(req, at + i * 16 + 8)? as usize;
            let ptr = mem.host_ptr(addr, len).ok()?;
            out.push(IoVec {
                base: ptr as *mut std::ffi::c_void,
                len,
            });
        }
        Some(out)
    }

    fn image(&self, id: u32) -> Option<(u32, u32, u32, bool)> {
        match self.resources.get(&id)? {
            Resource::Image {
                width,
                height,
                format,
                attached,
            } => Some((*width, *height, *format, *attached)),
            Resource::Blob { .. } => None,
        }
    }

    /// Read the scanout's picture back from the renderer and show it.
    fn present(&mut self, flushed: u32) {
        let Some((id, (x, y, w, h))) = self.scanout else {
            return;
        };
        if id != flushed {
            return;
        }
        let Some((width, height, format, attached)) = self.image(id) else {
            return;
        };
        // The renderer can only read back a resource that has had memory
        // attached; before that there is nothing to show.
        let Some(format) = Format::from_wire(format).filter(|_| attached) else {
            return;
        };
        let mut pixels = vec![0u8; width as usize * height as usize * 4];
        let t = Transfer {
            resource: id,
            stride: width * 4,
            area: Box3 {
                w: width,
                h: height,
                d: 1,
                ..Default::default()
            },
            ..Default::default()
        };
        if self.renderer.read_pixels(&t, &mut pixels).is_err() {
            return;
        }
        // Only the scanout's rectangle of a larger buffer is the screen.
        if (x, y, w, h) != (0, 0, width, height) {
            let mut cut = Vec::with_capacity(w as usize * h as usize * 4);
            for row in y..y + h {
                let start = (row as usize * width as usize + x as usize) * 4;
                cut.extend_from_slice(&pixels[start..start + w as usize * 4]);
            }
            self.display.present(w, h, format, cut);
        } else {
            self.display.present(width, height, format, pixels);
        }
    }

    fn unmap(&mut self, id: u32) -> bool {
        let Some(Resource::Blob { mapped, .. }) = self.resources.get_mut(&id) else {
            return false;
        };
        let Some((gpa, len)) = mapped.take() else {
            return false;
        };
        if let Some(mapper) = self.mapper.get() {
            let _ = mapper.unmap(gpa, len);
        }
        let _ = self.renderer.resource_unmap(id);
        true
    }

    /// Carry out one control request. Returns the response type and body.
    fn control(&mut self, req: &mut [u8], mem: &GuestMemory) -> (u32, Vec<u8>) {
        let ok = (RESP_OK_NODATA, Vec::new());
        let invalid = (RESP_ERR_INVALID_PARAMETER, Vec::new());
        let no_resource = (RESP_ERR_INVALID_RESOURCE_ID, Vec::new());
        let failed = (RESP_ERR_UNSPEC, Vec::new());
        let (Some(kind), Some(ctx)) = (u32_at(req, 0), u32_at(req, 16)) else {
            return invalid;
        };
        // Every command's fixed fields, read as a row of u32s after the header.
        let word = |i: usize| u32_at(req, HDR + i * 4);
        match kind {
            CMD_GET_DISPLAY_INFO => {
                let mut out = vec![0u8; MAX_SCANOUTS * 24];
                let (w, h) = self.display.size();
                out[8..12].copy_from_slice(&w.to_le_bytes());
                out[12..16].copy_from_slice(&h.to_le_bytes());
                out[16..20].copy_from_slice(&1u32.to_le_bytes());
                (RESP_OK_DISPLAY_INFO, out)
            }
            CMD_GET_CAPSET_INFO => {
                let Some(index) = word(0) else {
                    return invalid;
                };
                let mut out = vec![0u8; 16];
                match CAPSETS.get(index as usize) {
                    Some(id) => {
                        let (version, size) = self.renderer.capset_info(*id);
                        out[0..4].copy_from_slice(&id.to_le_bytes());
                        out[4..8].copy_from_slice(&version.to_le_bytes());
                        out[8..12].copy_from_slice(&size.to_le_bytes());
                    }
                    None => out[0..4].copy_from_slice(&u32::MAX.to_le_bytes()),
                }
                (RESP_OK_CAPSET_INFO, out)
            }
            CMD_GET_CAPSET => {
                let (Some(id), Some(version)) = (word(0), word(1)) else {
                    return invalid;
                };
                if !CAPSETS.contains(&id) {
                    return invalid;
                }
                (RESP_OK_CAPSET, self.renderer.capset(id, version))
            }
            CMD_CTX_CREATE => {
                let (Some(nlen), Some(context_init)) = (word(0), word(1)) else {
                    return invalid;
                };
                let name = req
                    .get(HDR + 8..HDR + 8 + (nlen as usize).min(64))
                    .unwrap_or(&[]);
                let name = String::from_utf8_lossy(name);
                let name = name.trim_end_matches('\0');
                let name = if name.is_empty() {
                    "gpu_renderer"
                } else {
                    name
                };
                match self.renderer.context_create(ctx, name, context_init) {
                    Ok(()) => ok,
                    Err(_) => failed,
                }
            }
            CMD_CTX_DESTROY => {
                self.renderer.context_destroy(ctx);
                ok
            }
            CMD_CTX_ATTACH_RESOURCE | CMD_CTX_DETACH_RESOURCE => {
                let Some(id) = word(0) else {
                    return invalid;
                };
                match self.resources.get(&id) {
                    None => return no_resource,
                    // A blob the renderer was never told of has nothing to attach.
                    Some(Resource::Blob {
                        renderer_has_it: false,
                        ..
                    }) => return ok,
                    Some(_) => {}
                }
                if kind == CMD_CTX_ATTACH_RESOURCE {
                    self.renderer.ctx_attach_resource(ctx, id);
                } else {
                    self.renderer.ctx_detach_resource(ctx, id);
                }
                ok
            }
            CMD_RESOURCE_CREATE_2D | CMD_RESOURCE_CREATE_3D => {
                let args = if kind == CMD_RESOURCE_CREATE_2D {
                    let (Some(id), Some(format), Some(w), Some(h)) =
                        (word(0), word(1), word(2), word(3))
                    else {
                        return invalid;
                    };
                    // A plain picture is a 2D texture that can be drawn to.
                    ResourceCreate {
                        handle: id,
                        target: 2,
                        format,
                        bind: 2,
                        width: w,
                        height: h,
                        depth: 1,
                        array_size: 1,
                        ..Default::default()
                    }
                } else {
                    let mut f = [0u32; 11];
                    for (i, v) in f.iter_mut().enumerate() {
                        let Some(w) = word(i) else {
                            return invalid;
                        };
                        *v = w;
                    }
                    ResourceCreate {
                        handle: f[0],
                        target: f[1],
                        format: f[2],
                        bind: f[3],
                        width: f[4],
                        height: f[5],
                        depth: f[6],
                        array_size: f[7],
                        last_level: f[8],
                        nr_samples: f[9],
                        flags: f[10],
                    }
                };
                if args.handle == 0 || self.resources.contains_key(&args.handle) {
                    return no_resource;
                }
                if self.renderer.resource_create(&args).is_err() {
                    return failed;
                }
                self.resources.insert(
                    args.handle,
                    Resource::Image {
                        width: args.width,
                        height: args.height,
                        format: args.format,
                        attached: false,
                    },
                );
                ok
            }
            CMD_RESOURCE_UNREF => {
                let Some(id) = word(0) else {
                    return invalid;
                };
                self.unmap(id);
                let Some(resource) = self.resources.remove(&id) else {
                    return no_resource;
                };
                if !matches!(
                    resource,
                    Resource::Blob {
                        renderer_has_it: false,
                        ..
                    }
                ) {
                    self.renderer.resource_unref(id);
                }
                if self.scanout.is_some_and(|(shown, _)| shown == id) {
                    self.scanout = None;
                }
                ok
            }
            CMD_RESOURCE_ATTACH_BACKING => {
                let (Some(id), Some(count)) = (word(0), word(1)) else {
                    return invalid;
                };
                let Some(Resource::Image { attached, .. }) = self.resources.get_mut(&id) else {
                    return no_resource;
                };
                let Some(mut iov) = Self::iovecs(req, HDR + 8, count, mem) else {
                    return invalid;
                };
                // SAFETY: the pieces are guest RAM, which outlives the
                // renderer's use of them (the machine owns both).
                if unsafe { self.renderer.resource_attach(id, &mut iov) }.is_err() {
                    return failed;
                }
                *attached = true;
                ok
            }
            CMD_RESOURCE_DETACH_BACKING => {
                let Some(id) = word(0) else {
                    return invalid;
                };
                let Some(Resource::Image { attached, .. }) = self.resources.get_mut(&id) else {
                    return no_resource;
                };
                self.renderer.resource_detach(id);
                *attached = false;
                ok
            }
            CMD_TRANSFER_TO_HOST_2D => {
                // rect x, y, w, h; offset u64; resource id.
                let (Some(x), Some(y), Some(w), Some(h), Some(offset), Some(id)) = (
                    word(0),
                    word(1),
                    word(2),
                    word(3),
                    u64_at(req, HDR + 16),
                    word(6),
                ) else {
                    return invalid;
                };
                if self.image(id).is_none() {
                    return no_resource;
                }
                let t = Transfer {
                    resource: id,
                    area: Box3 {
                        x,
                        y,
                        z: 0,
                        w,
                        h,
                        d: 1,
                    },
                    offset,
                    ..Default::default()
                };
                match self.renderer.transfer_write(&t) {
                    Ok(()) => ok,
                    Err(_) => failed,
                }
            }
            CMD_TRANSFER_TO_HOST_3D | CMD_TRANSFER_FROM_HOST_3D => {
                let mut f = [0u32; 6];
                for (i, v) in f.iter_mut().enumerate() {
                    let Some(w) = word(i) else {
                        return invalid;
                    };
                    *v = w;
                }
                let (Some(offset), Some(id), Some(level), Some(stride), Some(layer_stride)) =
                    (u64_at(req, HDR + 24), word(8), word(9), word(10), word(11))
                else {
                    return invalid;
                };
                if self.image(id).is_none() {
                    return no_resource;
                }
                let t = Transfer {
                    resource: id,
                    ctx,
                    level,
                    stride,
                    layer_stride,
                    area: Box3 {
                        x: f[0],
                        y: f[1],
                        z: f[2],
                        w: f[3],
                        h: f[4],
                        d: f[5],
                    },
                    offset,
                };
                let done = if kind == CMD_TRANSFER_TO_HOST_3D {
                    self.renderer.transfer_write(&t)
                } else {
                    self.renderer.transfer_read(&t)
                };
                match done {
                    Ok(()) => ok,
                    Err(_) => failed,
                }
            }
            CMD_SUBMIT_3D => {
                let Some(size) = word(0) else {
                    return invalid;
                };
                let size = size as usize;
                if size == 0 {
                    return ok;
                }
                if !size.is_multiple_of(4) {
                    return invalid;
                }
                let Some(commands) = req.get_mut(HDR + 8..HDR + 8 + size) else {
                    return invalid;
                };
                match self.renderer.submit(ctx, commands) {
                    Ok(()) => ok,
                    Err(_) => failed,
                }
            }
            CMD_RESOURCE_CREATE_BLOB => {
                let (Some(id), Some(blob_mem), Some(blob_flags), Some(blob_id), Some(size)) = (
                    word(0),
                    word(1),
                    word(2),
                    u64_at(req, HDR + 16),
                    u64_at(req, HDR + 24),
                ) else {
                    return invalid;
                };
                if id == 0 || self.resources.contains_key(&id) {
                    return no_resource;
                }
                // The guest's own memory given a resource number, outside
                // any context: the renderer has no part in it.
                let renderer_has_it = !(ctx == 0 && blob_mem == BLOB_MEM_GUEST);
                if renderer_has_it {
                    let blob = CreateBlob {
                        blob_mem,
                        blob_flags,
                        blob_id,
                        size,
                    };
                    if self.renderer.create_blob(ctx, id, &blob).is_err() {
                        return failed;
                    }
                }
                self.resources.insert(
                    id,
                    Resource::Blob {
                        renderer_has_it,
                        mapped: None,
                    },
                );
                ok
            }
            CMD_RESOURCE_MAP_BLOB => {
                let (Some(id), Some(offset)) = (word(0), u64_at(req, HDR + 8)) else {
                    return invalid;
                };
                let Some(Resource::Blob {
                    renderer_has_it: true,
                    mapped,
                }) = self.resources.get_mut(&id)
                else {
                    return no_resource;
                };
                if mapped.is_some() {
                    return invalid;
                }
                let Some(mapper) = self.mapper.get() else {
                    return failed;
                };
                let Ok((host, size)) = self.renderer.resource_map(id) else {
                    return failed;
                };
                let len = size.div_ceil(PAGE) * PAGE;
                let fits = offset.is_multiple_of(PAGE)
                    && (host as u64).is_multiple_of(PAGE)
                    && offset.checked_add(len).is_some_and(|end| end <= SHM_SIZE);
                // SAFETY: the renderer keeps the range until `resource_unmap`,
                // which this device calls only after unmapping it here.
                if !fits || unsafe { mapper.map(self.shm_base + offset, host, len) }.is_err() {
                    let _ = self.renderer.resource_unmap(id);
                    return failed;
                }
                *mapped = Some((self.shm_base + offset, len));
                let info = self.renderer.resource_map_info(id).unwrap_or(0) & 0x0f;
                let mut out = vec![0u8; 8];
                out[0..4].copy_from_slice(&info.to_le_bytes());
                (RESP_OK_MAP_INFO, out)
            }
            CMD_RESOURCE_UNMAP_BLOB => {
                let Some(id) = word(0) else {
                    return invalid;
                };
                if self.unmap(id) {
                    ok
                } else {
                    no_resource
                }
            }
            CMD_RESOURCE_ASSIGN_UUID => {
                let Some(id) = word(0) else {
                    return invalid;
                };
                if !self.resources.contains_key(&id) {
                    return no_resource;
                }
                let mut uuid = vec![0u8; 16];
                uuid[12..16].copy_from_slice(&id.to_be_bytes());
                (RESP_OK_RESOURCE_UUID, uuid)
            }
            CMD_SET_SCANOUT | CMD_SET_SCANOUT_BLOB => {
                let (Some(x), Some(y), Some(w), Some(h), Some(scanout), Some(id)) =
                    (word(0), word(1), word(2), word(3), word(4), word(5))
                else {
                    return invalid;
                };
                if scanout != 0 {
                    return invalid;
                }
                if id == 0 {
                    self.scanout = None;
                    return ok;
                }
                let Some((width, height, _, _)) = self.image(id) else {
                    // A blob on the screen cannot be read back; nothing shows.
                    return if self.resources.contains_key(&id) {
                        ok
                    } else {
                        no_resource
                    };
                };
                let inside = w > 0
                    && h > 0
                    && x.checked_add(w).is_some_and(|r| r <= width)
                    && y.checked_add(h).is_some_and(|b| b <= height);
                if !inside {
                    return invalid;
                }
                self.scanout = Some((id, (x, y, w, h)));
                ok
            }
            CMD_RESOURCE_FLUSH => {
                // rect x, y, w, h; resource id.
                let Some(id) = word(4) else {
                    return invalid;
                };
                if !self.resources.contains_key(&id) {
                    return no_resource;
                }
                self.present(id);
                ok
            }
            _ => invalid,
        }
    }

    /// Hand back the held answers whose fences the renderer has finished.
    fn release(&mut self, queue: &mut Queue, mem: &GuestMemory) -> bool {
        if self.held.is_empty() {
            return false;
        }
        let done = self.fences.completed.lock().unwrap().clone();
        let mut any = false;
        let mut i = 0;
        while i < self.held.len() {
            let h = &self.held[i];
            if done
                .get(&h.ring)
                .is_some_and(|newest| *newest >= h.fence_id)
            {
                let h = self.held.remove(i);
                any |= queue.push_used(mem, h.head, h.written).is_ok();
            } else {
                i += 1;
            }
        }
        any
    }
}

impl VirtioDevice for Gpu3d {
    fn device_type(&self) -> u32 {
        TYPE_GPU
    }

    fn features(&self) -> u64 {
        F_VIRGL | F_RESOURCE_BLOB | F_CONTEXT_INIT
    }

    fn queue_max_sizes(&self) -> Vec<u16> {
        vec![256, 16] // control, cursor
    }

    fn read_config(&self, offset: u64, data: &mut [u8]) {
        // events_read, events_clear, num_scanouts, num_capsets.
        let mut cfg = [0u8; 16];
        cfg[8..12].copy_from_slice(&1u32.to_le_bytes());
        cfg[12..16].copy_from_slice(&(CAPSETS.len() as u32).to_le_bytes());
        for (i, b) in data.iter_mut().enumerate() {
            *b = cfg.get(offset as usize + i).copied().unwrap_or(0);
        }
    }

    fn notify(&mut self, index: usize, queue: &mut Queue, mem: &GuestMemory) -> bool {
        use std::sync::atomic::Ordering;
        if index != 0 {
            // The cursor queue: taken and ignored (Android draws its own).
            let mut any = false;
            while let Ok(Some(chain)) = queue.pop(mem) {
                any |= queue.push_used(mem, chain.head, 0).is_ok();
            }
            return any;
        }
        // Fences that finish while this runs are picked up at the end; the
        // renderer must not wake the machine, which is waiting on us.
        self.fences.busy.store(true, Ordering::SeqCst);
        let mut any = false;
        while let Ok(Some(chain)) = queue.pop(mem) {
            let mut req = chain.read_request(mem, MAX_REQUEST).unwrap_or_default();
            let (kind, body) = self.control(&mut req, mem);
            let flags = u32_at(&req, 4).unwrap_or(0);
            let fenced = flags & FLAG_FENCE != 0;
            // The answer's header echoes the request's fence.
            let mut resp = vec![0u8; HDR];
            resp[0..4].copy_from_slice(&kind.to_le_bytes());
            if fenced && req.len() >= HDR {
                resp[4..8]
                    .copy_from_slice(&(flags & (FLAG_FENCE | FLAG_INFO_RING_IDX)).to_le_bytes());
                resp[8..24].copy_from_slice(&req[8..24]);
            }
            resp.extend_from_slice(&body);
            let written = chain.write_response(mem, &resp).unwrap_or(0);
            let wait = fenced && req.len() >= HDR && {
                let fence = Fence {
                    flags: if flags & FLAG_INFO_RING_IDX != 0 {
                        FLAG_FENCE | FENCE_RING_IDX
                    } else {
                        FLAG_FENCE
                    },
                    fence_id: u64_at(&req, 8).unwrap_or(0),
                    ctx_id: u32_at(&req, 16).unwrap_or(0),
                    ring_idx: req[20],
                };
                // Ring 1 of a context carries placeholders: done at once.
                let placeholder = flags & FLAG_INFO_RING_IDX != 0 && fence.ring_idx == 1;
                if placeholder || self.renderer.create_fence(&fence).is_err() {
                    false
                } else {
                    self.held.push(Held {
                        ring: ring_of(&fence),
                        fence_id: fence.fence_id,
                        head: chain.head,
                        written,
                    });
                    true
                }
            };
            if !wait {
                if queue.push_used(mem, chain.head, written).is_err() {
                    break;
                }
                any = true;
            }
        }
        self.fences.busy.store(false, Ordering::SeqCst);
        any | self.release(queue, mem)
    }

    fn poll(&mut self, queues: &mut [Queue], mem: &GuestMemory) -> Vec<usize> {
        let Some(control) = queues.get_mut(0) else {
            return Vec::new();
        };
        if self.release(control, mem) {
            vec![0]
        } else {
            Vec::new()
        }
    }

    fn reset(&mut self) {
        let ids: Vec<u32> = self.resources.keys().copied().collect();
        for id in ids {
            self.unmap(id);
            if !matches!(
                self.resources.remove(&id),
                Some(Resource::Blob {
                    renderer_has_it: false,
                    ..
                })
            ) {
                self.renderer.resource_unref(id);
            }
        }
        self.scanout = None;
        self.held.clear();
    }

    fn shm_region(&self) -> Option<(u8, u64)> {
        Some((SHM_ID, SHM_SIZE))
    }

    fn set_shm_base(&mut self, gpa: u64) {
        self.shm_base = gpa;
    }
}

#[cfg(test)]
mod tests {
    use super::super::queue::tests::{publish, put_desc, queue, USED};
    use super::*;

    const WRITE: u16 = 2;
    const NEXT: u16 = 1;

    /// A renderer that writes down what it is asked and has canned answers.
    #[derive(Default)]
    struct Fake {
        log: Mutex<Vec<String>>,
        /// Memory "inside the renderer" for blobs to be mapped from.
        blob: Mutex<Option<Vec<u8>>>,
        /// The address the first backing piece was attached at.
        attached_at: Mutex<usize>,
    }

    impl Fake {
        fn say(&self, what: String) {
            self.log.lock().unwrap().push(what);
        }
        fn log(&self) -> Vec<String> {
            std::mem::take(&mut *self.log.lock().unwrap())
        }
    }

    impl Renderer for Fake {
        fn capset_info(&self, id: u32) -> (u32, u32) {
            (0, if id == 3 { 116 } else { 16 })
        }
        fn capset(&self, id: u32, _version: u32) -> Vec<u8> {
            vec![id as u8; if id == 3 { 116 } else { 16 }]
        }
        fn context_create(&self, ctx: u32, name: &str, init: u32) -> Result<()> {
            self.say(format!("ctx_create {ctx} {name} {init}"));
            Ok(())
        }
        fn context_destroy(&self, ctx: u32) {
            self.say(format!("ctx_destroy {ctx}"));
        }
        fn submit(&self, ctx: u32, commands: &mut [u8]) -> Result<()> {
            self.say(format!("submit {ctx} {commands:?}"));
            Ok(())
        }
        fn create_fence(&self, fence: &Fence) -> Result<()> {
            self.say(format!(
                "fence {} ring {:?}",
                fence.fence_id,
                ring_of(fence)
            ));
            Ok(())
        }
        fn resource_create(&self, a: &ResourceCreate) -> Result<()> {
            self.say(format!(
                "create {} {}x{} target {} bind {}",
                a.handle, a.width, a.height, a.target, a.bind
            ));
            Ok(())
        }
        fn resource_unref(&self, resource: u32) {
            self.say(format!("unref {resource}"));
        }
        unsafe fn resource_attach(&self, resource: u32, iov: &mut [IoVec]) -> Result<()> {
            *self.attached_at.lock().unwrap() = iov[0].base as usize;
            self.say(format!(
                "attach {resource} {} pieces, first {} bytes",
                iov.len(),
                iov[0].len
            ));
            Ok(())
        }
        fn resource_detach(&self, resource: u32) {
            self.say(format!("detach {resource}"));
        }
        fn ctx_attach_resource(&self, ctx: u32, resource: u32) {
            self.say(format!("ctx_attach {ctx} {resource}"));
        }
        fn ctx_detach_resource(&self, ctx: u32, resource: u32) {
            self.say(format!("ctx_detach {ctx} {resource}"));
        }
        fn transfer_write(&self, t: &Transfer) -> Result<()> {
            self.say(format!(
                "write {} box {},{} {}x{}x{}",
                t.resource, t.area.x, t.area.y, t.area.w, t.area.h, t.area.d
            ));
            Ok(())
        }
        fn transfer_read(&self, t: &Transfer) -> Result<()> {
            self.say(format!("read {}", t.resource));
            Ok(())
        }
        fn read_pixels(&self, t: &Transfer, out: &mut [u8]) -> Result<()> {
            // Each pixel says where it is: (x, y, 0, 255).
            for (i, px) in out.chunks_exact_mut(4).enumerate() {
                px.copy_from_slice(&[
                    (i as u32 % t.area.w) as u8,
                    (i as u32 / t.area.w) as u8,
                    0,
                    255,
                ]);
            }
            Ok(())
        }
        fn create_blob(&self, ctx: u32, resource: u32, b: &CreateBlob) -> Result<()> {
            self.say(format!(
                "blob {ctx} {resource} mem {} flags {} id {} size {}",
                b.blob_mem, b.blob_flags, b.blob_id, b.size
            ));
            // Page-aligned memory of that size, as a real renderer gives.
            *self.blob.lock().unwrap() = Some(vec![0u8; b.size as usize + 2 * PAGE as usize]);
            Ok(())
        }
        fn resource_map(&self, resource: u32) -> Result<(*mut u8, u64)> {
            self.say(format!("map {resource}"));
            let mut blob = self.blob.lock().unwrap();
            let v = blob.as_mut().unwrap();
            let ptr = v.as_mut_ptr();
            let aligned = ptr.wrapping_add(ptr.align_offset(PAGE as usize));
            Ok((aligned, v.len() as u64 - 2 * PAGE))
        }
        fn resource_unmap(&self, resource: u32) -> Result<()> {
            self.say(format!("unmap {resource}"));
            Ok(())
        }
        fn resource_map_info(&self, _: u32) -> Result<u32> {
            Ok(0xf3) // only the low four bits are for the guest
        }
    }

    /// A mapper that writes down what it is asked.
    #[derive(Default)]
    struct Maps(Arc<Mutex<Vec<String>>>);

    impl crate::memory::Mapper for Maps {
        unsafe fn map(&self, gpa: u64, host: *mut u8, len: u64) -> Result<()> {
            let aligned = (host as u64).is_multiple_of(PAGE);
            self.0
                .lock()
                .unwrap()
                .push(format!("map {gpa:#x} {len:#x} aligned {aligned}"));
            Ok(())
        }
        fn unmap(&self, gpa: u64, len: u64) -> Result<()> {
            self.0
                .lock()
                .unwrap()
                .push(format!("unmap {gpa:#x} {len:#x}"));
            Ok(())
        }
    }

    struct Rig {
        gpu: Gpu3d,
        fake: Arc<Fake>,
        fences: Arc<Fences>,
        maps: Arc<Mutex<Vec<String>>>,
        display: Display,
        mem: GuestMemory,
        queue: Queue,
        sent: u16,
    }

    const BASE: u64 = 0x80_0000_0000;

    fn rig() -> Rig {
        let fake = Arc::new(Fake::default());
        let fences = Arc::new(Fences::default());
        let maps = Arc::new(Mutex::new(Vec::new()));
        let slot = MapperSlot::default();
        let _ = slot.set(Box::new(Maps(maps.clone())));
        let display = Display::new(4, 2);
        let mut gpu = Gpu3d::new(display.clone(), fake.clone(), fences.clone(), slot);
        gpu.set_shm_base(BASE);
        Rig {
            gpu,
            fake,
            fences,
            maps,
            display,
            mem: GuestMemory::new(64 << 20).unwrap(),
            queue: queue(64),
            sent: 0,
        }
    }

    /// A request: header (type, flags, fence, ctx, ring) then `body`.
    fn request(kind: u32, flags: u32, fence: u64, ctx: u32, ring: u8, body: &[u8]) -> Vec<u8> {
        let mut r = Vec::new();
        r.extend_from_slice(&kind.to_le_bytes());
        r.extend_from_slice(&flags.to_le_bytes());
        r.extend_from_slice(&fence.to_le_bytes());
        r.extend_from_slice(&ctx.to_le_bytes());
        r.extend_from_slice(&[ring, 0, 0, 0]);
        r.extend_from_slice(body);
        r
    }

    fn words(w: &[u32]) -> Vec<u8> {
        w.iter().flat_map(|v| v.to_le_bytes()).collect()
    }

    impl Rig {
        /// Send a request; returns how many answers have been handed back
        /// in all, and this request's answer buffer.
        fn send(&mut self, req: &[u8]) -> (u16, Vec<u8>) {
            let n = self.sent;
            let (out, back) = (0x10_0000 + n as u64 * 0x2000, 0x10_1000 + n as u64 * 0x2000);
            self.mem.write(out, req).unwrap();
            put_desc(&self.mem, n * 2, out, req.len() as u32, NEXT, n * 2 + 1);
            put_desc(&self.mem, n * 2 + 1, back, 0x1000, WRITE, 0);
            publish(&self.mem, 64, n, n * 2);
            self.sent += 1;
            self.gpu.notify(0, &mut self.queue, &self.mem);
            let mut answer = vec![0u8; 200];
            self.mem.read(back, &mut answer).unwrap();
            (self.mem.read_u16(USED + 2).unwrap(), answer)
        }

        fn kind(&mut self, req: &[u8]) -> u32 {
            u32_at(&self.send(req).1, 0).unwrap()
        }
    }

    #[test]
    fn the_guest_learns_what_the_renderer_offers() {
        let mut r = rig();
        let mut config = [0u8; 16];
        r.gpu.read_config(0, &mut config);
        assert_eq!(u32_at(&config, 12), Some(2), "two capability sets");
        assert_eq!(r.gpu.features(), 0b11001, "virgl, blobs, context types");
        assert_eq!(r.gpu.shm_region(), Some((1, 8 << 30)));
        // By index: Vulkan (3, 116 bytes), then the composer (9, 16 bytes).
        for (index, id, size) in [(0u32, 3u32, 116u32), (1, 9, 16)] {
            let (_, a) = r.send(&request(
                CMD_GET_CAPSET_INFO,
                0,
                0,
                0,
                0,
                &words(&[index, 0]),
            ));
            assert_eq!(
                (u32_at(&a, 0), u32_at(&a, 24), u32_at(&a, 32)),
                (Some(RESP_OK_CAPSET_INFO), Some(id), Some(size))
            );
        }
        // Beyond the list: an id the kernel recognizes as "none".
        let (_, a) = r.send(&request(CMD_GET_CAPSET_INFO, 0, 0, 0, 0, &words(&[2, 0])));
        assert_eq!(u32_at(&a, 24), Some(u32::MAX));
        let (_, a) = r.send(&request(CMD_GET_CAPSET, 0, 0, 0, 0, &words(&[3, 0])));
        assert_eq!(
            (u32_at(&a, 0), &a[24..24 + 116]),
            (Some(RESP_OK_CAPSET), &[3u8; 116][..])
        );
        assert_eq!(
            r.kind(&request(CMD_GET_CAPSET, 0, 0, 0, 0, &words(&[4, 0]))),
            RESP_ERR_INVALID_PARAMETER
        );
    }

    #[test]
    fn contexts_commands_and_resources_reach_the_renderer() {
        let mut r = rig();
        let mut body = words(&[5, 3]);
        body.extend_from_slice(b"angle");
        body.resize(72, 0);
        assert_eq!(
            r.kind(&request(CMD_CTX_CREATE, 0, 0, 7, 0, &body)),
            RESP_OK_NODATA
        );
        // A buffer from Android's allocator: created, given guest memory,
        // attached to the context, filled.
        let create = words(&[20, 2, 1, 2, 4, 2, 1, 1, 0, 0, 0, 0]);
        assert_eq!(
            r.kind(&request(CMD_RESOURCE_CREATE_3D, 0, 0, 7, 0, &create)),
            RESP_OK_NODATA
        );
        let mut backing = words(&[20, 1]);
        backing.extend_from_slice(&0x30_0000u64.to_le_bytes());
        backing.extend_from_slice(&words(&[4096, 0]));
        assert_eq!(
            r.kind(&request(CMD_RESOURCE_ATTACH_BACKING, 0, 0, 0, 0, &backing)),
            RESP_OK_NODATA
        );
        assert_eq!(
            *r.fake.attached_at.lock().unwrap(),
            r.mem.host_ptr(0x30_0000, 4096).unwrap() as usize,
            "the renderer is given the host address of the guest's pages"
        );
        assert_eq!(
            r.kind(&request(
                CMD_CTX_ATTACH_RESOURCE,
                0,
                0,
                7,
                0,
                &words(&[20, 0])
            )),
            RESP_OK_NODATA
        );
        let mut transfer = words(&[0, 0, 0, 4, 2, 1]);
        transfer.extend_from_slice(&0u64.to_le_bytes());
        transfer.extend_from_slice(&words(&[20, 0, 16, 0]));
        assert_eq!(
            r.kind(&request(CMD_TRANSFER_TO_HOST_3D, 0, 0, 7, 0, &transfer)),
            RESP_OK_NODATA
        );
        let mut submit = words(&[8, 0]);
        submit.extend_from_slice(&[1, 2, 3, 4, 5, 6, 7, 8]);
        assert_eq!(
            r.kind(&request(CMD_SUBMIT_3D, 0, 0, 7, 0, &submit)),
            RESP_OK_NODATA
        );
        assert_eq!(
            r.fake.log(),
            vec![
                "ctx_create 7 angle 3",
                "create 20 4x2 target 2 bind 2",
                "attach 20 1 pieces, first 4096 bytes",
                "ctx_attach 7 20",
                "write 20 box 0,0 4x2x1",
                "submit 7 [1, 2, 3, 4, 5, 6, 7, 8]",
            ]
        );
        // A command buffer that is not whole words, or runs past the request.
        let mut odd = words(&[6, 0]);
        odd.extend_from_slice(&[0; 6]);
        assert_eq!(
            r.kind(&request(CMD_SUBMIT_3D, 0, 0, 7, 0, &odd)),
            RESP_ERR_INVALID_PARAMETER
        );
        assert_eq!(
            r.kind(&request(CMD_SUBMIT_3D, 0, 0, 7, 0, &words(&[400, 0]))),
            RESP_ERR_INVALID_PARAMETER
        );
        // Backing that is not guest memory is refused, not handed over.
        let mut wild = words(&[20, 1]);
        wild.extend_from_slice(&0xffff_0000_0000u64.to_le_bytes());
        wild.extend_from_slice(&words(&[4096, 0]));
        assert_eq!(
            r.kind(&request(CMD_RESOURCE_ATTACH_BACKING, 0, 0, 0, 0, &wild)),
            RESP_ERR_INVALID_PARAMETER
        );
        assert!(r.fake.log().is_empty(), "none of that reached the renderer");
    }

    #[test]
    fn a_fenced_answer_waits_for_the_renderer() {
        let mut r = rig();
        let woken = Arc::new(Mutex::new(0));
        let w = woken.clone();
        r.fences.set_wake(Box::new(move || *w.lock().unwrap() += 1));
        let mut submit = words(&[4, 0]);
        submit.extend_from_slice(&[9, 9, 9, 9]);
        // Fence 5 on ring 0 of context 7: written, but not handed back.
        let (handed, a) = r.send(&request(
            CMD_SUBMIT_3D,
            FLAG_FENCE | FLAG_INFO_RING_IDX,
            5,
            7,
            0,
            &submit,
        ));
        assert_eq!(handed, 0, "held");
        assert_eq!(
            (u32_at(&a, 0), u32_at(&a, 4), u64_at(&a, 8), u32_at(&a, 16)),
            (Some(RESP_OK_NODATA), Some(3), Some(5), Some(7))
        );
        assert_eq!(
            r.fake.log(),
            vec!["submit 7 [9, 9, 9, 9]", "fence 5 ring Some((7, 0))"]
        );
        // An unfenced request meanwhile is answered at once.
        assert_eq!(r.send(&request(CMD_CTX_DESTROY, 0, 0, 9, 0, &[])).0, 1);
        // Another timeline finishing does not release it.
        r.fences.completed(Fence {
            flags: 1,
            fence_id: 99,
            ctx_id: 0,
            ring_idx: 0,
        });
        let mut queues = vec![std::mem::replace(&mut r.queue, queue(64)), queue(64)];
        assert!(r.gpu.poll(&mut queues, &r.mem).is_empty());
        // Its own does: the renderer's thread wakes the machine, whose poll
        // hands the answer back.
        r.fences.completed(Fence {
            flags: 1 | FENCE_RING_IDX,
            fence_id: 5,
            ctx_id: 7,
            ring_idx: 0,
        });
        assert_eq!(*woken.lock().unwrap(), 2);
        assert_eq!(r.gpu.poll(&mut queues, &r.mem), vec![0]);
        assert_eq!(r.mem.read_u16(USED + 2).unwrap(), 2);
        r.queue = queues.remove(0);
        // A fence already behind the renderer is not held at all, and ring 1
        // (placeholders) never goes to the renderer.
        assert_eq!(
            r.send(&request(
                CMD_SUBMIT_3D,
                FLAG_FENCE | FLAG_INFO_RING_IDX,
                4,
                7,
                0,
                &submit
            ))
            .0,
            3
        );
        r.fake.log();
        assert_eq!(
            r.send(&request(
                CMD_SUBMIT_3D,
                FLAG_FENCE | FLAG_INFO_RING_IDX,
                6,
                7,
                1,
                &submit
            ))
            .0,
            4
        );
        assert_eq!(
            r.fake.log(),
            vec!["submit 7 [9, 9, 9, 9]"],
            "no fence was asked for"
        );
    }

    #[test]
    fn a_blob_is_placed_in_the_guests_window_and_taken_out_again() {
        let mut r = rig();
        // The command ring: host memory, mappable, 1 MiB + 12 KiB.
        let size = 1_048_576u64 + 12_288;
        let mut create = words(&[30, 2, 1, 0]);
        create.extend_from_slice(&0u64.to_le_bytes());
        create.extend_from_slice(&size.to_le_bytes());
        assert_eq!(
            r.kind(&request(CMD_RESOURCE_CREATE_BLOB, 0, 0, 7, 0, &create)),
            RESP_OK_NODATA
        );
        let mut map = words(&[30, 0]);
        map.extend_from_slice(&0x20_0000u64.to_le_bytes());
        let (_, a) = r.send(&request(CMD_RESOURCE_MAP_BLOB, 0, 0, 7, 0, &map));
        assert_eq!(
            (u32_at(&a, 0), u32_at(&a, 24)),
            (Some(RESP_OK_MAP_INFO), Some(3)),
            "cache mode, low bits only"
        );
        assert_eq!(
            *r.maps.lock().unwrap(),
            vec![format!(
                "map {:#x} {size:#x} aligned true",
                BASE + 0x20_0000
            )]
        );
        // Twice is refused; a window offset that is not page-aligned, or
        // beyond the window, is refused and the renderer's mapping undone.
        assert_eq!(
            r.kind(&request(CMD_RESOURCE_MAP_BLOB, 0, 0, 7, 0, &map)),
            RESP_ERR_INVALID_PARAMETER
        );
        assert_eq!(
            r.kind(&request(
                CMD_RESOURCE_UNMAP_BLOB,
                0,
                0,
                7,
                0,
                &words(&[30, 0])
            )),
            RESP_OK_NODATA
        );
        for bad in [0x20_0800u64, 8 << 30] {
            let mut map = words(&[30, 0]);
            map.extend_from_slice(&bad.to_le_bytes());
            assert_eq!(
                r.kind(&request(CMD_RESOURCE_MAP_BLOB, 0, 0, 7, 0, &map)),
                RESP_ERR_UNSPEC
            );
        }
        assert_eq!(r.maps.lock().unwrap().len(), 2, "one map, one unmap");
        // Unref of a mapped blob unmaps it first.
        let mut map = words(&[30, 0]);
        map.extend_from_slice(&0u64.to_le_bytes());
        r.send(&request(CMD_RESOURCE_MAP_BLOB, 0, 0, 7, 0, &map));
        r.fake.log();
        assert_eq!(
            r.kind(&request(CMD_RESOURCE_UNREF, 0, 0, 0, 0, &words(&[30, 0]))),
            RESP_OK_NODATA
        );
        assert_eq!(r.fake.log(), vec!["unmap 30", "unref 30"]);
        assert!(r
            .maps
            .lock()
            .unwrap()
            .last()
            .unwrap()
            .starts_with(&format!("unmap {BASE:#x}")));
        // The guest's own memory given a number outside any context is the
        // device's business alone.
        let mut guest = words(&[31, BLOB_MEM_GUEST, 0, 0]);
        guest.extend_from_slice(&[0u8; 16]);
        assert_eq!(
            r.kind(&request(CMD_RESOURCE_CREATE_BLOB, 0, 0, 0, 0, &guest)),
            RESP_OK_NODATA
        );
        assert_eq!(
            r.kind(&request(CMD_RESOURCE_UNREF, 0, 0, 0, 0, &words(&[31, 0]))),
            RESP_OK_NODATA
        );
        assert!(r.fake.log().is_empty());
    }

    #[test]
    fn a_flush_reads_the_picture_back_from_the_renderer() {
        let mut r = rig();
        // A buffer wider than the screen (8x2), shown from x=2.
        let create = words(&[40, 2, 67, 2, 8, 2, 1, 1, 0, 0, 0, 0]);
        r.send(&request(CMD_RESOURCE_CREATE_3D, 0, 0, 0, 0, &create));
        let scanout = words(&[2, 0, 4, 2, 0, 40]);
        assert_eq!(
            r.kind(&request(CMD_SET_SCANOUT, 0, 0, 0, 0, &scanout)),
            RESP_OK_NODATA
        );
        let flush = words(&[0, 0, 8, 2, 40, 0]);
        // Nothing can be read back until memory has been attached.
        assert_eq!(
            r.kind(&request(CMD_RESOURCE_FLUSH, 0, 0, 0, 0, &flush)),
            RESP_OK_NODATA
        );
        assert_eq!(r.display.sequence(), 0);
        let mut backing = words(&[40, 1]);
        backing.extend_from_slice(&0x30_0000u64.to_le_bytes());
        backing.extend_from_slice(&words(&[4096, 0]));
        r.send(&request(CMD_RESOURCE_ATTACH_BACKING, 0, 0, 0, 0, &backing));
        assert_eq!(
            r.kind(&request(CMD_RESOURCE_FLUSH, 0, 0, 0, 0, &flush)),
            RESP_OK_NODATA
        );
        let frame = r.display.frame();
        assert_eq!(
            (frame.sequence, frame.width, frame.height, frame.format),
            (1, 4, 2, Format::Rgba)
        );
        // The renderer's pixels say where they were: the cut starts at x=2.
        assert_eq!(&frame.pixels[..8], &[2, 0, 0, 255, 3, 0, 0, 255]);
        assert_eq!(&frame.pixels[16..20], &[2, 1, 0, 255], "second row");
        // A scanout rectangle outside the buffer, or an unknown buffer.
        assert_eq!(
            r.kind(&request(
                CMD_SET_SCANOUT,
                0,
                0,
                0,
                0,
                &words(&[6, 0, 4, 2, 0, 40])
            )),
            RESP_ERR_INVALID_PARAMETER
        );
        assert_eq!(
            r.kind(&request(
                CMD_SET_SCANOUT,
                0,
                0,
                0,
                0,
                &words(&[0, 0, 4, 2, 0, 41])
            )),
            RESP_ERR_INVALID_RESOURCE_ID
        );
    }
}
