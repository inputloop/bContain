// SPDX-FileCopyrightText: 2026 Bryant Frankford <nathanielfrankford@gmail.com> and Contributors
// SPDX-License-Identifier: GPL-3.0-or-later OR LGPL-3.0-or-later
//! CMOS real-time clock (I/O 0x70/0x71), so the guest boots with the right date.

use std::time::{SystemTime, UNIX_EPOCH};

#[derive(Default)]
pub struct Cmos {
    index: u8,
    /// Fixed clock for tests; `None` reads the host clock.
    fixed: Option<u64>,
}

fn bcd(v: u64) -> u8 {
    (((v / 10) << 4) | (v % 10)) as u8
}

/// Days since 1970-01-01 to (year, month, day). Howard Hinnant's algorithm.
fn civil(days: i64) -> (i64, u64, u64) {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u64;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u64;
    (yoe + era * 400 + i64::from(m <= 2), m, d)
}

impl Cmos {
    pub fn write_index(&mut self, v: u8) {
        self.index = v & 0x7f;
    }

    pub fn read_data(&self) -> u8 {
        let now = self.fixed.unwrap_or_else(|| {
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|d| d.as_secs())
                .unwrap_or(0)
        });
        let days = (now / 86_400) as i64;
        let secs = now % 86_400;
        let (y, m, d) = civil(days);
        match self.index {
            0x00 => bcd(secs % 60),
            0x02 => bcd(secs / 60 % 60),
            0x04 => bcd(secs / 3600),
            0x06 => bcd(((days + 4).rem_euclid(7) + 1) as u64), // 1 = Sunday
            0x07 => bcd(d),
            0x08 => bcd(m),
            0x09 => bcd((y % 100) as u64),
            0x32 => bcd((y / 100) as u64),
            0x0a => 0x26, // divider on, 1024 Hz; update not in progress
            0x0b => 0x02, // 24-hour, BCD
            0x0d => 0x80, // battery good
            _ => 0,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_bcd_time() {
        // 2026-10-26 04:30:15 UTC, a Monday.
        let mut c = Cmos {
            index: 0,
            fixed: Some(1_792_989_015),
        };
        let mut get = |i| {
            c.write_index(i);
            c.read_data()
        };
        assert_eq!(get(0x09), 0x26);
        assert_eq!(get(0x32), 0x20);
        assert_eq!(get(0x08), 0x10);
        assert_eq!(get(0x07), 0x26);
        assert_eq!(get(0x04), 0x04);
        assert_eq!(get(0x02), 0x30);
        assert_eq!(get(0x00), 0x15);
        assert_eq!(get(0x06), 0x02);
    }
}
