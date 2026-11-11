// SPDX-FileCopyrightText: 2026 Bryant Frankford <nathanielfrankford@gmail.com> and Contributors
// SPDX-License-Identifier: GPL-3.0-or-later OR LGPL-3.0-or-later
//! virtio-snd (Virtio 1.2, section 5.14): a sound card with one playback
//! stream and one capture stream, 16-bit samples, mono or stereo.
//!
//! The device carries samples and keeps time; what plays them and where
//! recorded sound comes from is a [`SoundBackend`]'s business.
//!
//! **The device is the guest's clock.** The guest's driver advances its
//! position only when a buffer comes back, so each buffer (one "period" of
//! sound) is handed back when that much real time has passed, counted from
//! when the stream started so that the pace does not drift. A timer thread
//! asks the machine for a `poll` at each period's end.
//!
//! Written from gameDroid's docs/engine/E9-sound-camera-spec.md, which was
//! read from Linux 6.12's `sound/virtio` and `virtio_snd.h`.

use std::sync::{Arc, Condvar, Mutex, OnceLock};
use std::time::{Duration, Instant};

use super::gpu::u32_at;
use super::queue::Queue;
use super::{VirtioDevice, TYPE_SOUND};
use crate::memory::GuestMemory;

const Q_CONTROL: usize = 0;
const Q_TX: usize = 2;
const Q_RX: usize = 3;

const R_PCM_INFO: u32 = 0x100;
const R_PCM_SET_PARAMS: u32 = 0x101;
const R_PCM_PREPARE: u32 = 0x102;
const R_PCM_RELEASE: u32 = 0x103;
const R_PCM_START: u32 = 0x104;
const R_PCM_STOP: u32 = 0x105;

const S_OK: u32 = 0x8000;
const S_BAD_MSG: u32 = 0x8001;
const S_NOT_SUPP: u32 = 0x8002;

/// `VIRTIO_SND_PCM_FMT_S16`: the one sample format offered.
const FMT_S16: u8 = 5;
/// Sample rates offered, by the specification's rate numbers.
const RATES: [(u8, u32); 6] = [
    (1, 8000),
    (2, 11025),
    (3, 16000),
    (5, 32000),
    (6, 44100),
    (7, 48000),
];
const MAX_CHANNELS: u8 = 2;
/// The largest period taken: a quarter of a second of 48 kHz stereo.
const MAX_PERIOD_BYTES: u32 = 48_000;
/// Stream 0 plays, stream 1 records.
const STREAMS: usize = 2;
const PLAYBACK: usize = 0;
const CAPTURE: usize = 1;

/// Where played sound goes and recorded sound comes from. Samples are
/// 16-bit signed little-endian, channels interleaved. Calls come from the
/// thread running the guest and must not block.
pub trait SoundBackend: Send {
    fn playback_started(&mut self, _rate: u32, _channels: u8) {}
    /// One period of sound the guest played.
    fn play(&mut self, samples: &[u8]);
    fn playback_stopped(&mut self) {}
    fn capture_started(&mut self, _rate: u32, _channels: u8) {}
    /// Fill `out` with one period of recorded sound (silence if none).
    fn record(&mut self, out: &mut [u8]);
    fn capture_stopped(&mut self) {}
}

/// Plays into nothing and records silence.
pub struct Silent;

impl SoundBackend for Silent {
    fn play(&mut self, _: &[u8]) {}
    fn record(&mut self, out: &mut [u8]) {
        out.fill(0);
    }
}

#[derive(Clone, Copy)]
struct Params {
    period_bytes: u32,
    channels: u8,
    rate: u32,
}

impl Params {
    fn period(&self) -> Duration {
        let frames = self.period_bytes as u64 / (self.channels as u64 * 2);
        Duration::from_nanos(frames * 1_000_000_000 / self.rate as u64)
    }
}

#[derive(Default)]
struct Stream {
    params: Option<Params>,
    /// When the stream started, and how many periods have been served.
    running: Option<(Instant, u64)>,
    /// Hand every waiting buffer back at the next poll.
    flush: bool,
}

