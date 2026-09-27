//! Simulation-only semihosting window. Not an on-chip RL78 peripheral.
//!
//! A 16-bit store to offset `0x04` schedules a print of the NUL-terminated
//! guest string at `data_addr`. The bytes come from the one RAM/ROM window
//! that contains the address. A missing NUL, or an address that is MMIO /
//! unmapped, produces no output and does not read that device.

use std::sync::{Arc, Mutex};

use sim_kernel::{Addr, BusError, EventCtl, EventCtx, MemoryBus, MemoryMapped, SimEvent};

const WINDOW_LEN: usize = 6;

pub struct SemihostingUnit {
    data_addr: u32,
    ctl: EventCtl,
}

impl SemihostingUnit {
    #[must_use]
    pub fn new(ctl: EventCtl) -> Self {
        Self { data_addr: 0, ctl }
    }

    fn write_ctrl(&mut self, offset: u64, value: u16) {
        match offset {
            0x00 => {
                self.data_addr &= 0xFFFF_0000;
                self.data_addr |= u32::from(value);
            }
            0x02 => {
                self.data_addr &= 0x0000_FFFF;
                self.data_addr |= u32::from(value) << 16;
            }
            0x04 => {
                let data_addr = Addr::from(self.data_addr);
                self.ctl
                    .schedule(self.ctl.now(), Box::new(SemihostingEvent { data_addr }));
            }
            _ => {}
        }
    }

    fn register_image(&self) -> [u8; WINDOW_LEN] {
        let mut image = [0u8; WINDOW_LEN];
        image[..4].copy_from_slice(&self.data_addr.to_le_bytes());
        image
    }
}

pub struct SemihostingMmio {
    inner: Arc<Mutex<SemihostingUnit>>,
}

impl SemihostingMmio {
    #[must_use]
    pub fn new(inner: Arc<Mutex<SemihostingUnit>>) -> Self {
        Self { inner }
    }
}

impl MemoryMapped for SemihostingMmio {
    fn len(&self) -> u64 {
        WINDOW_LEN as u64
    }

    fn read(&mut self, offset: u64, buf: &mut [u8]) -> Result<(), BusError> {
        let image = self.inner.lock().expect("semihosting").register_image();
        let start = usize::try_from(offset).map_err(|_| BusError::OutOfRange {
            addr: offset,
            offset,
            len: buf.len(),
        })?;
        let end = start.checked_add(buf.len()).ok_or(BusError::OutOfRange {
            addr: offset,
            offset,
            len: buf.len(),
        })?;
        let Some(src) = image.get(start..end) else {
            return Err(BusError::OutOfRange {
                addr: offset,
                offset,
                len: buf.len(),
            });
        };
        buf.copy_from_slice(src);
        Ok(())
    }

    fn write(&mut self, offset: u64, buf: &[u8]) -> Result<(), BusError> {
        if buf.len() != 2 {
            return Err(BusError::Unmapped {
                addr: offset,
                len: buf.len(),
            });
        }
        let value = u16::from_le_bytes([buf[0], buf[1]]);
        self.inner
            .lock()
            .expect("semihosting")
            .write_ctrl(offset, value);
        Ok(())
    }

    fn load(&mut self, offset: u64, _buf: &[u8]) -> Result<(), BusError> {
        Err(BusError::NotLoadable { addr: offset })
    }

    fn host_ptr(&mut self) -> Option<*mut u8> {
        None
    }
}

struct SemihostingEvent {
    data_addr: Addr,
}

impl SimEvent for SemihostingEvent {
    fn fire(&mut self, ctx: &mut EventCtx<'_>) {
        let Some(text) = guest_cstring(ctx.bus, self.data_addr) else {
            return;
        };
        eprint!("{text}");
    }
}

/// End of the RAM/ROM window containing `addr`.
fn host_limit(bus: &mut MemoryBus, addr: Addr) -> Option<Addr> {
    let mut limit = None;
    bus.for_each_region(|base, size, host| {
        if limit.is_some() || host.is_none() {
            return;
        }
        let end = base.saturating_add(size);
        if addr >= base && addr < end {
            limit = Some(end);
        }
    });
    limit
}

