// SPDX-FileCopyrightText: 2026 Bryant Frankford <nathanielfrankford@gmail.com> and Contributors
// SPDX-License-Identifier: GPL-3.0-or-later OR LGPL-3.0-or-later
//! virtio-input (Virtio 1.2, section 5.8): a touchscreen, keyboard or
//! gamepad the guest sees as an ordinary Linux input device.
//!
//! The host describes the device (its name and which events it can send),
//! keeps an [`InputSender`], and pushes events through it at any time from
//! any thread. Event types and codes are Linux's (`input-event-codes.h`).

use std::collections::{BTreeMap, VecDeque};
use std::sync::{Arc, Mutex, OnceLock};

use super::queue::Queue;
use super::{VirtioDevice, TYPE_INPUT};
use crate::memory::GuestMemory;

pub const EV_SYN: u16 = 0x00;
pub const EV_KEY: u16 = 0x01;
pub const EV_ABS: u16 = 0x03;
pub const SYN_REPORT: u16 = 0;
pub const BTN_TOUCH: u16 = 0x14a;
pub const ABS_MT_SLOT: u16 = 0x2f;
pub const ABS_MT_POSITION_X: u16 = 0x35;
pub const ABS_MT_POSITION_Y: u16 = 0x36;
pub const ABS_MT_TRACKING_ID: u16 = 0x39;
/// `INPUT_PROP_DIRECT`: touches land where the finger is (a touchscreen).
pub const PROP_DIRECT: u16 = 1;

const CFG_ID_NAME: u8 = 0x01;
const CFG_ID_SERIAL: u8 = 0x02;
const CFG_ID_DEVIDS: u8 = 0x03;
const CFG_PROP_BITS: u8 = 0x10;
const CFG_EV_BITS: u8 = 0x11;
const CFG_ABS_INFO: u8 = 0x12;

/// Events waiting for the guest are dropped beyond this, oldest first: a
/// guest that stops reading must not grow the host without bound.
const MAX_PENDING: usize = 4096;
/// Fingers a touchscreen tracks.
pub const TOUCH_SLOTS: u32 = 10;

/// One input event, as Linux defines it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Event {
    pub kind: u16,
    pub code: u16,
    pub value: i32,
}

impl Event {
    pub const fn new(kind: u16, code: u16, value: i32) -> Event {
        Event { kind, code, value }
    }

    /// Marks the end of a group of events that happened together.
    pub const fn sync() -> Event {
        Event::new(EV_SYN, SYN_REPORT, 0)
    }

    fn to_bytes(self) -> [u8; 8] {
        let mut b = [0u8; 8];
        b[0..2].copy_from_slice(&self.kind.to_le_bytes());
        b[2..4].copy_from_slice(&self.code.to_le_bytes());
        b[4..8].copy_from_slice(&self.value.to_le_bytes());
        b
    }
}

/// The range of one absolute axis.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct AbsInfo {
    pub min: i32,
    pub max: i32,
    pub fuzz: i32,
    pub flat: i32,
    pub resolution: i32,
}

struct Shared {
    pending: Mutex<VecDeque<Event>>,
    /// Asks the machine to deliver pending events; set once it is running.
    wake: OnceLock<Box<dyn Fn() + Send + Sync>>,
}

/// The host's end of an input device.
#[derive(Clone)]
pub struct InputSender {
    shared: Arc<Shared>,
}

impl InputSender {
    /// Queue `events` for the guest, in order. Events sent before the guest
    /// is ready are kept and delivered when it is.
    pub fn send(&self, events: &[Event]) {
        {
            let mut q = self.shared.pending.lock().unwrap();
            q.extend(events.iter().copied());
            while q.len() > MAX_PENDING {
                q.pop_front();
            }
        }
        if let Some(wake) = self.shared.wake.get() {
            wake();
        }
    }

