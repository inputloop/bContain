// SPDX-FileCopyrightText: 2026 Bryant Frankford <nathanielfrankford@gmail.com> and Contributors
// SPDX-License-Identifier: GPL-3.0-or-later OR LGPL-3.0-or-later
//! virtio-media: a camera for the guest.
//!
//! virtio-media carries Linux's own video interface (V4L2) across to the
//! host: the guest's driver forwards each `ioctl` on `/dev/videoN` as a
//! command, and the device answers as a camera driver would. This device is
//! one capture camera giving Motion-JPEG frames, which is what Android's
//! external-camera service accepts.
//!
//! Where frames come from is a [`FrameSource`]'s business: a real webcam,
//! or a test picture.
//!
//! Frames travel through memory the device owns, placed in the guest's
//! address space inside the device's shared-memory window (the guest
//! `mmap`s its buffers), and are announced by an event on the event queue.
//!
//! Written from the virtio-media guest driver's `protocol.h` and
//! `virtio_media_driver.c` / `virtio_media_ioctls.c`, and Linux's
//! `videodev2.h` for the layouts of the V4L2 structures.

use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Condvar, Mutex, OnceLock};
use std::time::{Duration, Instant};

use super::gpu::{u32_at, u64_at};
use super::queue::Queue;
use super::{VirtioDevice, TYPE_MEDIA};
use crate::memory::{GuestMemory, MapperSlot};

const Q_COMMAND: usize = 0;
const Q_EVENT: usize = 1;

const CMD_OPEN: u32 = 1;
const CMD_CLOSE: u32 = 2;
const CMD_IOCTL: u32 = 3;
const CMD_MMAP: u32 = 4;
const CMD_MUNMAP: u32 = 5;
const EVT_DQBUF: u32 = 1;

// The ioctls' numbers (`_IOC_NR` of each `VIDIOC_*`).
const ENUM_FMT: u32 = 2;
const G_FMT: u32 = 4;
const S_FMT: u32 = 5;
const REQBUFS: u32 = 8;
const QUERYBUF: u32 = 9;
const QBUF: u32 = 15;
const STREAMON: u32 = 18;
const STREAMOFF: u32 = 19;
const G_PARM: u32 = 21;
const S_PARM: u32 = 22;
const ENUMINPUT: u32 = 26;
const G_INPUT: u32 = 38;
const S_INPUT: u32 = 39;
const TRY_FMT: u32 = 64;
const ENUM_FRAMESIZES: u32 = 74;
const ENUM_FRAMEINTERVALS: u32 = 75;

// Error numbers, as Linux counts them.
const ENOMEM: u32 = 12;
const EBUSY: u32 = 16;
const EINVAL: u32 = 22;
const ENOTTY: u32 = 25;

const CAP_VIDEO_CAPTURE: u32 = 0x1;
const CAP_STREAMING: u32 = 0x0400_0000;
const BUF_TYPE_VIDEO_CAPTURE: u32 = 1;
const MEMORY_MMAP: u32 = 1;
/// `V4L2_PIX_FMT_MJPEG`: the four characters "MJPG".
const MJPEG: u32 = u32::from_le_bytes(*b"MJPG");
const FIELD_NONE: u32 = 1;
const COLORSPACE_JPEG: u32 = 7;
const BUF_FLAG_MAPPED: u32 = 0x1;
const BUF_FLAG_QUEUED: u32 = 0x2;
const BUF_FLAG_DONE: u32 = 0x4;
const BUF_FLAG_TIMESTAMP_MONOTONIC: u32 = 0x2000;

/// Sizes of the V4L2 structures on a 64-bit guest.
const SIZEOF_BUFFER: usize = 88;
const SIZEOF_PLANE: usize = 64;
const SIZEOF_FORMAT: usize = 208;
const SIZEOF_STREAMPARM: usize = 204;
const MAX_PLANES: usize = 8;

const PAGE: usize = 4096;
/// The shared-memory window: id 0 ("mmap"), 256 MiB of guest address space.
const SHM_ID: u8 = 0;
const SHM_SIZE: u64 = 256 << 20;
const MAX_BUFFERS: u32 = 16;
const MAX_SESSIONS: usize = 16;

/// A picture size and rate a camera offers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Mode {
    pub width: u32,
    pub height: u32,
    pub fps: u32,
}

/// Where the camera's frames come from. Calls come from the thread running
/// the guest and must not block for long.
pub trait FrameSource: Send {
    /// What the camera offers; the first is the default. Never empty.
    fn modes(&self) -> Vec<Mode>;
    /// The guest started the camera in this mode.
    fn start(&mut self, mode: Mode);
    /// The newest frame, as one complete JPEG picture, if there is a new
    /// one since the last call.
    fn frame(&mut self) -> Option<Vec<u8>>;
    fn stop(&mut self);
}

/// Memory for one frame buffer: whole pages, so it can be placed in the
/// guest's address space.
struct Pages {
    ptr: *mut u8,
    len: usize,
}

// SAFETY: the allocation is owned and only touched under the device's lock.
unsafe impl Send for Pages {}

impl Pages {
    fn new(len: usize) -> Option<Pages> {
        let layout = std::alloc::Layout::from_size_align(len, PAGE).ok()?;
        // SAFETY: the layout has a non-zero size (callers pass at least a page).
        let ptr = unsafe { std::alloc::alloc_zeroed(layout) };
        (!ptr.is_null()).then_some(Pages { ptr, len })
    }