impl Stream {
    /// When the period now being played or recorded ends.
    fn deadline(&self) -> Option<Instant> {
        let (start, done) = self.running?;
        Some(start + self.params?.period() * (done + 1) as u32)
    }
}

struct Inner {
    backend: Box<dyn SoundBackend>,
    streams: [Stream; STREAMS],
}

struct Shared {
    inner: Mutex<Inner>,
    /// Told when a stream starts or stops, for the timer thread.
    changed: Condvar,
    wake: OnceLock<Box<dyn Fn() + Send + Sync>>,
}

#[derive(Clone)]
pub struct Sound {
    shared: Arc<Shared>,
}

impl Sound {
    pub fn new(backend: Box<dyn SoundBackend>) -> Sound {
        Sound {
            shared: Arc::new(Shared {
                inner: Mutex::new(Inner {
                    backend,
                    streams: Default::default(),
                }),
                changed: Condvar::new(),
                wake: OnceLock::new(),
            }),
        }
    }

    /// Have the device ask `wake` for a `poll` at the end of each period,
    /// and start the thread that keeps the time.
    pub(crate) fn set_wake(&self, wake: Box<dyn Fn() + Send + Sync>) {
        if self.shared.wake.set(wake).is_err() {
            return;
        }
        let shared = Arc::downgrade(&self.shared);
        std::thread::spawn(move || loop {
            let Some(shared) = shared.upgrade() else {
                return; // the card is gone
            };
            let inner = shared.inner.lock().unwrap();
            let next = inner.streams.iter().filter_map(Stream::deadline).min();
            match next {
                // Nothing running: wait to be told, but look up now and
                // then so the thread ends with the machine.
                None => {
                    let _ = shared.changed.wait_timeout(inner, Duration::from_secs(1));
                }
                Some(at) => {
                    let wait = at.saturating_duration_since(Instant::now());
                    drop(inner);
                    std::thread::sleep(wait);
                    if let Some(wake) = shared.wake.get() {
                        wake();
                    }
                }
            }
        });
    }
}

fn status(code: u32) -> Vec<u8> {
    code.to_le_bytes().to_vec()
}

impl Inner {
    fn control(&mut self, req: &[u8]) -> Vec<u8> {
        let Some(code) = u32_at(req, 0) else {
            return status(S_BAD_MSG);
        };
        if code == R_PCM_INFO {
            // start_id, count, size of each record.
            let (Some(start), Some(count), Some(size)) =
                (u32_at(req, 4), u32_at(req, 8), u32_at(req, 12))
            else {
                return status(S_BAD_MSG);
            };
            let fits = start
                .checked_add(count)
                .is_some_and(|end| end as usize <= STREAMS);
            if !fits || !(32..=4096).contains(&size) {
                return status(S_BAD_MSG);
            }
            let mut out = status(S_OK);
            for stream in start..start + count {
                let mut info = vec![0u8; size as usize];
                // hda_fn_nid 0 for both: it becomes the guest's device
                // number, and Android opens device 0 in both directions.
                let formats = 1u64 << FMT_S16;
                let rates: u64 = RATES.iter().map(|(bit, _)| 1u64 << bit).sum();
                info[8..16].copy_from_slice(&formats.to_le_bytes());
                info[16..24].copy_from_slice(&rates.to_le_bytes());
                info[24] = stream as u8; // direction: 0 output, 1 input
                info[25] = 1;
                info[26] = MAX_CHANNELS;
                out.extend_from_slice(&info);
            }
            return out;
        }
        let Some(id) = u32_at(req, 4).map(|s| s as usize).filter(|s| *s < STREAMS) else {
            return status(S_BAD_MSG);
        };
        match code {
            R_PCM_SET_PARAMS => {
                // buffer_bytes@8, period_bytes@12, features@16, channels@20,
                // format@21, rate@22.
                let (Some(period_bytes), Some(tail)) = (u32_at(req, 12), req.get(20..23)) else {
                    return status(S_BAD_MSG);
                };
                let (channels, format, rate) = (tail[0], tail[1], tail[2]);
                let Some((_, hz)) = RATES.iter().find(|(bit, _)| *bit == rate) else {
                    return status(S_NOT_SUPP);
                };
                if format != FMT_S16 || channels == 0 || channels > MAX_CHANNELS {
                    return status(S_NOT_SUPP);
                }
                let frame = channels as u32 * 2;
                if period_bytes == 0 || period_bytes % frame != 0 || period_bytes > MAX_PERIOD_BYTES
                {
                    return status(S_BAD_MSG);
                }
                if self.streams[id].running.is_some() {
                    return status(S_BAD_MSG);
                }
                self.streams[id].params = Some(Params {
                    period_bytes,
                    channels,
                    rate: *hz,
                });
                status(S_OK)
            }
            R_PCM_PREPARE => match self.streams[id].params {
                Some(_) => status(S_OK),
                None => status(S_BAD_MSG),
            },
            R_PCM_START => {
                let Some(p) = self.streams[id].params else {
                    return status(S_BAD_MSG);
                };
                if self.streams[id].running.is_none() {
                    self.streams[id].running = Some((Instant::now(), 0));
                    if id == PLAYBACK {
                        self.backend.playback_started(p.rate, p.channels);
                    } else {
                        self.backend.capture_started(p.rate, p.channels);
                    }
                }
                status(S_OK)
            }
            R_PCM_STOP | R_PCM_RELEASE => {
                if self.streams[id].running.take().is_some() {
                    if id == PLAYBACK {
                        self.backend.playback_stopped();
                    } else {
                        self.backend.capture_stopped();
                    }
                }
                if code == R_PCM_RELEASE {
                    // The driver waits for every buffer it queued.
                    self.streams[id].flush = true;
                }
                status(S_OK)
            }
            _ => status(S_NOT_SUPP),
        }
    }