    /// A finger in `slot` touches or moves to (x, y).
    pub fn touch(&self, slot: u32, x: i32, y: i32) {
        self.send(&[
            Event::new(EV_ABS, ABS_MT_SLOT, slot as i32),
            Event::new(EV_ABS, ABS_MT_TRACKING_ID, slot as i32),
            Event::new(EV_ABS, ABS_MT_POSITION_X, x),
            Event::new(EV_ABS, ABS_MT_POSITION_Y, y),
            Event::new(EV_KEY, BTN_TOUCH, 1),
            Event::sync(),
        ]);
    }

    /// The finger in `slot` lifts. `last` says no other finger is still down.
    pub fn release(&self, slot: u32, last: bool) {
        let mut events = vec![
            Event::new(EV_ABS, ABS_MT_SLOT, slot as i32),
            Event::new(EV_ABS, ABS_MT_TRACKING_ID, -1),
        ];
        if last {
            events.push(Event::new(EV_KEY, BTN_TOUCH, 0));
        }
        events.push(Event::sync());
        self.send(&events);
    }

    /// A key or button goes down or up.
    pub fn key(&self, code: u16, down: bool) {
        self.send(&[Event::new(EV_KEY, code, down as i32), Event::sync()]);
    }
}

/// An input device to give the guest. Build one, keep the sender, put the
/// device in the machine's configuration.
#[derive(Clone)]
pub struct Input {
    name: String,
    serial: String,
    /// bustype, vendor, product, version.
    ids: [u16; 4],
    props: Vec<u8>,
    /// Event type -> bitmap of the codes it can send.
    events: BTreeMap<u16, Vec<u8>>,
    axes: BTreeMap<u16, AbsInfo>,
    shared: Arc<Shared>,
    select: u8,
    subsel: u8,
}

fn set_bit(bits: &mut Vec<u8>, n: u16) {
    let byte = n as usize / 8;
    if bits.len() <= byte {
        bits.resize(byte + 1, 0);
    }
    bits[byte] |= 1 << (n % 8);
}

impl Input {
    /// A device named `name` that so far can send nothing.
    pub fn new(name: &str, serial: &str) -> (Input, InputSender) {
        let shared = Arc::new(Shared {
            pending: Mutex::new(VecDeque::new()),
            wake: OnceLock::new(),
        });
        let input = Input {
            name: name.to_string(),
            serial: serial.to_string(),
            ids: [0x06, 0x1af4, 0x0001, 0x0001], // BUS_VIRTUAL
            props: Vec::new(),
            events: BTreeMap::new(),
            axes: BTreeMap::new(),
            shared: shared.clone(),
            select: 0,
            subsel: 0,
        };
        (input, InputSender { shared })
    }

    /// The device can send event `code` of type `kind`.
    pub fn with_event(mut self, kind: u16, code: u16) -> Input {
        set_bit(self.events.entry(kind).or_default(), code);
        self
    }

    /// The device has absolute axis `code` with this range.
    pub fn with_axis(mut self, code: u16, info: AbsInfo) -> Input {
        self.axes.insert(code, info);
        self.with_event(EV_ABS, code)
    }

    pub fn with_property(mut self, prop: u16) -> Input {
        set_bit(&mut self.props, prop);
        self
    }

    /// A multi-touch screen `width` by `height`, reporting in pixels.
    pub fn touchscreen(width: u32, height: u32) -> (Input, InputSender) {
        let (input, sender) = Input::new("bvmm touchscreen", "bvmm-touch-0");
        let axis = |max: u32| AbsInfo {
            max: max as i32,
            ..Default::default()
        };
        let input = input
            .with_property(PROP_DIRECT)
            .with_event(EV_KEY, BTN_TOUCH)
            .with_axis(ABS_MT_SLOT, axis(TOUCH_SLOTS - 1))
            .with_axis(ABS_MT_TRACKING_ID, axis(TOUCH_SLOTS))
            .with_axis(ABS_MT_POSITION_X, axis(width.saturating_sub(1)))
            .with_axis(ABS_MT_POSITION_Y, axis(height.saturating_sub(1)));
        (input, sender)
    }