    fn fill(&mut self, data: &[u8]) -> usize {
        let n = data.len().min(self.len);
        // SAFETY: `n` bytes fit in both; the regions do not overlap.
        unsafe { std::ptr::copy_nonoverlapping(data.as_ptr(), self.ptr, n) };
        n
    }
}

impl Drop for Pages {
    fn drop(&mut self) {
        if let Ok(layout) = std::alloc::Layout::from_size_align(self.len, PAGE) {
            // SAFETY: allocated in `new` with this layout.
            unsafe { std::alloc::dealloc(self.ptr, layout) };
        }
    }
}

struct Buffer {
    pages: Pages,
    queued: bool,
    /// Where in the window it is mapped, if it is.
    mapped: Option<u64>,
}

struct Session {
    mode: Mode,
    buffers: Vec<Buffer>,
    /// Buffers the guest has handed over to be filled, oldest first.
    waiting: VecDeque<u32>,
    streaming: bool,
    sequence: u32,
}

struct Inner {
    source: Box<dyn FrameSource>,
    modes: Vec<Mode>,
    sessions: HashMap<u32, Session>,
    next_session: u32,
    /// Which session has the camera running.
    streaming: Option<u32>,
    /// Buffers freed while still mapped: kept until the guest unmaps them.
    /// Keyed by where in the window they are.
    orphans: HashMap<u64, Pages>,
    /// Where the next mapping goes in the window. Addresses are not
    /// reused; the window is large and a camera maps a handful of buffers.
    next_address: u64,
    next_frame: Option<Instant>,
    started: Instant,
    mapper: MapperSlot,
    shm_base: u64,
}

struct Shared {
    inner: Mutex<Inner>,
    changed: Condvar,
    wake: OnceLock<Box<dyn Fn() + Send + Sync>>,
}

#[derive(Clone)]
pub struct Camera {
    shared: Arc<Shared>,
    name: [u8; 32],
}

fn put32(b: &mut [u8], at: usize, v: u32) {
    if let Some(slot) = b.get_mut(at..at + 4) {
        slot.copy_from_slice(&v.to_le_bytes());
    }
}

/// The bytes one frame's buffer needs: a JPEG is far smaller than its
/// picture, so the picture's size in bytes at one byte per pixel is ample.
fn buffer_bytes(mode: Mode) -> usize {
    ((mode.width as usize * mode.height as usize).max(PAGE)).div_ceil(PAGE) * PAGE
}

impl Camera {
    /// A camera called `name`, with `source` behind it.
    pub fn new(name: &str, source: Box<dyn FrameSource>) -> Camera {
        let mut card = [0u8; 32];
        let bytes = name.as_bytes();
        let n = bytes.len().min(31);
        card[..n].copy_from_slice(&bytes[..n]);
        let modes = source.modes();
        Camera {
            name: card,
            shared: Arc::new(Shared {
                inner: Mutex::new(Inner {
                    source,
                    modes,
                    sessions: HashMap::new(),
                    next_session: 1,
                    streaming: None,
                    orphans: HashMap::new(),
                    next_address: 0,
                    next_frame: None,
                    started: Instant::now(),
                    mapper: MapperSlot::default(),
                    shm_base: 0,
                }),
                changed: Condvar::new(),
                wake: OnceLock::new(),
            }),
        }
    }

    /// Give the device the machine's way of placing memory in the guest.
    pub(crate) fn set_mapper(&self, mapper: MapperSlot) {
        self.shared.inner.lock().unwrap().mapper = mapper;
    }

    /// Have the device ask `wake` for a `poll` when a frame is due, and
    /// start the thread that keeps the time.
    pub(crate) fn set_wake(&self, wake: Box<dyn Fn() + Send + Sync>) {
        if self.shared.wake.set(wake).is_err() {
            return;
        }
        let shared = Arc::downgrade(&self.shared);
        std::thread::spawn(move || loop {
            let Some(shared) = shared.upgrade() else {
                return;
            };
            let inner = shared.inner.lock().unwrap();
            match inner.next_frame {
                None => {
                    let _ = shared.changed.wait_timeout(inner, Duration::from_secs(1));
                }
                Some(at) => {
                    drop(inner);
                    std::thread::sleep(at.saturating_duration_since(Instant::now()));
                    if let Some(wake) = shared.wake.get() {
                        wake();
                    }
                }
            }
        });
    }
}

/// An answer: the status, then the payload.
fn answer(status: u32, payload: &[u8]) -> Vec<u8> {
    let mut out = vec![0u8; 8];
    put32(&mut out, 0, status);
    if status == 0 {
        out.extend_from_slice(payload);
    }
    out
}

impl Inner {
    fn mode_for(&self, width: u32, height: u32) -> Mode {
        *self
            .modes
            .iter()
            .find(|m| m.width == width && m.height == height)
            .unwrap_or(&self.modes[0])
    }

    /// `struct v4l2_format` for a capture of `mode`.
    fn format(mode: Mode) -> Vec<u8> {
        let mut f = vec![0u8; SIZEOF_FORMAT];
        put32(&mut f, 0, BUF_TYPE_VIDEO_CAPTURE);
        // The union starts at 8 (it holds pointers): struct v4l2_pix_format.
        put32(&mut f, 8, mode.width);
        put32(&mut f, 12, mode.height);
        put32(&mut f, 16, MJPEG);
        put32(&mut f, 20, FIELD_NONE);
        put32(&mut f, 28, buffer_bytes(mode) as u32); // sizeimage
        put32(&mut f, 32, COLORSPACE_JPEG);
        f
    }