    /// Serve one period of stream `id` from its queue. False if the guest
    /// had no buffer waiting.
    fn serve(&mut self, id: usize, queue: &mut Queue, mem: &GuestMemory) -> bool {
        let Ok(Some(chain)) = queue.pop(mem) else {
            return false;
        };
        let ok = [S_OK.to_le_bytes(), 0u32.to_le_bytes()].concat(); // status, latency
        let written = if id == PLAYBACK {
            // Out: the stream number, then the samples. In: the status.
            let data = chain
                .read_request(mem, 4 + MAX_PERIOD_BYTES as usize)
                .unwrap_or_default();
            if data.len() > 4 {
                self.backend.play(&data[4..]);
            }
            chain.write_response(mem, &ok).unwrap_or(0)
        } else {
            // In: the samples, then the status last.
            let room: usize = chain
                .descs
                .iter()
                .filter(|d| d.writable)
                .map(|d| d.len as usize)
                .sum();
            let mut reply = vec![0u8; room.saturating_sub(8).min(MAX_PERIOD_BYTES as usize)];
            self.backend.record(&mut reply);
            reply.extend_from_slice(&ok);
            chain.write_response(mem, &reply).unwrap_or(0)
        };
        queue.push_used(mem, chain.head, written).is_ok()
    }
}

impl VirtioDevice for Sound {
    fn device_type(&self) -> u32 {
        TYPE_SOUND
    }

    fn features(&self) -> u64 {
        0
    }

    fn queue_max_sizes(&self) -> Vec<u16> {
        vec![64, 64, 256, 256] // control, event, playback, capture
    }

    fn read_config(&self, offset: u64, data: &mut [u8]) {
        // jacks, streams, chmaps, controls.
        let mut cfg = [0u8; 16];
        cfg[4..8].copy_from_slice(&(STREAMS as u32).to_le_bytes());
        for (i, b) in data.iter_mut().enumerate() {
            *b = cfg.get(offset as usize + i).copied().unwrap_or(0);
        }
    }