    /// A keyboard with every ordinary key (Linux key codes 1 to 248).
    pub fn keyboard() -> (Input, InputSender) {
        let (mut input, sender) = Input::new("bvmm keyboard", "bvmm-kbd-0");
        for code in 1..=248 {
            input = input.with_event(EV_KEY, code);
        }
        (input, sender)
    }

    /// Have the device ask `wake` to deliver events whenever some are sent.
    pub(crate) fn set_wake(&self, wake: Box<dyn Fn() + Send + Sync>) {
        let _ = self.shared.wake.set(wake);
    }

    /// The configuration payload for the current select/subsel.
    fn payload(&self) -> Vec<u8> {
        let mut p = match self.select {
            CFG_ID_NAME => self.name.as_bytes().to_vec(),
            CFG_ID_SERIAL => self.serial.as_bytes().to_vec(),
            CFG_ID_DEVIDS => self.ids.iter().flat_map(|v| v.to_le_bytes()).collect(),
            CFG_PROP_BITS => self.props.clone(),
            CFG_EV_BITS => self
                .events
                .get(&(self.subsel as u16))
                .cloned()
                .unwrap_or_default(),
            CFG_ABS_INFO => match self.axes.get(&(self.subsel as u16)) {
                Some(a) => [a.min, a.max, a.fuzz, a.flat, a.resolution]
                    .iter()
                    .flat_map(|v| v.to_le_bytes())
                    .collect(),
                None => Vec::new(),
            },
            _ => Vec::new(),
        };
        p.truncate(128);
        p
    }
}

impl VirtioDevice for Input {
    fn device_type(&self) -> u32 {
        TYPE_INPUT
    }

    fn features(&self) -> u64 {
        0
    }

    fn queue_max_sizes(&self) -> Vec<u16> {
        vec![64, 64] // events to the guest, status (LEDs) from it
    }

    fn read_config(&self, offset: u64, data: &mut [u8]) {
        // struct virtio_input_config: select, subsel, size, 5 reserved, 128 payload.
        let payload = self.payload();
        let mut cfg = [0u8; 8 + 128];
        cfg[0] = self.select;
        cfg[1] = self.subsel;
        cfg[2] = payload.len() as u8;
        cfg[8..8 + payload.len()].copy_from_slice(&payload);
        for (i, b) in data.iter_mut().enumerate() {
            *b = cfg.get(offset as usize + i).copied().unwrap_or(0);
        }
    }

    fn write_config(&mut self, offset: u64, data: &[u8]) {
        for (i, b) in data.iter().enumerate() {
            match offset as usize + i {
                0 => self.select = *b,
                1 => self.subsel = *b,
                _ => {}
            }
        }
    }

    fn notify(&mut self, index: usize, queue: &mut Queue, mem: &GuestMemory) -> bool {
        if index != 1 {
            // New buffers on the event queue: `poll` fills them.
            return false;
        }
        // Status events (keyboard LEDs and the like): accept and ignore.
        let mut any = false;
        while let Ok(Some(chain)) = queue.pop(mem) {
            if queue.push_used(mem, chain.head, 0).is_err() {
                break;
            }
            any = true;
        }
        any
    }

    fn poll(&mut self, queues: &mut [Queue], mem: &GuestMemory) -> Vec<usize> {
        let Some(queue) = queues.get_mut(0) else {
            return Vec::new();
        };
        let mut pending = self.shared.pending.lock().unwrap();
        let mut delivered = false;
        while let Some(event) = pending.front().copied() {
            // One event per buffer the driver offered; stop when it has none.
            let Ok(Some(chain)) = queue.pop(mem) else {
                break;
            };
            let written = chain.write_response(mem, &event.to_bytes()).unwrap_or(0);
            if queue.push_used(mem, chain.head, written).is_err() {
                break;
            }
            if written == 8 {
                pending.pop_front();
            }
            delivered = true;
        }
        if delivered {
            vec![0]
        } else {
            Vec::new()
        }
    }