    /// `struct v4l2_buffer` describing buffer `index` of `session`.
    fn buffer(session: &Session, index: u32, extra_flags: u32) -> Vec<u8> {
        let mut b = vec![0u8; SIZEOF_BUFFER];
        let Some(buf) = session.buffers.get(index as usize) else {
            return b;
        };
        let mut flags = BUF_FLAG_TIMESTAMP_MONOTONIC | extra_flags;
        if buf.mapped.is_some() {
            flags |= BUF_FLAG_MAPPED;
        }
        if buf.queued {
            flags |= BUF_FLAG_QUEUED;
        }
        put32(&mut b, 0, index);
        put32(&mut b, 4, BUF_TYPE_VIDEO_CAPTURE);
        put32(&mut b, 12, flags);
        put32(&mut b, 16, FIELD_NONE);
        put32(&mut b, 60, MEMORY_MMAP);
        // m.offset: what the guest passes to mmap to name this buffer.
        put32(&mut b, 64, index << 24);
        put32(&mut b, 72, buf.pages.len as u32);
        b
    }

    /// Take a session's buffers away. Those still mapped in the guest are
    /// kept alive until it unmaps them.
    fn free_buffers(&mut self, id: u32) {
        let Some(session) = self.sessions.get_mut(&id) else {
            return;
        };
        session.waiting.clear();
        for buffer in session.buffers.drain(..) {
            if let Some(at) = buffer.mapped {
                self.orphans.insert(at, buffer.pages);
            }
        }
    }

    fn stop(&mut self, id: u32) {
        if self.streaming == Some(id) {
            self.streaming = None;
            self.next_frame = None;
            self.source.stop();
        }
        if let Some(session) = self.sessions.get_mut(&id) {
            session.streaming = false;
            session.waiting.clear();
            for b in &mut session.buffers {
                b.queued = false;
            }
        }
    }

