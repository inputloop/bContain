// SPDX-FileCopyrightText: 2026 Bryant Frankford <nathanielfrankford@gmail.com> and Contributors
// SPDX-License-Identifier: GPL-3.0-or-later OR LGPL-3.0-or-later
//! virtio-gpu in 2D mode (Virtio 1.2, section 5.7): the guest draws into
//! its own memory, tells the device which rectangle changed, and the device
//! copies it into a picture the host can show. No 3D, no capability sets.
//!
//! The picture lands in a [`Display`], which the embedder reads whenever it
//! wants the latest frame.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use super::queue::Queue;
use super::{VirtioDevice, TYPE_GPU};
use crate::memory::GuestMemory;

const CMD_GET_DISPLAY_INFO: u32 = 0x0100;
const CMD_RESOURCE_CREATE_2D: u32 = 0x0101;
const CMD_RESOURCE_UNREF: u32 = 0x0102;
const CMD_SET_SCANOUT: u32 = 0x0103;
const CMD_RESOURCE_FLUSH: u32 = 0x0104;
const CMD_TRANSFER_TO_HOST_2D: u32 = 0x0105;
const CMD_RESOURCE_ATTACH_BACKING: u32 = 0x0106;
const CMD_RESOURCE_DETACH_BACKING: u32 = 0x0107;
const CMD_UPDATE_CURSOR: u32 = 0x0300;
const CMD_MOVE_CURSOR: u32 = 0x0301;

const RESP_OK_NODATA: u32 = 0x1100;
const RESP_OK_DISPLAY_INFO: u32 = 0x1101;
const RESP_ERR_UNSPEC: u32 = 0x1200;
const RESP_ERR_OUT_OF_MEMORY: u32 = 0x1201;
const RESP_ERR_INVALID_SCANOUT_ID: u32 = 0x1202;
const RESP_ERR_INVALID_RESOURCE_ID: u32 = 0x1203;
const RESP_ERR_INVALID_PARAMETER: u32 = 0x1205;

const FLAG_FENCE: u32 = 1;
const HDR: usize = 24;
const MAX_SCANOUTS: usize = 16;
/// Limits on what a guest can make the host allocate.
const MAX_DIMENSION: u32 = 8192;
const MAX_RESOURCE_BYTES: usize = 512 << 20;
const MAX_BACKING_ENTRIES: u32 = 16384;
const MAX_REQUEST: usize = HDR + 8 + MAX_BACKING_ENTRIES as usize * 16;

/// Pixel layouts, all four bytes per pixel (enum virtio_gpu_formats).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Format {
    Bgra,
    Bgrx,
    Argb,
    Xrgb,
    Rgba,
    Xbgr,
    Abgr,
    Rgbx,
}

impl Format {
    pub(crate) fn from_wire(v: u32) -> Option<Format> {
        Some(match v {
            1 => Format::Bgra,
            2 => Format::Bgrx,
            3 => Format::Argb,
            4 => Format::Xrgb,
            67 => Format::Rgba,
            68 => Format::Xbgr,
            121 => Format::Abgr,
            134 => Format::Rgbx,
            _ => return None,
        })
    }

    /// Byte positions of red, green and blue within a pixel.
    pub fn rgb_offsets(self) -> (usize, usize, usize) {
        match self {
            Format::Bgra | Format::Bgrx => (2, 1, 0),
            Format::Argb | Format::Xrgb => (1, 2, 3),
            Format::Rgba | Format::Rgbx => (0, 1, 2),
            Format::Xbgr | Format::Abgr => (3, 2, 1),
        }
    }
}

/// The latest picture the guest presented.
#[derive(Debug, Clone)]
pub struct Frame {
    pub width: u32,
    pub height: u32,
    pub format: Format,
    /// `width * height * 4` bytes, rows top to bottom.
    pub pixels: Vec<u8>,
    /// Counts presentations; 0 means the guest has not drawn yet.
    pub sequence: u64,
}

