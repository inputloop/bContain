// SPDX-FileCopyrightText: 2026 Bryant Frankford <nathanielfrankford@gmail.com> and Contributors
// SPDX-License-Identifier: GPL-3.0-or-later OR LGPL-3.0-or-later
//! A 16550A UART: the guest's console on COM1 (I/O 0x3f8, IRQ 4).

use std::collections::VecDeque;
use std::io::Write;

const DATA: u8 = 0; // RBR/THR, or DLL when DLAB
const IER: u8 = 1; // or DLM when DLAB
const IIR_FCR: u8 = 2;
const LCR: u8 = 3;
const MCR: u8 = 4;
const LSR: u8 = 5;
const MSR: u8 = 6;
const SCR: u8 = 7;

const IER_RDA: u8 = 0x01;
const IER_THRE: u8 = 0x02;
const IIR_NONE: u8 = 0x01;
const IIR_THRE: u8 = 0x02;
const IIR_RDA: u8 = 0x04;
const IIR_FIFO: u8 = 0xc0;
const LCR_DLAB: u8 = 0x80;
const LSR_DR: u8 = 0x01;
const LSR_THRE: u8 = 0x20;
const LSR_TEMT: u8 = 0x40;
const MCR_LOOP: u8 = 0x10;

pub struct Serial {
    ier: u8,
    lcr: u8,
    mcr: u8,
    scr: u8,
    dll: u8,
    dlm: u8,
    fifo: bool,
    /// A THR-empty interrupt is pending (cleared by reading IIR or writing THR).
    thre_pending: bool,
    rx: VecDeque<u8>,
    out: Box<dyn Write + Send>,
    /// Current level of the interrupt line.
    level: bool,
}

impl Serial {
    pub fn new(out: Box<dyn Write + Send>) -> Serial {
        Serial {
            ier: 0,
            lcr: 0x03,
            mcr: 0x08,
            scr: 0,
            dll: 0x0c,
            dlm: 0,
            fifo: false,
            thre_pending: false,
            rx: VecDeque::new(),
            out,
            level: false,
        }
    }

    fn iir(&self) -> u8 {
        let base = if self.fifo { IIR_FIFO } else { 0 };
        if self.ier & IER_RDA != 0 && !self.rx.is_empty() {
            base | IIR_RDA
        } else if self.ier & IER_THRE != 0 && self.thre_pending {
            base | IIR_THRE
        } else {
            base | IIR_NONE
        }
    }

    /// Recompute the interrupt line. Returns true on a rising edge, which is
    /// when the caller must deliver IRQ 4 (ISA interrupts are edge-triggered).
    fn update(&mut self) -> bool {
        let level = self.iir() & IIR_NONE == 0;
        let rising = level && !self.level;
        self.level = level;
        rising
    }

    /// Guest reads register `offset` (0..7). Returns the value and whether to raise IRQ 4.
    pub fn read(&mut self, offset: u8) -> (u8, bool) {
        let dlab = self.lcr & LCR_DLAB != 0;
        let v = match offset {
            DATA if dlab => self.dll,
            // In loopback mode, written bytes were queued into `rx` too.
            DATA => self.rx.pop_front().unwrap_or(0),
            IER if dlab => self.dlm,
            IER => self.ier,
            IIR_FCR => {
                let v = self.iir();
                if v & 0x0f == IIR_THRE {
                    self.thre_pending = false;
                }
                v
            }
            LCR => self.lcr,
            MCR => self.mcr,
            LSR => LSR_THRE | LSR_TEMT | if self.rx.is_empty() { 0 } else { LSR_DR },
            MSR => {
                if self.mcr & MCR_LOOP != 0 {
                    // Loopback: DTR->DSR, RTS->CTS, OUT1->RI, OUT2->DCD.
                    let m = self.mcr;
                    ((m & 0x01) << 5) | ((m & 0x02) << 3) | ((m & 0x04) << 4) | ((m & 0x08) << 4)
                } else {
                    0xb0 // DCD | DSR | CTS
                }
            }
            SCR => self.scr,
            _ => 0xff,
        };
        let edge = self.update();
        (v, edge)
    }