    fn ioctl(&mut self, id: u32, code: u32, payload: &[u8]) -> Vec<u8> {
        if !self.sessions.contains_key(&id) {
            return answer(EINVAL, &[]);
        }
        // A capture camera has one kind of buffer queue.
        let capture = |at: usize| u32_at(payload, at) == Some(BUF_TYPE_VIDEO_CAPTURE);
        match code {
            ENUM_FMT => {
                // struct v4l2_fmtdesc (64): index, type, flags, description[32], pixelformat.
                if payload.len() < 64 || !capture(4) || u32_at(payload, 0) != Some(0) {
                    return answer(EINVAL, &[]);
                }
                let mut f = payload[..64].to_vec();
                put32(&mut f, 8, 1); // V4L2_FMT_FLAG_COMPRESSED
                f[12..44].fill(0);
                f[12..23].copy_from_slice(b"Motion-JPEG");
                put32(&mut f, 44, MJPEG);
                answer(0, &f)
            }
            G_FMT => {
                if !capture(0) {
                    return answer(EINVAL, &[]);
                }
                answer(0, &Self::format(self.sessions[&id].mode))
            }
            S_FMT | TRY_FMT => {
                let (Some(width), Some(height)) = (u32_at(payload, 8), u32_at(payload, 12)) else {
                    return answer(EINVAL, &[]);
                };
                if !capture(0) {
                    return answer(EINVAL, &[]);
                }
                // The nearest thing offered, as V4L2 drivers do.
                let mode = self.mode_for(width, height);
                if code == S_FMT {
                    let session = self.sessions.get_mut(&id).expect("checked above");
                    if session.streaming || !session.buffers.is_empty() {
                        return answer(EBUSY, &[]);
                    }
                    session.mode = Mode {
                        fps: session.mode.fps,
                        ..mode
                    };
                }
                answer(0, &Self::format(mode))
            }
            ENUM_FRAMESIZES => {
                // struct v4l2_frmsizeenum (44): index, pixel_format, type, {width, height}.
                let (Some(index), Some(format)) = (u32_at(payload, 0), u32_at(payload, 4)) else {
                    return answer(EINVAL, &[]);
                };
                let mut sizes: Vec<(u32, u32)> = Vec::new();
                for m in &self.modes {
                    if !sizes.contains(&(m.width, m.height)) {
                        sizes.push((m.width, m.height));
                    }
                }
                let Some((w, h)) = sizes.get(index as usize).filter(|_| format == MJPEG) else {
                    return answer(EINVAL, &[]);
                };
                let mut f = vec![0u8; 44];
                put32(&mut f, 0, index);
                put32(&mut f, 4, MJPEG);
                put32(&mut f, 8, 1); // V4L2_FRMSIZE_TYPE_DISCRETE
                put32(&mut f, 12, *w);
                put32(&mut f, 16, *h);
                answer(0, &f)
            }
            ENUM_FRAMEINTERVALS => {
                // struct v4l2_frmivalenum (52): index, pixel_format, width,
                // height, type, {numerator, denominator}.
                let (Some(index), Some(format), Some(w), Some(h)) = (
                    u32_at(payload, 0),
                    u32_at(payload, 4),
                    u32_at(payload, 8),
                    u32_at(payload, 12),
                ) else {
                    return answer(EINVAL, &[]);
                };
                let rates: Vec<u32> = self
                    .modes
                    .iter()
                    .filter(|m| m.width == w && m.height == h)
                    .map(|m| m.fps)
                    .collect();
                let Some(fps) = rates.get(index as usize).filter(|_| format == MJPEG) else {
                    return answer(EINVAL, &[]);
                };
                let mut f = vec![0u8; 52];
                put32(&mut f, 0, index);
                put32(&mut f, 4, MJPEG);
                put32(&mut f, 8, w);
                put32(&mut f, 12, h);
                put32(&mut f, 16, 1); // V4L2_FRMIVAL_TYPE_DISCRETE
                put32(&mut f, 20, 1);
                put32(&mut f, 24, *fps);
                answer(0, &f)
            }
            G_PARM | S_PARM => {
                // struct v4l2_streamparm (204): type, then v4l2_captureparm:
                // capability, capturemode, timeperframe {numerator, denominator}.
                if !capture(0) {
                    return answer(EINVAL, &[]);
                }
                let session = self.sessions.get_mut(&id).expect("checked above");
                if code == S_PARM {
                    let (n, d) = (
                        u32_at(payload, 12).unwrap_or(0),
                        u32_at(payload, 16).unwrap_or(0),
                    );
                    let wanted = d.checked_div(n).unwrap_or(0);
                    let offered: Vec<u32> = self
                        .modes
                        .iter()
                        .filter(|m| {
                            m.width == session.mode.width && m.height == session.mode.height
                        })
                        .map(|m| m.fps)
                        .collect();
                    // The offered rate nearest what was asked.
                    if let Some(fps) = offered.iter().min_by_key(|f| f.abs_diff(wanted)) {
                        session.mode.fps = *fps;
                    }
                }
                let mut p = vec![0u8; SIZEOF_STREAMPARM];
                put32(&mut p, 0, BUF_TYPE_VIDEO_CAPTURE);
                put32(&mut p, 4, 0x1000); // V4L2_CAP_TIMEPERFRAME
                put32(&mut p, 12, 1);
                put32(&mut p, 16, session.mode.fps.max(1));
                answer(0, &p)
            }
            REQBUFS => {
                // struct v4l2_requestbuffers (20): count, type, memory, capabilities.
                let (Some(count), Some(memory)) = (u32_at(payload, 0), u32_at(payload, 8)) else {
                    return answer(EINVAL, &[]);
                };
                if !capture(4) || memory != MEMORY_MMAP {
                    return answer(EINVAL, &[]);
                }
                if self.sessions[&id].streaming {
                    return answer(EBUSY, &[]);
                }
                self.free_buffers(id);
                let count = count.min(MAX_BUFFERS);
                let bytes = buffer_bytes(self.sessions[&id].mode);
                let mut made = Vec::new();
                for _ in 0..count {
                    match Pages::new(bytes) {
                        Some(pages) => made.push(Buffer {
                            pages,
                            queued: false,
                            mapped: None,
                        }),
                        None => return answer(ENOMEM, &[]),
                    }
                }
                self.sessions.get_mut(&id).expect("checked above").buffers = made;
                let mut r = payload[..20.min(payload.len())].to_vec();
                r.resize(20, 0);
                put32(&mut r, 0, count);
                put32(&mut r, 12, 0x1); // V4L2_BUF_CAP_SUPPORTS_MMAP
                answer(0, &r)
            }
            QUERYBUF | QBUF => {
                // struct v4l2_buffer: index, type.
                let Some(index) = u32_at(payload, 0) else {
                    return answer(EINVAL, &[]);
                };
                let session = self.sessions.get_mut(&id).expect("checked above");
                if !capture(4) || index as usize >= session.buffers.len() {
                    return answer(EINVAL, &[]);
                }
                if code == QBUF && !session.buffers[index as usize].queued {
                    session.buffers[index as usize].queued = true;
                    session.waiting.push_back(index);
                }
                answer(0, &Self::buffer(session, index, 0))
            }
            STREAMON => {
                if !capture(0) {
                    return answer(EINVAL, &[]);
                }
                if self.streaming.is_some_and(|other| other != id) {
                    return answer(EBUSY, &[]); // one user of the camera at a time
                }
                if self.streaming.is_none() {
                    let mode = self.sessions[&id].mode;
                    self.source.start(mode);
                    self.streaming = Some(id);
                    self.next_frame = Some(Instant::now());
                    self.sessions.get_mut(&id).expect("checked above").streaming = true;
                }
                answer(0, &[])
            }
            STREAMOFF => {
                if !capture(0) {
                    return answer(EINVAL, &[]);
                }
                self.stop(id);
                answer(0, &[])
            }
            ENUMINPUT => {
                // struct v4l2_input (80): index, name[32], type.
                if u32_at(payload, 0) != Some(0) {
                    return answer(EINVAL, &[]);
                }
                let mut i = vec![0u8; 80];
                i[4..10].copy_from_slice(b"Camera");
                put32(&mut i, 36, 2); // V4L2_INPUT_TYPE_CAMERA
                answer(0, &i)
            }
            G_INPUT => answer(0, &0u32.to_le_bytes()),
            S_INPUT => match u32_at(payload, 0) {
                Some(0) => answer(0, &0u32.to_le_bytes()),
                _ => answer(EINVAL, &[]),
            },
            // Controls, standards, tuners and the rest: this camera has none.
            _ => answer(ENOTTY, &[]),
        }
    }