impl Frame {
    /// The picture as 24-bit RGB, three bytes per pixel.
    pub fn to_rgb(&self) -> Vec<u8> {
        let (r, g, b) = self.format.rgb_offsets();
        let mut out = Vec::with_capacity(self.pixels.len() / 4 * 3);
        for px in self.pixels.chunks_exact(4) {
            out.extend_from_slice(&[px[r], px[g], px[b]]);
        }
        out
    }

    /// The picture as a binary PPM file (a format any image tool reads).
    pub fn to_ppm(&self) -> Vec<u8> {
        let mut out = format!("P6\n{} {}\n255\n", self.width, self.height).into_bytes();
        out.extend_from_slice(&self.to_rgb());
        out
    }
}

/// A screen the guest sees and the host reads. Cheap to clone; all clones
/// are the same screen.
#[derive(Clone)]
pub struct Display {
    width: u32,
    height: u32,
    frame: Arc<Mutex<Frame>>,
}

impl Display {
    pub fn new(width: u32, height: u32) -> Display {
        let (width, height) = (
            width.clamp(1, MAX_DIMENSION),
            height.clamp(1, MAX_DIMENSION),
        );
        Display {
            width,
            height,
            frame: Arc::new(Mutex::new(Frame {
                width,
                height,
                format: Format::Rgbx,
                pixels: vec![0; width as usize * height as usize * 4],
                sequence: 0,
            })),
        }
    }

    pub fn size(&self) -> (u32, u32) {
        (self.width, self.height)
    }

    /// A copy of the latest frame.
    pub fn frame(&self) -> Frame {
        self.frame.lock().unwrap().clone()
    }

    /// Show a new picture: `width * height * 4` bytes, rows top to bottom.
    pub(crate) fn present(&self, width: u32, height: u32, format: Format, pixels: Vec<u8>) {
        let mut frame = self.frame.lock().unwrap();
        frame.width = width;
        frame.height = height;
        frame.format = format;
        frame.pixels = pixels;
        frame.sequence += 1;
    }

    /// How many times the guest has presented.
    pub fn sequence(&self) -> u64 {
        self.frame.lock().unwrap().sequence
    }
}

struct Resource {
    width: u32,
    height: u32,
    format: Format,
    pixels: Vec<u8>,
    /// Guest memory the guest draws into: (address, length) pieces.
    backing: Vec<(u64, u32)>,
}

pub struct Gpu {
    display: Display,
    resources: HashMap<u32, Resource>,
    /// Resource shown on scanout 0, if any, and the part of it that shows:
    /// (x, y, width, height). A resource may be wider than the screen.
    scanout: Option<(u32, (u32, u32, u32, u32))>,
    allocated: usize,
}

pub(crate) fn u32_at(b: &[u8], at: usize) -> Option<u32> {
    Some(u32::from_le_bytes(b.get(at..at + 4)?.try_into().ok()?))
}

pub(crate) fn u64_at(b: &[u8], at: usize) -> Option<u64> {
    Some(u64::from_le_bytes(b.get(at..at + 8)?.try_into().ok()?))
}

/// A rectangle from a request, checked to lie inside `width` x `height`.
fn rect_at(b: &[u8], at: usize, width: u32, height: u32) -> Option<(u32, u32, u32, u32)> {
    let (x, y, w, h) = (
        u32_at(b, at)?,
        u32_at(b, at + 4)?,
        u32_at(b, at + 8)?,
        u32_at(b, at + 12)?,
    );
    let inside = x.checked_add(w).is_some_and(|r| r <= width)
        && y.checked_add(h).is_some_and(|r| r <= height);
    inside.then_some((x, y, w, h))
}

impl Gpu {
    pub fn new(display: Display) -> Gpu {
        Gpu {
            display,
            resources: HashMap::new(),
            scanout: None,
            allocated: 0,
        }
    }

    /// Copy `out.len()` bytes from a resource's backing, starting `offset`
    /// bytes into it.
    fn read_backing(
        backing: &[(u64, u32)],
        mut offset: u64,
        out: &mut [u8],
        mem: &GuestMemory,
    ) -> Option<()> {
        let mut done = 0;
        for (addr, len) in backing {
            let len = *len as u64;
            if offset >= len {
                offset -= len;
                continue;
            }
            let take = ((len - offset) as usize).min(out.len() - done);
            mem.read(addr + offset, &mut out[done..done + take]).ok()?;
            done += take;
            offset = 0;
            if done == out.len() {
                return Some(());
            }
        }
        (done == out.len()).then_some(())
    }