    fn reset(&mut self) {
        self.select = 0;
        self.subsel = 0;
    }
}

#[cfg(test)]
mod tests {
    use super::super::queue::tests::{publish, put_desc, queue, USED};
    use super::*;

    const WRITE: u16 = 2;

    fn config(input: &mut Input, select: u8, subsel: u8) -> Vec<u8> {
        input.write_config(0, &[select, subsel]);
        let mut size = [0u8; 1];
        input.read_config(2, &mut size);
        let mut payload = vec![0u8; size[0] as usize];
        input.read_config(8, &mut payload);
        payload
    }

    #[test]
    fn a_touchscreen_describes_itself() {
        let (mut ts, _tx) = Input::touchscreen(720, 1280);
        assert_eq!(config(&mut ts, CFG_ID_NAME, 0), b"bvmm touchscreen");
        assert_eq!(config(&mut ts, CFG_ID_SERIAL, 0), b"bvmm-touch-0");
        assert_eq!(config(&mut ts, CFG_ID_DEVIDS, 0).len(), 8);
        assert_eq!(
            config(&mut ts, CFG_PROP_BITS, 0),
            [0b10],
            "INPUT_PROP_DIRECT"
        );
        let keys = config(&mut ts, CFG_EV_BITS, EV_KEY as u8);
        assert_eq!(keys.len(), BTN_TOUCH as usize / 8 + 1);
        assert_eq!(keys[BTN_TOUCH as usize / 8], 1 << (BTN_TOUCH % 8));
        let abs = config(&mut ts, CFG_EV_BITS, EV_ABS as u8);
        for code in [
            ABS_MT_SLOT,
            ABS_MT_POSITION_X,
            ABS_MT_POSITION_Y,
            ABS_MT_TRACKING_ID,
        ] {
            assert_ne!(
                abs[code as usize / 8] & (1 << (code % 8)),
                0,
                "axis {code:#x}"
            );
        }
        let x = config(&mut ts, CFG_ABS_INFO, ABS_MT_POSITION_X as u8);
        assert_eq!(x.len(), 20);
        assert_eq!(i32::from_le_bytes(x[4..8].try_into().unwrap()), 719);
        let y = config(&mut ts, CFG_ABS_INFO, ABS_MT_POSITION_Y as u8);
        assert_eq!(i32::from_le_bytes(y[4..8].try_into().unwrap()), 1279);
        // Things it does not have are empty, which is how the driver learns that.
        assert!(
            config(&mut ts, CFG_EV_BITS, 0x02).is_empty(),
            "no relative axes"
        );
        assert!(config(&mut ts, CFG_ABS_INFO, 0x00).is_empty(), "no ABS_X");
        assert!(config(&mut ts, 0x77, 0).is_empty());
    }

    #[test]
    fn a_keyboard_has_the_ordinary_keys() {
        let (mut kbd, _tx) = Input::keyboard();
        let keys = config(&mut kbd, CFG_EV_BITS, EV_KEY as u8);
        assert_eq!(keys.len(), 32);
        assert_eq!(keys[0], 0xfe, "codes 1..7, not 0");
        assert_eq!(keys[30 / 8] & (1 << (30 % 8)), 1 << 6, "KEY_A");
        assert!(config(&mut kbd, CFG_EV_BITS, EV_ABS as u8).is_empty());
    }