    fn notify(&mut self, index: usize, queue: &mut Queue, mem: &GuestMemory) -> bool {
        if index != Q_CONTROL {
            return false; // sound buffers wait in their queues for `poll`
        }
        let mut inner = self.shared.inner.lock().unwrap();
        let mut any = false;
        while let Ok(Some(chain)) = queue.pop(mem) {
            let req = chain.read_request(mem, 64).unwrap_or_default();
            let reply = inner.control(&req);
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
        let mut inner = self.shared.inner.lock().unwrap();
        let mut used = Vec::new();
        let now = Instant::now();
        for (id, q) in [(PLAYBACK, Q_TX), (CAPTURE, Q_RX)] {
            let Some(queue) = queues.get_mut(q) else {
                continue;
            };
            let mut any = false;
            if std::mem::take(&mut inner.streams[id].flush) {
                // Hand everything back, with no sound in it.
                while let Ok(Some(chain)) = queue.pop(mem) {
                    let ok = [S_OK.to_le_bytes(), 0u32.to_le_bytes()].concat();
                    let written = chain.write_response(mem, &ok).unwrap_or(0);
                    any |= queue.push_used(mem, chain.head, written).is_ok();
                }
            }
            while let Some(deadline) = inner.streams[id].deadline() {
                if deadline > now {
                    break;
                }
                // A period has passed whether or not the guest had a
                // buffer ready; without one, that sound is simply lost.
                any |= inner.serve(id, queue, mem);
                let period = inner.streams[id]
                    .params
                    .map(|p| p.period())
                    .unwrap_or_default();
                if let Some((start, done)) = &mut inner.streams[id].running {
                    *done += 1;
                    // Far behind (the machine was paused, say): start
                    // counting afresh instead of racing to catch up.
                    if now.saturating_duration_since(deadline) > period * 16 {
                        *start = now;
                        *done = 0;
                    }
                }
            }
            if any {
                used.push(q);
            }
        }
        used
    }

    fn reset(&mut self) {
        let mut inner = self.shared.inner.lock().unwrap();
        for id in 0..STREAMS {
            if inner.streams[id].running.take().is_some() {
                if id == PLAYBACK {
                    inner.backend.playback_stopped();
                } else {
                    inner.backend.capture_stopped();
                }
            }
            inner.streams[id] = Stream::default();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::super::queue::tests::{publish, put_desc, queue, USED};
    use super::*;

    const WRITE: u16 = 2;
    const NEXT: u16 = 1;

    /// Remembers what was played, and "records" a counting pattern.
    #[derive(Default)]
    struct Tape {
        log: Arc<Mutex<Vec<String>>>,
    }

    impl SoundBackend for Tape {
        fn playback_started(&mut self, rate: u32, channels: u8) {
            self.log
                .lock()
                .unwrap()
                .push(format!("play start {rate} {channels}"));
        }
        fn play(&mut self, samples: &[u8]) {
            self.log.lock().unwrap().push(format!(
                "play {} bytes, first {}",
                samples.len(),
                samples[0]
            ));
        }
        fn playback_stopped(&mut self) {
            self.log.lock().unwrap().push("play stop".into());
        }
        fn capture_started(&mut self, rate: u32, channels: u8) {
            self.log
                .lock()
                .unwrap()
                .push(format!("record start {rate} {channels}"));
        }
        fn record(&mut self, out: &mut [u8]) {
            for (i, b) in out.iter_mut().enumerate() {
                *b = i as u8;
            }
        }
    }

    /// Send one control request on a fresh queue; returns the answer.
    fn control(snd: &mut Sound, req: &[u8]) -> Vec<u8> {
        let mem = GuestMemory::new(64 << 20).unwrap();
        let mut q = queue(8);
        mem.write(0x10_0000, req).unwrap();
        put_desc(&mem, 0, 0x10_0000, req.len() as u32, NEXT, 1);
        put_desc(&mem, 1, 0x10_1000, 256, WRITE, 0);
        publish(&mem, 8, 0, 0);
        assert!(snd.notify(Q_CONTROL, &mut q, &mem));
        let mut answer = vec![0u8; 256];
        mem.read(0x10_1000, &mut answer).unwrap();
        answer
    }

    fn words(w: &[u32]) -> Vec<u8> {
        w.iter().flat_map(|v| v.to_le_bytes()).collect()
    }

    /// SET_PARAMS: 10 ms periods of 48 kHz stereo (1920 bytes).
    fn params(stream: u32, period_bytes: u32, channels: u8, format: u8, rate: u8) -> Vec<u8> {
        let mut r = words(&[R_PCM_SET_PARAMS, stream, period_bytes * 4, period_bytes, 0]);
        r.extend_from_slice(&[channels, format, rate, 0]);
        r
    }

    #[test]
    fn the_guest_is_told_of_one_output_and_one_input() {
        let mut snd = Sound::new(Box::new(Silent));
        let mut config = [0u8; 16];
        snd.read_config(0, &mut config);
        assert_eq!(
            config,
            [0, 0, 0, 0, 2, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0],
            "two streams, nothing else"
        );
        let a = control(&mut snd, &words(&[R_PCM_INFO, 0, 2, 32]));
        assert_eq!(u32_at(&a, 0), Some(S_OK));
        for (stream, direction) in [(0usize, 0u8), (1, 1)] {
            let info = &a[4 + stream * 32..4 + (stream + 1) * 32];
            assert_eq!(u32_at(info, 0), Some(0), "device number 0 for both");
            assert_eq!(
                u64::from_le_bytes(info[8..16].try_into().unwrap()),
                0x20,
                "16-bit samples"
            );
            assert_eq!(
                u64::from_le_bytes(info[16..24].try_into().unwrap()),
                0xee,
                "8 to 48 kHz"
            );
            assert_eq!(&info[24..27], &[direction, 1, 2]);
        }
        // More streams than there are, and a record too small to hold one.
        assert_eq!(
            u32_at(&control(&mut snd, &words(&[R_PCM_INFO, 1, 2, 32])), 0),
            Some(S_BAD_MSG)
        );
        assert_eq!(
            u32_at(&control(&mut snd, &words(&[R_PCM_INFO, 0, 1, 8])), 0),
            Some(S_BAD_MSG)
        );
    }

    #[test]
    fn only_sound_the_card_can_carry_is_accepted() {
        let mut snd = Sound::new(Box::new(Silent));
        let kind = |snd: &mut Sound, req: &[u8]| u32_at(&control(snd, req), 0).unwrap();
        assert_eq!(
            kind(&mut snd, &words(&[R_PCM_START, 0])),
            S_BAD_MSG,
            "no parameters yet"
        );
        assert_eq!(kind(&mut snd, &params(0, 1920, 2, FMT_S16, 7)), S_OK);
        assert_eq!(
            kind(&mut snd, &params(0, 1920, 2, 17, 7)),
            S_NOT_SUPP,
            "32-bit samples"
        );
        assert_eq!(
            kind(&mut snd, &params(0, 1920, 2, FMT_S16, 12)),
            S_NOT_SUPP,
            "192 kHz"
        );
        assert_eq!(
            kind(&mut snd, &params(0, 1920, 6, FMT_S16, 7)),
            S_NOT_SUPP,
            "six channels"
        );
        assert_eq!(
            kind(&mut snd, &params(0, 1921, 2, FMT_S16, 7)),
            S_BAD_MSG,
            "half a frame"
        );
        assert_eq!(kind(&mut snd, &params(0, 0, 2, FMT_S16, 7)), S_BAD_MSG);
        assert_eq!(
            kind(&mut snd, &params(5, 1920, 2, FMT_S16, 7)),
            S_BAD_MSG,
            "no such stream"
        );
        assert_eq!(
            kind(&mut snd, &words(&[0x300, 0])),
            S_NOT_SUPP,
            "controls are not offered"
        );
    }

    #[test]
    fn sound_is_handed_back_at_the_pace_it_plays_and_released_at_once() {
        let log = Arc::new(Mutex::new(Vec::new()));
        let mut snd = Sound::new(Box::new(Tape { log: log.clone() }));
        control(&mut snd, &params(0, 1920, 2, FMT_S16, 7));
        control(&mut snd, &params(1, 320, 1, FMT_S16, 3)); // 10 ms of 16 kHz mono
        let mem = GuestMemory::new(64 << 20).unwrap();
        // Playback and capture queues live at the same test addresses, so
        // each gets its own memory; the device only sees one at a time here.
        let mem_rx = GuestMemory::new(64 << 20).unwrap();
        let mut tx = queue(8);
        let mut rx = queue(8);
        // Three playback buffers: the stream number, then a period of sound.
        for n in 0..3u16 {
            let at = 0x20_0000 + n as u64 * 0x1000;
            let mut out = 0u32.to_le_bytes().to_vec();
            out.extend_from_slice(&vec![n as u8 + 1; 1920]);
            mem.write(at, &out).unwrap();
            put_desc(&mem, n * 2, at, out.len() as u32, NEXT, n * 2 + 1);
            put_desc(&mem, n * 2 + 1, 0x30_0000 + n as u64 * 16, 8, WRITE, 0);
            publish(&mem, 8, n, n * 2);
        }
        // One capture buffer: the stream number; room for a period and the status.
        mem_rx.write(0x20_0000, &1u32.to_le_bytes()).unwrap();
        put_desc(&mem_rx, 0, 0x20_0000, 4, NEXT, 1);
        put_desc(&mem_rx, 1, 0x30_0000, 328, WRITE, 0);
        publish(&mem_rx, 8, 0, 0);

        let poll_tx = |snd: &mut Sound, tx: &mut Queue| {
            let mut queues = vec![
                queue(8),
                queue(8),
                std::mem::replace(tx, queue(8)),
                queue(8),
            ];
            // The capture queue here is empty, so only playback is served.
            let used = snd.poll(&mut queues, &mem);
            *tx = queues.remove(2);
            used
        };
        // Queued but not started: nothing comes back however long we wait.
        assert!(poll_tx(&mut snd, &mut tx).is_empty());
        control(&mut snd, &words(&[R_PCM_START, 0]));
        // Started, but the first period has not passed yet.
        assert!(poll_tx(&mut snd, &mut tx).is_empty());
        assert_eq!(mem.read_u16(USED + 2).unwrap(), 0);
        // After a little over one period (10 ms), exactly one comes back.
        std::thread::sleep(Duration::from_millis(13));
        assert_eq!(poll_tx(&mut snd, &mut tx), vec![Q_TX]);
        assert_eq!(mem.read_u16(USED + 2).unwrap(), 1, "one period, one buffer");
        assert_eq!(mem.read_u32(0x30_0000).unwrap(), S_OK);
        assert_eq!(
            *log.lock().unwrap(),
            vec!["play start 48000 2", "play 1920 bytes, first 1"]
        );
        // Release: the two still waiting come back at once, unplayed.
        control(&mut snd, &words(&[R_PCM_RELEASE, 0]));
        assert_eq!(poll_tx(&mut snd, &mut tx), vec![Q_TX]);
        assert_eq!(mem.read_u16(USED + 2).unwrap(), 3);
        assert_eq!(log.lock().unwrap().last().unwrap(), "play stop");
        assert_eq!(
            log.lock()
                .unwrap()
                .iter()
                .filter(|l| l.starts_with("play 1920"))
                .count(),
            1
        );

        // Capture: after a period, the buffer comes back full, status last.
        control(&mut snd, &words(&[R_PCM_START, 1]));
        std::thread::sleep(Duration::from_millis(13));
        let mut queues = vec![
            queue(8),
            queue(8),
            queue(8),
            std::mem::replace(&mut rx, queue(8)),
        ];
        // (The playback queue object here points at already-used test
        // memory in `mem_rx`, but playback is stopped, so it is not read.)
        assert_eq!(snd.poll(&mut queues, &mem_rx), vec![Q_RX]);
        let mut got = vec![0u8; 328];
        mem_rx.read(0x30_0000, &mut got).unwrap();
        assert_eq!(&got[..4], &[0, 1, 2, 3], "the recorded samples");
        assert_eq!(u32_at(&got, 320), Some(S_OK), "the status comes last");
        assert_eq!(
            mem_rx.read_u32(USED + 4 + 4).unwrap(),
            328,
            "samples and status"
        );
        assert!(log
            .lock()
            .unwrap()
            .contains(&"record start 16000 1".to_string()));
    }
}