/// NUL-terminated text inside the RAM/ROM window at `addr`.
///
/// `None` when `addr` is not in such a window, the window ends before a NUL,
/// or the read fails. Invalid UTF-8 is kept, with U+FFFD in place of bad
/// sequences.
fn guest_cstring(bus: &mut MemoryBus, addr: Addr) -> Option<String> {
    let end = host_limit(bus, addr)?;
    let len = usize::try_from(end.checked_sub(addr)?).ok()?;
    let mut buf = vec![0u8; len];
    bus.read(addr, &mut buf).ok()?;
    let nul = buf.iter().position(|&b| b == 0)?;
    Some(String::from_utf8_lossy(&buf[..nul]).into_owned())
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use sim_kernel::{EventCtl, EventCtx, MemoryBus, MemoryMapBuilder, Ram, Rom};

    use super::*;

    fn mmio(ctl: EventCtl) -> SemihostingMmio {
        SemihostingMmio::new(Arc::new(Mutex::new(SemihostingUnit::new(ctl))))
    }

    fn write_addr(bus: &mut MemoryBus, base: Addr, addr: u32) {
        let bytes = addr.to_le_bytes();
        bus.write(base, &bytes[0..2]).unwrap();
        bus.write(base + 2, &bytes[2..4]).unwrap();
    }

    #[test]
    fn read_returns_address_halves_and_zero_at_trigger() {
        let mut dev = mmio(EventCtl::new());
        dev.write(0x00, &0x1234u16.to_le_bytes()).unwrap();
        dev.write(0x02, &0xABCDu16.to_le_bytes()).unwrap();

        let mut word = [0xFFu8; 2];
        dev.read(0x00, &mut word).unwrap();
        assert_eq!(word, 0x1234u16.to_le_bytes());
        dev.read(0x02, &mut word).unwrap();
        assert_eq!(word, 0xABCDu16.to_le_bytes());
        dev.read(0x04, &mut word).unwrap();
        assert_eq!(word, [0, 0]);

        let mut byte = [0xFFu8; 1];
        dev.read(0x01, &mut byte).unwrap();
        assert_eq!(byte, [0x12]);
        dev.read(0x05, &mut byte).unwrap();
        assert_eq!(byte, [0]);
    }

    #[test]
    fn cstring_stops_at_nul_past_the_old_64_byte_cap() {
        let mut payload = vec![b'A'; 300];
        payload.push(0);
        let mut bus = MemoryMapBuilder::new()
            .map(0x1000, Box::new(Ram::from_bytes(payload)))
            .unwrap()
            .build()
            .unwrap();
        let text = guest_cstring(&mut bus, 0x1000).unwrap();
        assert_eq!(text.len(), 300);
        assert!(text.chars().all(|c| c == 'A'));
    }

    #[test]
    fn missing_nul_or_non_host_address_yields_nothing() {
        let mut bus = MemoryMapBuilder::new()
            .map(0x1000, Box::new(Ram::from_bytes(vec![b'B'; 8])))
            .unwrap()
            .map(0xE0000, Box::new(mmio(EventCtl::new())))
            .unwrap()
            .map(0x2000, Box::new(Rom::from_bytes(b"ok\0rest".to_vec())))
            .unwrap()
            .build()
            .unwrap();
        assert!(guest_cstring(&mut bus, 0x1000).is_none());
        assert!(guest_cstring(&mut bus, 0xE0000).is_none());
        assert!(guest_cstring(&mut bus, 0xF0000).is_none());
        assert_eq!(guest_cstring(&mut bus, 0x2000).as_deref(), Some("ok"));
        assert!(bus.take_unmapped_log().is_empty());
    }

    #[test]
    fn invalid_utf8_is_kept_with_replacement() {
        let mut bus = MemoryMapBuilder::new()
            .map(0x1000, Box::new(Ram::from_bytes(vec![0x61, 0xFF, 0x62, 0])))
            .unwrap()
            .build()
            .unwrap();
        let text = guest_cstring(&mut bus, 0x1000).unwrap();
        assert_eq!(text, "a\u{FFFD}b");
    }

    #[test]
    fn trigger_write_fires_the_guest_string() {
        let ctl = EventCtl::new();
        let mut bus = MemoryMapBuilder::new()
            .map(0xF0000, Box::new(Ram::from_bytes(b"hello\0".to_vec())))
            .unwrap()
            .map(0xE0000, Box::new(mmio(ctl.clone())))
            .unwrap()
            .build()
            .unwrap();
        write_addr(&mut bus, 0xE0000, 0xF0000);
        bus.write(0xE0000 + 4, &[0, 0]).unwrap();

        let mut word = [0xFFu8; 2];
        bus.read(0xE0000, &mut word).unwrap();
        assert_eq!(u16::from_le_bytes(word), 0x0000);
        bus.read(0xE0002, &mut word).unwrap();
        assert_eq!(u16::from_le_bytes(word), 0x000F);
        bus.read(0xE0004, &mut word).unwrap();
        assert_eq!(word, [0, 0]);

        let (_id, mut event) = ctl.events().pop_due(ctl.now()).expect("scheduled");
        let mut ctx = EventCtx {
            now: ctl.now(),
            bus: &mut bus,
            stop: None,
        };
        event.fire(&mut ctx);
        assert!(ctx.stop.is_none());
        assert!(bus.take_unmapped_log().is_empty());
    }
}