    /// Guest writes `value` to register `offset`. Returns whether to raise IRQ 4.
    pub fn write(&mut self, offset: u8, value: u8) -> bool {
        let dlab = self.lcr & LCR_DLAB != 0;
        match offset {
            DATA if dlab => self.dll = value,
            DATA => {
                if self.mcr & MCR_LOOP != 0 {
                    self.rx.push_back(value);
                } else {
                    let _ = self.out.write_all(&[value]);
                    if value == b'\n' {
                        let _ = self.out.flush();
                    }
                }
                // Writing THR clears the THRE condition; the byte "transmits"
                // at once, so THR is empty again: a fresh edge.
                self.thre_pending = false;
                self.update();
                self.thre_pending = true;
            }
            IER if dlab => self.dlm = value,
            IER => {
                let enabling_thre = value & IER_THRE != 0 && self.ier & IER_THRE == 0;
                self.ier = value & 0x0f;
                if enabling_thre {
                    self.thre_pending = true;
                }
            }
            IIR_FCR => {
                self.fifo = value & 0x01 != 0;
                if value & 0x02 != 0 {
                    self.rx.clear();
                }
            }
            LCR => self.lcr = value,
            MCR => self.mcr = value & 0x1f,
            SCR => self.scr = value,
            _ => {}
        }
        self.update()
    }

    /// Bytes typed on the host console. Returns whether to raise IRQ 4.
    pub fn push_input(&mut self, bytes: &[u8]) -> bool {
        if self.mcr & MCR_LOOP != 0 {
            return false;
        }
        self.rx.extend(bytes);
        self.update()
    }

    pub fn flush(&mut self) {
        let _ = self.out.flush();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};

    #[derive(Clone, Default)]
    struct Sink(Arc<Mutex<Vec<u8>>>);
    impl Write for Sink {
        fn write(&mut self, b: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(b);
            Ok(b.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn transmits_and_passes_linux_loopback_probe() {
        let sink = Sink::default();
        let mut s = Serial::new(Box::new(sink.clone()));
        for b in b"hi\n" {
            s.write(DATA, *b);
        }
        assert_eq!(&*sink.0.lock().unwrap(), b"hi\n");

        // Linux autoconfig: scratch register, then MCR loopback must read MSR 0x90.
        s.write(SCR, 0xa5);
        assert_eq!(s.read(SCR).0, 0xa5);
        s.write(MCR, MCR_LOOP | 0x0a);
        assert_eq!(s.read(MSR).0 & 0xf0, 0x90);
        s.write(MCR, 0x08);
        // LSR says the transmitter is idle
        assert_eq!(s.read(LSR).0 & (LSR_THRE | LSR_TEMT), LSR_THRE | LSR_TEMT);
    }

    #[test]
    fn divisor_latch() {
        let mut s = Serial::new(Box::new(Sink::default()));
        s.write(LCR, LCR_DLAB | 0x03);
        s.write(DATA, 0x01);
        s.write(IER, 0x00);
        assert_eq!(s.read(DATA).0, 0x01);
        s.write(LCR, 0x03);
        assert_eq!(s.read(IER).0, 0);
    }

    #[test]
    fn interrupts() {
        let mut s = Serial::new(Box::new(Sink::default()));
        // Enabling THRE interrupts raises one at once (THR is empty).
        assert!(s.write(IER, IER_THRE));
        assert_eq!(s.read(IIR_FCR).0 & 0x0f, IIR_THRE);
        // ...and reading IIR acknowledged it.
        assert_eq!(s.read(IIR_FCR).0 & 0x0f, IIR_NONE);
        // Each transmitted byte is a new edge.
        assert!(s.write(DATA, b'x'));
        // Received data has priority and raises an edge when enabled.
        s.write(IER, IER_THRE | IER_RDA);
        s.read(IIR_FCR);
        s.read(IIR_FCR);
        assert!(s.push_input(b"ab"));
        assert_eq!(s.read(IIR_FCR).0 & 0x0f, IIR_RDA);
        assert_eq!(s.read(LSR).0 & LSR_DR, LSR_DR);
        assert_eq!(s.read(DATA).0, b'a');
        assert_eq!(s.read(DATA).0, b'b');
        assert_eq!(s.read(LSR).0 & LSR_DR, 0);
    }
}