    /// Carry out one control request. Returns the response type and body.
    fn control(&mut self, req: &[u8], mem: &GuestMemory) -> (u32, Vec<u8>) {
        let invalid = (RESP_ERR_INVALID_PARAMETER, Vec::new());
        let Some(kind) = u32_at(req, 0) else {
            return invalid;
        };
        let body = HDR;
        match kind {
            CMD_GET_DISPLAY_INFO => {
                let mut out = vec![0u8; MAX_SCANOUTS * 24];
                let (w, h) = self.display.size();
                out[8..12].copy_from_slice(&w.to_le_bytes());
                out[12..16].copy_from_slice(&h.to_le_bytes());
                out[16..20].copy_from_slice(&1u32.to_le_bytes()); // enabled
                (RESP_OK_DISPLAY_INFO, out)
            }
            CMD_RESOURCE_CREATE_2D => {
                let (Some(id), Some(format), Some(w), Some(h)) = (
                    u32_at(req, body),
                    u32_at(req, body + 4),
                    u32_at(req, body + 8),
                    u32_at(req, body + 12),
                ) else {
                    return invalid;
                };
                let Some(format) = Format::from_wire(format) else {
                    return invalid;
                };
                if id == 0 || self.resources.contains_key(&id) {
                    return (RESP_ERR_INVALID_RESOURCE_ID, Vec::new());
                }
                if w == 0 || h == 0 || w > MAX_DIMENSION || h > MAX_DIMENSION {
                    return invalid;
                }
                let bytes = w as usize * h as usize * 4;
                if self.allocated + bytes > MAX_RESOURCE_BYTES {
                    return (RESP_ERR_OUT_OF_MEMORY, Vec::new());
                }
                self.allocated += bytes;
                self.resources.insert(
                    id,
                    Resource {
                        width: w,
                        height: h,
                        format,
                        pixels: vec![0; bytes],
                        backing: Vec::new(),
                    },
                );
                (RESP_OK_NODATA, Vec::new())
            }
            CMD_RESOURCE_UNREF => {
                let Some(id) = u32_at(req, body) else {
                    return invalid;
                };
                match self.resources.remove(&id) {
                    Some(r) => {
                        self.allocated -= r.pixels.len();
                        if self.scanout.is_some_and(|s| s.0 == id) {
                            self.scanout = None;
                        }
                        (RESP_OK_NODATA, Vec::new())
                    }
                    None => (RESP_ERR_INVALID_RESOURCE_ID, Vec::new()),
                }
            }
            CMD_SET_SCANOUT => {
                let (Some(scanout), Some(id)) = (u32_at(req, body + 16), u32_at(req, body + 20))
                else {
                    return invalid;
                };
                if scanout != 0 {
                    return (RESP_ERR_INVALID_SCANOUT_ID, Vec::new());
                }
                if id == 0 {
                    self.scanout = None; // the guest turned the screen off
                    return (RESP_OK_NODATA, Vec::new());
                }
                let Some(r) = self.resources.get(&id) else {
                    return (RESP_ERR_INVALID_RESOURCE_ID, Vec::new());
                };
                let Some(rect) = rect_at(req, body, r.width, r.height) else {
                    return invalid;
                };
                if rect.2 == 0 || rect.3 == 0 {
                    return invalid;
                }
                self.scanout = Some((id, rect));
                (RESP_OK_NODATA, Vec::new())
            }
            CMD_RESOURCE_ATTACH_BACKING => {
                let (Some(id), Some(count)) = (u32_at(req, body), u32_at(req, body + 4)) else {
                    return invalid;
                };
                if count > MAX_BACKING_ENTRIES {
                    return invalid;
                }
                let mut backing = Vec::with_capacity(count as usize);
                for i in 0..count as usize {
                    let at = body + 8 + i * 16;
                    let (Some(addr), Some(len)) = (u64_at(req, at), u32_at(req, at + 8)) else {
                        return invalid;
                    };
                    backing.push((addr, len));
                }
                match self.resources.get_mut(&id) {
                    Some(r) => {
                        r.backing = backing;
                        (RESP_OK_NODATA, Vec::new())
                    }
                    None => (RESP_ERR_INVALID_RESOURCE_ID, Vec::new()),
                }
            }
            CMD_RESOURCE_DETACH_BACKING => {
                let Some(id) = u32_at(req, body) else {
                    return invalid;
                };
                match self.resources.get_mut(&id) {
                    Some(r) => {
                        r.backing.clear();
                        (RESP_OK_NODATA, Vec::new())
                    }
                    None => (RESP_ERR_INVALID_RESOURCE_ID, Vec::new()),
                }
            }
            CMD_TRANSFER_TO_HOST_2D => {
                let (Some(offset), Some(id)) = (u64_at(req, body + 16), u32_at(req, body + 24))
                else {
                    return invalid;
                };
                let Some(r) = self.resources.get_mut(&id) else {
                    return (RESP_ERR_INVALID_RESOURCE_ID, Vec::new());
                };
                let Some((x, y, w, h)) = rect_at(req, body, r.width, r.height) else {
                    return invalid;
                };
                let stride = r.width as usize * 4;
                for row in 0..h as usize {
                    let dst = (y as usize + row) * stride + x as usize * 4;
                    let src = offset.checked_add((row * stride) as u64);
                    let line = &mut r.pixels[dst..dst + w as usize * 4];
                    let copied = src.and_then(|s| Gpu::read_backing(&r.backing, s, line, mem));
                    if copied.is_none() {
                        return (RESP_ERR_UNSPEC, Vec::new());
                    }
                }
                (RESP_OK_NODATA, Vec::new())
            }
            CMD_RESOURCE_FLUSH => {
                let Some(id) = u32_at(req, body + 16) else {
                    return invalid;
                };
                let Some(r) = self.resources.get(&id) else {
                    return (RESP_ERR_INVALID_RESOURCE_ID, Vec::new());
                };
                if rect_at(req, body, r.width, r.height).is_none() {
                    return invalid;
                }
                if let Some((_, (x, y, w, h))) = self.scanout.filter(|s| s.0 == id) {
                    let mut frame = self.display.frame.lock().unwrap();
                    frame.width = w;
                    frame.height = h;
                    frame.format = r.format;
                    frame.pixels.clear();
                    let stride = r.width as usize * 4;
                    for row in 0..h as usize {
                        let at = (y as usize + row) * stride + x as usize * 4;
                        frame
                            .pixels
                            .extend_from_slice(&r.pixels[at..at + w as usize * 4]);
                    }
                    frame.sequence += 1;
                }
                (RESP_OK_NODATA, Vec::new())
            }
            // The pointer is drawn by the guest; nothing to do for a cursor plane.
            CMD_UPDATE_CURSOR | CMD_MOVE_CURSOR => (RESP_OK_NODATA, Vec::new()),
            _ => (RESP_ERR_UNSPEC, Vec::new()),
        }
    }
}