    fn command(&mut self, req: &[u8]) -> Vec<u8> {
        let Some(cmd) = u32_at(req, 0) else {
            return answer(EINVAL, &[]);
        };
        match cmd {
            CMD_OPEN => {
                if self.sessions.len() >= MAX_SESSIONS {
                    return answer(EBUSY, &[]);
                }
                let id = self.next_session;
                self.next_session = self.next_session.wrapping_add(1).max(1);
                self.sessions.insert(
                    id,
                    Session {
                        mode: self.modes[0],
                        buffers: Vec::new(),
                        waiting: VecDeque::new(),
                        streaming: false,
                        sequence: 0,
                    },
                );
                let mut out = [0u8; 8];
                put32(&mut out, 0, id);
                answer(0, &out)
            }
            CMD_CLOSE => {
                let Some(id) = u32_at(req, 8) else {
                    return answer(EINVAL, &[]);
                };
                self.stop(id);
                self.free_buffers(id);
                self.sessions.remove(&id);
                answer(0, &[])
            }
            CMD_IOCTL => {
                let (Some(id), Some(code)) = (u32_at(req, 8), u32_at(req, 12)) else {
                    return answer(EINVAL, &[]);
                };
                self.ioctl(id, code, req.get(16..).unwrap_or(&[]))
            }
            CMD_MMAP => {
                // session_id, flags, offset: the buffer's `m.offset`.
                let (Some(id), Some(offset)) = (u32_at(req, 8), u32_at(req, 16)) else {
                    return answer(EINVAL, &[]);
                };
                let index = (offset >> 24) as usize;
                let (base, at) = (self.shm_base, self.next_address);
                let Some(buffer) = self
                    .sessions
                    .get_mut(&id)
                    .and_then(|s| s.buffers.get_mut(index))
                    .filter(|b| b.mapped.is_none() && offset & 0x00ff_ffff == 0)
                else {
                    return answer(EINVAL, &[]);
                };
                let len = buffer.pages.len as u64;
                let Some(mapper) = self.mapper.get() else {
                    return answer(ENOMEM, &[]);
                };
                if at + len > SHM_SIZE {
                    return answer(ENOMEM, &[]);
                }
                // SAFETY: the pages live until they are unmapped: a buffer
                // freed while mapped is kept in `orphans` (see free_buffers).
                if unsafe { mapper.map(base + at, buffer.pages.ptr, len) }.is_err() {
                    return answer(ENOMEM, &[]);
                }
                buffer.mapped = Some(at);
                self.next_address = at + len;
                let mut out = [0u8; 16];
                out[0..8].copy_from_slice(&at.to_le_bytes());
                out[8..16].copy_from_slice(&len.to_le_bytes());
                answer(0, &out)
            }
            CMD_MUNMAP => {
                let Some(at) = u64_at(req, 8) else {
                    return answer(EINVAL, &[]);
                };
                let mut len = self.orphans.remove(&at).map(|pages| pages.len as u64);
                if len.is_none() {
                    for session in self.sessions.values_mut() {
                        for buffer in &mut session.buffers {
                            if buffer.mapped == Some(at) {
                                buffer.mapped = None;
                                len = Some(buffer.pages.len as u64);
                            }
                        }
                    }
                }
                let Some(len) = len else {
                    return answer(EINVAL, &[]);
                };
                if let Some(mapper) = self.mapper.get() {
                    let _ = mapper.unmap(self.shm_base + at, len);
                }
                answer(0, &[])
            }
            _ => answer(ENOTTY, &[]),
        }
    }

    /// If a frame is due and the guest has a buffer waiting, fill it and
    /// tell the guest. False if nothing was sent.
    fn deliver(&mut self, events: &mut Queue, mem: &GuestMemory) -> bool {
        let (Some(id), Some(due)) = (self.streaming, self.next_frame) else {
            return false;
        };
        let now = Instant::now();
        if due > now {
            return false;
        }
        let fps = self.sessions.get(&id).map_or(30, |s| s.mode.fps.max(1));
        self.next_frame = Some(now + Duration::from_secs(1) / fps);
        let stamp = self.started.elapsed();
        let Some(session) = self.sessions.get_mut(&id) else {
            return false;
        };
        // No buffer from the guest, or nowhere to put the notice: the
        // frame is skipped, as a camera drops frames nobody collects.
        if session.waiting.is_empty() {
            return false;
        }
        let Some(frame) = self.source.frame() else {
            return false;
        };
        let Ok(Some(chain)) = events.pop(mem) else {
            return false;
        };
        let index = session.waiting.pop_front().expect("checked above");
        let buffer = &mut session.buffers[index as usize];
        buffer.queued = false;
        let used = buffer.pages.fill(&frame);
        session.sequence = session.sequence.wrapping_add(1);
        // struct virtio_media_event_dqbuf: header, v4l2_buffer, planes.
        let mut event = vec![0u8; 8];
        put32(&mut event, 0, EVT_DQBUF);
        put32(&mut event, 4, id);
        let mut b = Self::buffer(session, index, BUF_FLAG_DONE);
        put32(&mut b, 8, used as u32); // bytesused
        b[24..32].copy_from_slice(&(stamp.as_secs() as i64).to_le_bytes());
        b[32..40].copy_from_slice(&(stamp.subsec_micros() as i64).to_le_bytes());
        put32(&mut b, 56, session.sequence);
        event.extend_from_slice(&b);
        event.resize(8 + SIZEOF_BUFFER + MAX_PLANES * SIZEOF_PLANE, 0);
        let written = chain.write_response(mem, &event).unwrap_or(0);
        events.push_used(mem, chain.head, written).is_ok()
    }
}