    #[test]
    fn events_reach_the_guest_in_order_when_it_has_buffers() {
        let mem = GuestMemory::new(64 << 20).unwrap();
        let (mut ts, tx) = Input::touchscreen(720, 1280);
        let mut queues = vec![queue(8), queue(8)];
        // Sent before the guest offered any buffer: kept, not lost.
        tx.touch(0, 100, 200);
        assert!(ts.poll(&mut queues, &mem).is_empty());

        // The guest offers four 8-byte buffers; six events are waiting.
        for i in 0..4u16 {
            put_desc(&mem, i, 0x8000 + i as u64 * 8, 8, WRITE, 0);
            publish(&mem, 8, i, i);
        }
        assert_eq!(ts.poll(&mut queues, &mem), vec![0]);
        assert_eq!(mem.read_u16(USED + 2).unwrap(), 4);
        let event = |i: u64| {
            let mut b = [0u8; 8];
            mem.read(0x8000 + i * 8, &mut b).unwrap();
            (
                u16::from_le_bytes([b[0], b[1]]),
                u16::from_le_bytes([b[2], b[3]]),
                i32::from_le_bytes([b[4], b[5], b[6], b[7]]),
            )
        };
        assert_eq!(event(0), (EV_ABS, ABS_MT_SLOT, 0));
        assert_eq!(event(1), (EV_ABS, ABS_MT_TRACKING_ID, 0));
        assert_eq!(event(2), (EV_ABS, ABS_MT_POSITION_X, 100));
        assert_eq!(event(3), (EV_ABS, ABS_MT_POSITION_Y, 200));
        assert_eq!(
            mem.read_u32(USED + 4 + 4).unwrap(),
            8,
            "eight bytes written"
        );

        // Nothing more until the guest offers buffers again; then the rest.
        assert!(ts.poll(&mut queues, &mem).is_empty());
        for i in 4..6u16 {
            publish(&mem, 8, i, i - 4);
        }
        assert_eq!(ts.poll(&mut queues, &mem), vec![0]);
        assert_eq!(event(0), (EV_KEY, BTN_TOUCH, 1));
        assert_eq!(event(1), (EV_SYN, SYN_REPORT, 0));
        assert!(ts.poll(&mut queues, &mem).is_empty(), "all delivered");
    }

    #[test]
    fn release_and_keys_build_the_right_events() {
        let (_ts, tx) = Input::touchscreen(10, 10);
        tx.release(3, true);
        tx.key(30, true);
        let got: Vec<Event> = tx.shared.pending.lock().unwrap().iter().copied().collect();
        assert_eq!(
            got,
            vec![
                Event::new(EV_ABS, ABS_MT_SLOT, 3),
                Event::new(EV_ABS, ABS_MT_TRACKING_ID, -1),
                Event::new(EV_KEY, BTN_TOUCH, 0),
                Event::sync(),
                Event::new(EV_KEY, 30, 1),
                Event::sync(),
            ]
        );
    }

    #[test]
    fn a_guest_that_never_reads_cannot_grow_the_host() {
        let (_ts, tx) = Input::keyboard();
        for i in 0..(MAX_PENDING + 500) {
            tx.send(&[Event::new(EV_KEY, 30, (i % 2) as i32)]);
        }
        assert_eq!(tx.shared.pending.lock().unwrap().len(), MAX_PENDING);
    }

    #[test]
    fn sending_wakes_the_machine_once_it_is_listening() {
        let (input, tx) = Input::keyboard();
        tx.key(30, true); // before any wake is set: no panic, just queued
        let woken = Arc::new(Mutex::new(0));
        let w = woken.clone();
        input.set_wake(Box::new(move || *w.lock().unwrap() += 1));
        tx.key(30, false);
        assert_eq!(*woken.lock().unwrap(), 1);
    }

    #[test]
    fn status_queue_buffers_are_returned() {
        let mem = GuestMemory::new(64 << 20).unwrap();
        let (mut kbd, _tx) = Input::keyboard();
        let mut q = queue(8);
        put_desc(&mem, 0, 0x8000, 8, 0, 0);
        publish(&mem, 8, 0, 0);
        assert!(kbd.notify(1, &mut q, &mem));
        assert_eq!(mem.read_u16(USED + 2).unwrap(), 1);
        assert!(
            !kbd.notify(0, &mut q, &mem),
            "event-queue kicks are handled by poll"
        );
    }
}