impl VirtioDevice for Gpu {
    fn device_type(&self) -> u32 {
        TYPE_GPU
    }

    fn features(&self) -> u64 {
        0
    }

    fn queue_max_sizes(&self) -> Vec<u16> {
        vec![256, 16] // control, cursor
    }

    fn read_config(&self, offset: u64, data: &mut [u8]) {
        // struct virtio_gpu_config: events_read, events_clear, num_scanouts, num_capsets.
        let mut cfg = [0u8; 16];
        cfg[8..12].copy_from_slice(&1u32.to_le_bytes());
        for (i, b) in data.iter_mut().enumerate() {
            *b = cfg.get(offset as usize + i).copied().unwrap_or(0);
        }
    }

    fn notify(&mut self, _index: usize, queue: &mut Queue, mem: &GuestMemory) -> bool {
        let mut any = false;
        while let Ok(Some(chain)) = queue.pop(mem) {
            let req = chain.read_request(mem, MAX_REQUEST).unwrap_or_default();
            let (kind, body) = self.control(&req, mem);
            // The response header echoes the request's fence, if it had one.
            let mut resp = vec![0u8; HDR];
            resp[0..4].copy_from_slice(&kind.to_le_bytes());
            if u32_at(&req, 4).is_some_and(|f| f & FLAG_FENCE != 0) {
                resp[4..8].copy_from_slice(&FLAG_FENCE.to_le_bytes());
                resp[8..16].copy_from_slice(&req[8..16]);
                resp[16..20].copy_from_slice(&req[16..20]);
            }
            resp.extend_from_slice(&body);
            let written = chain.write_response(mem, &resp).unwrap_or(0);
            if queue.push_used(mem, chain.head, written).is_err() {
                break;
            }
            any = true;
        }
        any
    }