impl VirtioDevice for Camera {
    fn device_type(&self) -> u32 {
        TYPE_MEDIA
    }

    fn features(&self) -> u64 {
        0
    }

    fn queue_max_sizes(&self) -> Vec<u16> {
        vec![64, 64] // command, event
    }

    fn read_config(&self, offset: u64, data: &mut [u8]) {
        // device_caps, device_type (0: a video device), card[32].
        let mut cfg = [0u8; 40];
        put32(&mut cfg, 0, CAP_VIDEO_CAPTURE | CAP_STREAMING);
        cfg[8..40].copy_from_slice(&self.name);
        for (i, b) in data.iter_mut().enumerate() {
            *b = cfg.get(offset as usize + i).copied().unwrap_or(0);
        }
    }

    fn notify(&mut self, index: usize, queue: &mut Queue, mem: &GuestMemory) -> bool {
        if index != Q_COMMAND {
            return false; // event buffers wait in their queue for `poll`
        }
        let mut inner = self.shared.inner.lock().unwrap();
        let mut any = false;
        while let Ok(Some(chain)) = queue.pop(mem) {
            let req = chain.read_request(mem, 1024).unwrap_or_default();
            let reply = inner.command(&req);
            let written = chain.write_response(mem, &reply).unwrap_or(0);
            if queue.push_used(mem, chain.head, written).is_err() {
                break;
            }
            any = true;
        }
        drop(inner);
        self.shared.changed.notify_all();
        any
    }

    fn poll(&mut self, queues: &mut [Queue], mem: &GuestMemory) -> Vec<usize> {
        let Some(events) = queues.get_mut(Q_EVENT) else {
            return Vec::new();
        };
        if self.shared.inner.lock().unwrap().deliver(events, mem) {
            vec![Q_EVENT]
        } else {
            Vec::new()
        }
    }

    fn reset(&mut self) {
        let mut inner = self.shared.inner.lock().unwrap();
        let ids: Vec<u32> = inner.sessions.keys().copied().collect();
        for id in ids {
            inner.stop(id);
            inner.free_buffers(id);
        }
        inner.sessions.clear();
        let (base, mapped): (u64, Vec<(u64, u64)>) = (
            inner.shm_base,
            inner
                .orphans
                .iter()
                .map(|(at, p)| (*at, p.len as u64))
                .collect(),
        );
        if let Some(mapper) = inner.mapper.get() {
            for (at, len) in mapped {
                let _ = mapper.unmap(base + at, len);
            }
        }
        inner.orphans.clear();
    }

    fn shm_region(&self) -> Option<(u8, u64)> {
        Some((SHM_ID, SHM_SIZE))
    }

    fn set_shm_base(&mut self, gpa: u64) {
        self.shared.inner.lock().unwrap().shm_base = gpa;
    }
}

#[cfg(test)]
mod tests {
    use super::super::queue::tests::{publish, put_desc, queue, USED};
    use super::*;

    const WRITE: u16 = 2;
    const NEXT: u16 = 1;

    /// A camera that numbers its frames.
    struct Counting {
        log: Arc<Mutex<Vec<String>>>,
        n: u8,
    }

    impl FrameSource for Counting {
        fn modes(&self) -> Vec<Mode> {
            vec![
                Mode {
                    width: 640,
                    height: 480,
                    fps: 30,
                },
                Mode {
                    width: 640,
                    height: 480,
                    fps: 15,
                },
                Mode {
                    width: 1280,
                    height: 720,
                    fps: 30,
                },
            ]
        }
        fn start(&mut self, mode: Mode) {
            self.log
                .lock()
                .unwrap()
                .push(format!("start {}x{}@{}", mode.width, mode.height, mode.fps));
        }
        fn frame(&mut self) -> Option<Vec<u8>> {
            self.n += 1;
            Some(vec![self.n; 1000])
        }
        fn stop(&mut self) {
            self.log.lock().unwrap().push("stop".into());
        }
    }

    /// Remembers what was placed where, and what the memory held.
    #[derive(Default)]
    struct Maps(Arc<Mutex<Vec<(u64, usize, u64)>>>);

    impl crate::memory::Mapper for Maps {
        unsafe fn map(&self, gpa: u64, host: *mut u8, len: u64) -> crate::Result<()> {
            assert!((host as usize).is_multiple_of(PAGE) && len.is_multiple_of(PAGE as u64));
            self.0.lock().unwrap().push((gpa, host as usize, len));
            Ok(())
        }
        fn unmap(&self, gpa: u64, _len: u64) -> crate::Result<()> {
            self.0.lock().unwrap().retain(|(at, _, _)| *at != gpa);
            Ok(())
        }
    }

    struct Rig {
        camera: Camera,
        log: Arc<Mutex<Vec<String>>>,
        maps: Arc<Mutex<Vec<(u64, usize, u64)>>>,
        mem: GuestMemory,
        q: Queue,
        sent: u16,
    }

    const BASE: u64 = 0x90_0000_0000;

    fn rig() -> Rig {
        let log = Arc::new(Mutex::new(Vec::new()));
        let maps = Arc::new(Mutex::new(Vec::new()));
        let mut camera = Camera::new(
            "Test camera",
            Box::new(Counting {
                log: log.clone(),
                n: 0,
            }),
        );
        let slot = MapperSlot::default();
        let _ = slot.set(Box::new(Maps(maps.clone())));
        camera.set_mapper(slot);
        camera.set_shm_base(BASE);
        Rig {
            camera,
            log,
            maps,
            mem: GuestMemory::new(64 << 20).unwrap(),
            q: queue(64),
            sent: 0,
        }
    }

    fn words(w: &[u32]) -> Vec<u8> {
        w.iter().flat_map(|v| v.to_le_bytes()).collect()
    }

    impl Rig {
        /// Send a command; returns (status, payload).
        fn send(&mut self, req: &[u8]) -> (u32, Vec<u8>) {
            let n = self.sent;
            let (out, back) = (0x10_0000 + n as u64 * 0x2000, 0x10_1000 + n as u64 * 0x2000);
            self.mem.write(out, req).unwrap();
            put_desc(&self.mem, n * 2, out, req.len() as u32, NEXT, n * 2 + 1);
            put_desc(&self.mem, n * 2 + 1, back, 0x1000, WRITE, 0);
            publish(&self.mem, 64, n, n * 2);
            self.sent += 1;
            assert!(self.camera.notify(Q_COMMAND, &mut self.q, &self.mem));
            let mut answer = vec![0u8; 512];
            self.mem.read(back, &mut answer).unwrap();
            (u32_at(&answer, 0).unwrap(), answer[8..].to_vec())
        }

        fn ioctl(&mut self, session: u32, code: u32, payload: &[u8]) -> (u32, Vec<u8>) {
            let mut req = words(&[CMD_IOCTL, 0, session, code]);
            req.extend_from_slice(payload);
            self.send(&req)
        }

        fn open(&mut self) -> u32 {
            let (status, p) = self.send(&words(&[CMD_OPEN, 0]));
            assert_eq!(status, 0);
            u32_at(&p, 0).unwrap()
        }
    }

    fn format(width: u32, height: u32) -> Vec<u8> {
        let mut f = vec![0u8; SIZEOF_FORMAT];
        put32(&mut f, 0, BUF_TYPE_VIDEO_CAPTURE);
        put32(&mut f, 8, width);
        put32(&mut f, 12, height);
        f
    }

    #[test]
    fn the_guest_is_told_what_the_camera_offers() {
        let mut r = rig();
        let mut config = [0u8; 40];
        r.camera.read_config(0, &mut config);
        assert_eq!(u32_at(&config, 0), Some(0x0400_0001), "capture, streaming");
        assert_eq!(&config[8..19], b"Test camera");
        assert_eq!(r.camera.shm_region(), Some((0, 256 << 20)));
        let s = r.open();
        // One format: Motion-JPEG.
        let mut desc = vec![0u8; 64];
        put32(&mut desc, 4, BUF_TYPE_VIDEO_CAPTURE);
        let (status, f) = r.ioctl(s, ENUM_FMT, &desc);
        assert_eq!((status, &f[44..48]), (0, &b"MJPG"[..]));
        put32(&mut desc, 0, 1);
        assert_eq!(r.ioctl(s, ENUM_FMT, &desc).0, EINVAL, "no second format");
        // Two sizes, each listed once though one has two rates.
        let size = |r: &mut Rig, index: u32| {
            let (status, f) = r.ioctl(
                s,
                ENUM_FRAMESIZES,
                &words(&[index, MJPEG, 0, 0, 0, 0, 0, 0, 0, 0, 0]),
            );
            (
                status,
                u32_at(&f, 12).unwrap_or(0),
                u32_at(&f, 16).unwrap_or(0),
            )
        };
        assert_eq!(
            (size(&mut r, 0), size(&mut r, 1), size(&mut r, 2).0),
            ((0, 640, 480), (0, 1280, 720), EINVAL)
        );
        let rate = |r: &mut Rig, index: u32, w: u32, h: u32| {
            let (status, f) = r.ioctl(
                s,
                ENUM_FRAMEINTERVALS,
                &words(&[index, MJPEG, w, h, 0, 0, 0, 0, 0, 0, 0, 0, 0]),
            );
            (
                status,
                u32_at(&f, 20).unwrap_or(0),
                u32_at(&f, 24).unwrap_or(0),
            )
        };
        assert_eq!(
            (rate(&mut r, 0, 640, 480), rate(&mut r, 1, 640, 480)),
            ((0, 1, 30), (0, 1, 15))
        );
        assert_eq!(rate(&mut r, 1, 1280, 720).0, EINVAL);
        // Asking for a size it lacks gets the default; a size it has is kept.
        let (_, f) = r.ioctl(s, S_FMT, &format(1280, 720));
        assert_eq!(
            (u32_at(&f, 8), u32_at(&f, 12), u32_at(&f, 16)),
            (Some(1280), Some(720), Some(MJPEG))
        );
        let (_, f) = r.ioctl(s, TRY_FMT, &format(999, 999));
        assert_eq!((u32_at(&f, 8), u32_at(&f, 12)), (Some(640), Some(480)));
        let (_, f) = r.ioctl(s, G_FMT, &format(0, 0));
        assert_eq!(u32_at(&f, 8), Some(1280), "trying did not change it");
        // What a camera does not have is "no such ioctl", not a crash.
        assert_eq!(r.ioctl(s, 36, &[0; 68]).0, ENOTTY);
        assert_eq!(
            r.ioctl(77, G_FMT, &format(0, 0)).0,
            EINVAL,
            "no such session"
        );
    }