    fn reset(&mut self) {
        self.resources.clear();
        self.scanout = None;
        self.allocated = 0;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn request(kind: u32, body: &[u32]) -> Vec<u8> {
        let mut r = vec![0u8; HDR];
        r[0..4].copy_from_slice(&kind.to_le_bytes());
        for v in body {
            r.extend_from_slice(&v.to_le_bytes());
        }
        r
    }

    /// A 4x2 screen with resource 1 created, backed at 0x10000 and shown.
    fn ready(mem: &GuestMemory) -> (Gpu, Display) {
        let display = Display::new(4, 2);
        let mut gpu = Gpu::new(display.clone());
        let ok = |r: (u32, Vec<u8>)| assert_eq!(r.0, RESP_OK_NODATA);
        ok(gpu.control(&request(CMD_RESOURCE_CREATE_2D, &[1, 67, 4, 2]), mem));
        // Backing in two pieces: the first 20 bytes, then the remaining 12.
        ok(gpu.control(
            &request(
                CMD_RESOURCE_ATTACH_BACKING,
                &[1, 2, 0x10000, 0, 20, 0, 0x20000, 0, 12, 0],
            ),
            mem,
        ));
        ok(gpu.control(&request(CMD_SET_SCANOUT, &[0, 0, 4, 2, 0, 1]), mem));
        (gpu, display)
    }

    #[test]
    fn a_frame_travels_from_guest_memory_to_the_display() {
        let mem = GuestMemory::new(64 << 20).unwrap();
        let (mut gpu, display) = ready(&mem);
        assert_eq!(display.sequence(), 0, "nothing presented yet");
        // Eight RGBA pixels, pixel n is (n, 0x10+n, 0x20+n, 0xff).
        let pixels: Vec<u8> = (0..8u8)
            .flat_map(|n| [n, 0x10 + n, 0x20 + n, 0xff])
            .collect();
        mem.write(0x10000, &pixels[..20]).unwrap();
        mem.write(0x20000, &pixels[20..]).unwrap();
        let (kind, _) = gpu.control(
            &request(CMD_TRANSFER_TO_HOST_2D, &[0, 0, 4, 2, 0, 0, 1, 0]),
            &mem,
        );
        assert_eq!(kind, RESP_OK_NODATA);
        let (kind, _) = gpu.control(&request(CMD_RESOURCE_FLUSH, &[0, 0, 4, 2, 1, 0]), &mem);
        assert_eq!(kind, RESP_OK_NODATA);
        let frame = display.frame();
        assert_eq!((frame.width, frame.height, frame.sequence), (4, 2, 1));
        assert_eq!(frame.pixels, pixels);
        let rgb = frame.to_rgb();
        assert_eq!(&rgb[..6], &[0, 0x10, 0x20, 1, 0x11, 0x21]);
        let ppm = frame.to_ppm();
        assert!(ppm.starts_with(b"P6\n4 2\n255\n"));
        assert_eq!(ppm.len(), 11 + 24);
    }

    #[test]
    fn a_partial_update_changes_only_its_rectangle() {
        let mem = GuestMemory::new(64 << 20).unwrap();
        let (mut gpu, display) = ready(&mem);
        // The guest redraws the 2x1 rectangle at (1,1): pixels 5 and 6,
        // which live 20 bytes into the backing, at the start of the second piece.
        mem.write(0x20000, &[9, 9, 9, 9, 8, 8, 8, 8]).unwrap();
        let (kind, _) = gpu.control(
            &request(CMD_TRANSFER_TO_HOST_2D, &[1, 1, 2, 1, 20, 0, 1, 0]),
            &mem,
        );
        assert_eq!(kind, RESP_OK_NODATA);
        gpu.control(&request(CMD_RESOURCE_FLUSH, &[1, 1, 2, 1, 1, 0]), &mem);
        let px = display.frame().pixels;
        assert_eq!(&px[20..28], &[9, 9, 9, 9, 8, 8, 8, 8]);
        assert!(px[..20].iter().all(|b| *b == 0) && px[28..].iter().all(|b| *b == 0));
    }

    #[test]
    fn only_the_scanout_rectangle_of_a_wider_resource_is_shown() {
        let mem = GuestMemory::new(64 << 20).unwrap();
        let display = Display::new(2, 2);
        let mut gpu = Gpu::new(display.clone());
        // A 4x2 buffer of which the left 2x2 is the screen, as when a guest
        // rounds its buffer width up.
        gpu.control(&request(CMD_RESOURCE_CREATE_2D, &[1, 67, 4, 2]), &mem);
        gpu.control(
            &request(CMD_RESOURCE_ATTACH_BACKING, &[1, 1, 0x10000, 0, 32, 0]),
            &mem,
        );
        gpu.control(&request(CMD_SET_SCANOUT, &[0, 0, 2, 2, 0, 1]), &mem);
        let pixels: Vec<u8> = (0..8u8).flat_map(|n| [n, n, n, 0xff]).collect();
        mem.write(0x10000, &pixels).unwrap();
        gpu.control(
            &request(CMD_TRANSFER_TO_HOST_2D, &[0, 0, 4, 2, 0, 0, 1, 0]),
            &mem,
        );
        gpu.control(&request(CMD_RESOURCE_FLUSH, &[0, 0, 4, 2, 1, 0]), &mem);
        let frame = display.frame();
        assert_eq!((frame.width, frame.height), (2, 2));
        let shown: Vec<u8> = frame.pixels.chunks(4).map(|p| p[0]).collect();
        assert_eq!(shown, [0, 1, 4, 5], "columns 2 and 3 are off screen");
        // An empty scanout rectangle is refused.
        let (kind, _) = gpu.control(&request(CMD_SET_SCANOUT, &[0, 0, 0, 2, 0, 1]), &mem);
        assert_eq!(kind, RESP_ERR_INVALID_PARAMETER);
    }

    #[test]
    fn bgra_is_reordered_for_rgb_output() {
        let frame = Frame {
            width: 1,
            height: 1,
            format: Format::Bgra,
            pixels: vec![0x33, 0x22, 0x11, 0xff],
            sequence: 1,
        };
        assert_eq!(frame.to_rgb(), [0x11, 0x22, 0x33]);
    }

    #[test]
    fn hostile_requests_get_errors_not_memory() {
        let mem = GuestMemory::new(64 << 20).unwrap();
        let (mut gpu, display) = ready(&mem);
        let kind = |gpu: &mut Gpu, r: Vec<u8>| gpu.control(&r, &mem).0;
        // Rectangles outside the resource, including ones that overflow.
        for rect in [
            [3, 0, 2, 1],
            [0, 1, 4, 2],
            [u32::MAX, 0, 2, 1],
            [0, 0, 5, 1],
        ] {
            let mut body = rect.to_vec();
            body.extend([0, 0, 1, 0]);
            assert_eq!(
                kind(&mut gpu, request(CMD_TRANSFER_TO_HOST_2D, &body)),
                RESP_ERR_INVALID_PARAMETER
            );
        }
        // An offset past the backing, and one that overflows.
        for offset in [[28, 0], [u32::MAX, u32::MAX]] {
            let r = request(
                CMD_TRANSFER_TO_HOST_2D,
                &[0, 0, 4, 2, offset[0], offset[1], 1, 0],
            );
            assert_eq!(kind(&mut gpu, r), RESP_ERR_UNSPEC);
        }
        // Backing that points outside guest memory.
        let r = request(CMD_RESOURCE_ATTACH_BACKING, &[1, 1, 0, 0xffff, 32, 0]);
        assert_eq!(kind(&mut gpu, r), RESP_OK_NODATA);
        let r = request(CMD_TRANSFER_TO_HOST_2D, &[0, 0, 4, 2, 0, 0, 1, 0]);
        assert_eq!(kind(&mut gpu, r), RESP_ERR_UNSPEC);
        // Unknown resources, scanouts, formats and commands; oversized things.
        assert_eq!(
            kind(&mut gpu, request(CMD_RESOURCE_FLUSH, &[0, 0, 1, 1, 7, 0])),
            RESP_ERR_INVALID_RESOURCE_ID
        );
        assert_eq!(
            kind(&mut gpu, request(CMD_SET_SCANOUT, &[0, 0, 4, 2, 3, 1])),
            RESP_ERR_INVALID_SCANOUT_ID
        );
        assert_eq!(
            kind(&mut gpu, request(CMD_RESOURCE_CREATE_2D, &[2, 999, 4, 4])),
            RESP_ERR_INVALID_PARAMETER
        );
        assert_eq!(
            kind(&mut gpu, request(CMD_RESOURCE_CREATE_2D, &[2, 67, 9000, 4])),
            RESP_ERR_INVALID_PARAMETER
        );
        assert_eq!(
            kind(&mut gpu, request(CMD_RESOURCE_CREATE_2D, &[1, 67, 4, 4])),
            RESP_ERR_INVALID_RESOURCE_ID,
            "id already in use"
        );
        assert_eq!(kind(&mut gpu, request(0x9999, &[])), RESP_ERR_UNSPEC);
        assert_eq!(kind(&mut gpu, vec![1, 2]), RESP_ERR_INVALID_PARAMETER);
        assert_eq!(
            kind(&mut gpu, request(CMD_RESOURCE_ATTACH_BACKING, &[1, 100])),
            RESP_ERR_INVALID_PARAMETER,
            "claims more entries than it sent"
        );
        // The host will not allocate without bound.
        let mut made = 0;
        for id in 10..40 {
            if kind(
                &mut gpu,
                request(CMD_RESOURCE_CREATE_2D, &[id, 67, 8192, 8192]),
            ) != RESP_OK_NODATA
            {
                break;
            }
            made += 1;
        }
        assert_eq!(
            made, 1,
            "256 MiB each against a 512 MiB cap, 32 bytes already used"
        );
        assert_eq!(display.sequence(), 0, "none of this presented anything");
        // Freeing gives the allowance back.
        assert_eq!(
            kind(&mut gpu, request(CMD_RESOURCE_UNREF, &[10, 0])),
            RESP_OK_NODATA
        );
        assert_eq!(
            kind(
                &mut gpu,
                request(CMD_RESOURCE_CREATE_2D, &[11, 67, 8192, 8192])
            ),
            RESP_OK_NODATA
        );
    }

    #[test]
    fn display_info_reports_one_enabled_screen() {
        let mem = GuestMemory::new(64 << 20).unwrap();
        let mut gpu = Gpu::new(Display::new(720, 1280));
        let (kind, body) = gpu.control(&request(CMD_GET_DISPLAY_INFO, &[]), &mem);
        assert_eq!(kind, RESP_OK_DISPLAY_INFO);
        assert_eq!(body.len(), 16 * 24);
        assert_eq!(u32_at(&body, 8), Some(720));
        assert_eq!(u32_at(&body, 12), Some(1280));
        assert_eq!(u32_at(&body, 16), Some(1));
        assert!(body[24..].iter().all(|b| *b == 0), "the other 15 are off");
        let mut cfg = [0u8; 16];
        gpu.read_config(0, &mut cfg);
        assert_eq!(u32_at(&cfg, 8), Some(1), "one scanout");
    }
}