    #[test]
    fn frames_reach_the_guest_through_its_mapped_buffers() {
        let mut r = rig();
        let s = r.open();
        // Two buffers, memory-mapped.
        let (status, b) = r.ioctl(
            s,
            REQBUFS,
            &words(&[2, BUF_TYPE_VIDEO_CAPTURE, MEMORY_MMAP, 0, 0]),
        );
        assert_eq!((status, u32_at(&b, 0)), (0, Some(2)));
        let mut query = vec![0u8; SIZEOF_BUFFER];
        put32(&mut query, 0, 1);
        put32(&mut query, 4, BUF_TYPE_VIDEO_CAPTURE);
        let (_, b) = r.ioctl(s, QUERYBUF, &query);
        let (offset, length) = (u32_at(&b, 64).unwrap(), u32_at(&b, 72).unwrap());
        assert_eq!(
            (offset, length),
            (1 << 24, 307_200),
            "640x480, a whole number of pages"
        );
        // The guest maps buffer 1: it lands in the window, page-aligned.
        let (status, m) = r.send(&words(&[CMD_MMAP, 0, s, 1, offset]));
        let (at, len) = (u64_at(&m, 0).unwrap(), u64_at(&m, 8).unwrap());
        assert_eq!((status, at, len), (0, 0, 307_200));
        let (gpa, host, _) = r.maps.lock().unwrap()[0];
        assert_eq!(gpa, BASE);
        assert_eq!(
            r.send(&words(&[CMD_MMAP, 0, s, 1, offset])).0,
            EINVAL,
            "already mapped"
        );
        // Queue it and start the camera.
        let (_, b) = r.ioctl(s, QBUF, &query);
        assert_eq!(
            u32_at(&b, 12).unwrap() & (BUF_FLAG_QUEUED | BUF_FLAG_MAPPED),
            3
        );
        assert_eq!(r.ioctl(s, STREAMON, &words(&[BUF_TYPE_VIDEO_CAPTURE])).0, 0);
        assert_eq!(*r.log.lock().unwrap(), vec!["start 640x480@30"]);
        let other = r.open();
        assert_eq!(
            r.ioctl(other, STREAMON, &words(&[BUF_TYPE_VIDEO_CAPTURE]))
                .0,
            EBUSY,
            "one user at a time"
        );

        // An event buffer from the guest; the frame is due at once.
        let events = GuestMemory::new(64 << 20).unwrap();
        let mut queues = vec![queue(8), queue(8)];
        put_desc(&events, 0, 0x40_0000, 1024, WRITE, 0);
        publish(&events, 8, 0, 0);
        assert_eq!(r.camera.poll(&mut queues, &events), vec![Q_EVENT]);
        let mut e = vec![0u8; 8 + SIZEOF_BUFFER];
        events.read(0x40_0000, &mut e).unwrap();
        assert_eq!((u32_at(&e, 0), u32_at(&e, 4)), (Some(EVT_DQBUF), Some(s)));
        let b = &e[8..];
        assert_eq!(
            (u32_at(b, 0), u32_at(b, 8), u32_at(b, 56)),
            (Some(1), Some(1000), Some(1)),
            "buffer 1, 1000 bytes, frame 1"
        );
        assert_eq!(
            u32_at(b, 12).unwrap() & (BUF_FLAG_DONE | BUF_FLAG_QUEUED),
            BUF_FLAG_DONE
        );
        assert_eq!(
            events.read_u32(USED + 4 + 4).unwrap(),
            8 + 88 + 8 * 64,
            "the whole event"
        );
        // The frame is in the memory the guest has mapped.
        // SAFETY: the buffer is alive (the session holds it) and 1000 bytes long at least.
        let seen = unsafe { std::slice::from_raw_parts(host as *const u8, 1000) };
        assert!(seen.iter().all(|b| *b == 1));
        // No buffer queued now: the next frame is dropped, not invented.
        std::thread::sleep(Duration::from_millis(40));
        assert!(r.camera.poll(&mut queues, &events).is_empty());

        // Closing while mapped: the camera stops, and the memory stays put
        // until the guest unmaps it.
        assert_eq!(r.send(&words(&[CMD_CLOSE, 0, s, 0])).0, 0);
        assert_eq!(r.log.lock().unwrap().last().unwrap(), "stop");
        assert_eq!(r.maps.lock().unwrap().len(), 1);
        let mut unmap = words(&[CMD_MUNMAP, 0]);
        unmap.extend_from_slice(&at.to_le_bytes());
        assert_eq!(r.send(&unmap).0, 0);
        assert!(r.maps.lock().unwrap().is_empty());
        assert_eq!(r.send(&unmap).0, EINVAL, "nothing is there any more");
    }
}
