//! RL78/G23 digital I/O (ports P0–P15).
//!
//! Windows and fixed bits follow QEMU `hw/rl78/gpio.c`. PM bit 1 is input and
//! bit 0 is output. A level write updates only output pins, then drives
//! [`DigitalLevel`] on the pins that changed. An external level is latched
//! only while that pin is an input. [`DigitalLevel::HiZ`] is ignored.

use std::sync::{Arc, Mutex};

use sim_kernel::{BusError, DigitalLevel, MemoryBus, MemoryMapped, Resettable, SourcePort};

/// Number of port registers (`P0`..`P15` / `PM0`..`PM15`).
pub const PORTS: usize = 16;
/// Pins in one port register.
pub const PINS_PER_PORT: usize = 8;
/// `P0.0` .. `P15.7`, index `port * 8 + bit`.
pub const PINS: usize = PORTS * PINS_PER_PORT;

const MODE_INPUT: u8 = 1;
/// `PM13` has no SFR. Bit 7 stays an input; bits 0–6 are outputs.
const PM13_MODE: u8 = 0x80;

/// Board-edge output name for `pin` (`gpio_out_0` is `P0.0`).
#[must_use]
pub fn gpio_out_name(pin: usize) -> String {
    format!("gpio_out_{pin}")
}

/// Board-edge input name for `pin` (`gpio_in_0` is `P0.0`).
#[must_use]
pub fn gpio_in_name(pin: usize) -> String {
    format!("gpio_in_{pin}")
}

/// One MMIO window of the port block.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum GpioWindow {
    /// `PMn` at `0xFFF20`, 16 bytes. `PM13` is not a register; its direction is [`PM13_MODE`].
    Mode,
    /// `Pn` at `0xFFF00`, 16 bytes.
    Level,
    /// `PIOR` at `0xF0077`.
    Redirection,
    /// `PIDIS` at `0xF007D`.
    GlobalInputDisable,
    /// `PU` / `PIM` / `POM` / `PMCA` at `0xF0030`, 0x40 bytes.
    Other0,
    /// `PMCT` / `PMCE` / `CCS` / `CCDE` / `PTDC` / `PFOE` / `PDIDIS` at `0xF0260`.
    Other1,
}

impl GpioWindow {
    #[must_use]
    pub(crate) const fn len(self) -> u64 {
        match self {
            Self::Mode | Self::Level => 16,
            Self::Redirection | Self::GlobalInputDisable => 1,
            Self::Other0 => 0x40,
            Self::Other1 => 0x60,
        }
    }
}

/// Port block. Output pins are [`SourcePort`]s; inputs arrive via [`Self::on_pin`].
pub struct Gpio {
    mode: [u8; PORTS],
    level: [u8; PORTS],
    pullup: [u8; PORTS],
    input_mode: [u8; PORTS],
    output_mode: [u8; PORTS],
    digital_input_disable: [u8; PORTS],
    control_a: [u8; PORTS],
    control_t: [u8; PORTS],
    control_e: [u8; PORTS],
    redirection: u8,
    global_input_disable: u8,
    control_current: u8,
    /// `CCSn` values. Indices 0, 4, 5, 6, 7 are the implemented registers.
    ampere: [u8; 8],
    dc_level: u8,
    function_output: u16,
    out: [SourcePort<DigitalLevel>; PINS],
}

impl Default for Gpio {
    fn default() -> Self {
        Self::new()
    }
}

impl Gpio {
    #[must_use]
    pub fn new() -> Self {
        let mut gpio = Self {
            mode: [0; PORTS],
            level: [0; PORTS],
            pullup: [0; PORTS],
            input_mode: [0; PORTS],
            output_mode: [0; PORTS],
            digital_input_disable: [0; PORTS],
            control_a: [0; PORTS],
            control_t: [0; PORTS],
            control_e: [0; PORTS],
            redirection: 0,
            global_input_disable: 0,
            control_current: 0,
            ampere: [0; 8],
            dc_level: 0,
            function_output: 0,
            out: std::array::from_fn(|_| SourcePort::new()),
        };
        gpio.restore_reset();
        gpio
    }

    #[must_use]
    pub fn out_source(&mut self, pin: usize) -> &mut SourcePort<DigitalLevel> {
        &mut self.out[pin]
    }

    /// Wire sink for `gpio_in_{pin}`.
    pub fn on_pin(this: &Arc<Mutex<Self>>, pin: usize, values: &[DigitalLevel], changed: usize) {
        let Some(&level) = values.get(changed) else {
            return;
        };
        this.lock().expect("gpio").apply_input(pin, level);
        // TODO: raise the external pin IRQ (INTPn / INTKR) for this input edge.
    }

    fn restore_reset(&mut self) {
        self.mode = [0xFF; PORTS];
        self.mode[13] = PM13_MODE;
        self.level = [0; PORTS];
        self.pullup = [0; PORTS];
        self.pullup[4] = 0x01;
        self.input_mode = [0; PORTS];
        self.output_mode = [0; PORTS];
        self.digital_input_disable = [0; PORTS];
        self.control_a = [0xFF; PORTS];
        self.control_t = [0; PORTS];
        self.control_e = [0; PORTS];
        self.ampere = [0; 8];
        self.redirection = 0;
        self.global_input_disable = 0;
        self.control_current = 0;
        self.dc_level = 0;
        self.function_output = 0;
    }

    fn apply_input(&mut self, pin: usize, level: DigitalLevel) {
        let bit = match level {
            DigitalLevel::High => 1u8,
            DigitalLevel::Low => 0,
            DigitalLevel::HiZ => return,
        };
        let port = pin / PINS_PER_PORT;
        if port >= PORTS {
            return;
        }
        let bit_index = pin % PINS_PER_PORT;
        if (self.mode[port] >> bit_index) & 1 != MODE_INPUT {
            return;
        }
        self.level[port] &= !(1 << bit_index);
        self.level[port] |= bit << bit_index;
    }

    fn publish_level_changes(&self, port: usize, old: u8) {
        let new = self.level[port];
        let diff = old ^ new;
        for bit in 0..PINS_PER_PORT {
            if diff & (1 << bit) == 0 {
                continue;
            }
            let high = new & (1 << bit) != 0;
            let pin = port * PINS_PER_PORT + bit;
            self.out[pin].drive(if high {
                DigitalLevel::High
            } else {
                DigitalLevel::Low
            });
        }
    }

    fn write_mode(&mut self, port: usize, data: u8) {
        // PM13 is not a register. Direction stays [`PM13_MODE`].
        if port == 13 {
            return;
        }
        self.mode[port] = match port {
            10 | 15 => data | 0x80,
            12 => data | 0x18,
            _ => data,
        };
    }

    fn write_level(&mut self, port: usize, mut data: u8) {
        data = match port {
            10 | 15 => data & !0x80,
            13 => data & !0x7E,
            _ => data,
        };
        let old = self.level[port];
        let mode = self.mode[port];
        self.level[port] = (old & mode) | (!mode & data);
        self.publish_level_changes(port, old);
    }

    fn read(&self, window: GpioWindow, offset: u64) -> u8 {
        let index = offset as usize;
        let port = index & 0x0F;
        match window {
            GpioWindow::Mode => self.mode[port],
            GpioWindow::Level => self.level[port],
            GpioWindow::Redirection => self.redirection,
            GpioWindow::GlobalInputDisable => self.global_input_disable,
            GpioWindow::Other0 => match index >> 4 {
                0 => self.pullup[port],
                1 => self.input_mode[port],
                2 => self.output_mode[port],
                3 => self.control_a[port],
                _ => 0,
            },
            GpioWindow::Other1 => match index >> 4 {
                0 => self.control_t[port],
                2 => self.control_e[port],
                5 => self.digital_input_disable[port],
                4 => self.read_drive_group(port),
                _ => 0,
            },
        }
    }

    fn write(&mut self, window: GpioWindow, offset: u64, data: u8) {
        let index = offset as usize;
        let port = index & 0x0F;
        match window {
            GpioWindow::Mode => self.write_mode(port, data),
            GpioWindow::Level => self.write_level(port, data),
            GpioWindow::Redirection => self.redirection = data & !0xC0,
            GpioWindow::GlobalInputDisable => self.global_input_disable = data & !0xFE,
            GpioWindow::Other0 => match index >> 4 {
                0 => self.pullup[port] = data,
                1 => self.input_mode[port] = data,
                2 => self.output_mode[port] = data,
                3 => self.control_a[port] = data,
                _ => {}
            },
            GpioWindow::Other1 => match index >> 4 {
                0 => self.control_t[port] = data,
                2 => self.control_e[port] = data,
                5 => self.digital_input_disable[port] = data,
                4 => self.write_drive_group(port, data),
                _ => {}
            },
        }
    }

    fn read_drive_group(&self, index: usize) -> u8 {
        match index {
            0 | 4 | 5 | 6 | 7 => self.ampere[index],
            8 => self.control_current,
            9 => self.dc_level,
            0x0A => self.function_output as u8,
            0x0B => (self.function_output >> 8) as u8,
            _ => 0,
        }
    }

    fn write_drive_group(&mut self, index: usize, mut data: u8) {
        match index {
            0 | 4 | 5 | 6 | 7 => {
                if data > 4 {
                    data = 0;
                }
                self.ampere[index] = data;
            }
            8 => self.control_current = data,
            9 => self.dc_level = data & !0x80,
            0x0A => {
                self.function_output = (self.function_output & 0xFF00) | u16::from(data);
            }
            0x0B => {
                if data & 0xC0 != 0xC0 {
                    data |= 0xC0;
                }
                self.function_output = (self.function_output & 0x00FF) | (u16::from(data) << 8);
            }
            _ => {}
        }
    }
}

impl Resettable for Gpio {
    fn reset(&mut self, _bus: &mut MemoryBus) {
        self.restore_reset();
    }
}

pub(crate) struct GpioMmio {
    inner: Arc<Mutex<Gpio>>,
    window: GpioWindow,
}

impl GpioMmio {
    #[must_use]
    pub(crate) fn new(inner: Arc<Mutex<Gpio>>, window: GpioWindow) -> Self {
        Self { inner, window }
    }
}

impl MemoryMapped for GpioMmio {
    fn len(&self) -> u64 {
        self.window.len()
    }

    fn read(&mut self, offset: u64, buf: &mut [u8]) -> Result<(), BusError> {
        if buf.len() != 1 || offset >= self.window.len() {
            return Err(oob(offset, buf.len()));
        }
        buf[0] = self.inner.lock().expect("gpio").read(self.window, offset);
        Ok(())
    }

    fn write(&mut self, offset: u64, buf: &[u8]) -> Result<(), BusError> {
        if buf.len() != 1 || offset >= self.window.len() {
            return Err(oob(offset, buf.len()));
        }
        self.inner
            .lock()
            .expect("gpio")
            .write(self.window, offset, buf[0]);
        Ok(())
    }

    fn host_ptr(&mut self) -> Option<*mut u8> {
        None
    }

    fn load(&mut self, offset: u64, _buf: &[u8]) -> Result<(), BusError> {
        Err(BusError::NotLoadable { addr: offset })
    }
}

fn oob(offset: u64, len: usize) -> BusError {
    BusError::OutOfRange {
        addr: offset,
        offset,
        len,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use sim_kernel::{MemoryMapBuilder, OutPort, Wire};

    const LEVEL: u64 = 0xFFF00;
    const MODE: u64 = 0xFFF20;
    const OTHER0: u64 = 0xF0030;
    const PIOR: u64 = 0xF0077;
    const PIDIS: u64 = 0xF007D;
    const OTHER1: u64 = 0xF0260;

    fn mapped(gpio: &Arc<Mutex<Gpio>>) -> sim_kernel::MemoryBus {
        MemoryMapBuilder::new()
            .map(
                LEVEL,
                Box::new(GpioMmio::new(Arc::clone(gpio), GpioWindow::Level)),
            )
            .unwrap()
            .map(
                MODE,
                Box::new(GpioMmio::new(Arc::clone(gpio), GpioWindow::Mode)),
            )
            .unwrap()
            .map(
                PIOR,
                Box::new(GpioMmio::new(Arc::clone(gpio), GpioWindow::Redirection)),
            )
            .unwrap()
            .map(
                PIDIS,
                Box::new(GpioMmio::new(
                    Arc::clone(gpio),
                    GpioWindow::GlobalInputDisable,
                )),
            )
            .unwrap()
            .map(
                OTHER0,
                Box::new(GpioMmio::new(Arc::clone(gpio), GpioWindow::Other0)),
            )
            .unwrap()
            .map(
                OTHER1,
                Box::new(GpioMmio::new(Arc::clone(gpio), GpioWindow::Other1)),
            )
            .unwrap()
            .build()
            .unwrap()
    }

    fn read_at(bus: &mut sim_kernel::MemoryBus, addr: u64) -> u8 {
        let mut buf = [0u8; 1];
        bus.read(addr, &mut buf).unwrap();
        buf[0]
    }

    #[test]
    fn reset_values_match_qemu() {
        let gpio = Arc::new(Mutex::new(Gpio::new()));
        let mut bus = mapped(&gpio);
        assert_eq!(read_at(&mut bus, MODE), 0xFF);
        assert_eq!(read_at(&mut bus, LEVEL), 0x00);
        assert_eq!(read_at(&mut bus, MODE + 13), PM13_MODE);
        assert_eq!(read_at(&mut bus, OTHER0 + 4), 0x01);
        assert_eq!(read_at(&mut bus, OTHER0 + 0x30), 0xFF);
        bus.write(OTHER0 + 4, &[0x00]).unwrap();
        Resettable::reset(&mut *gpio.lock().unwrap(), &mut bus);
        assert_eq!(read_at(&mut bus, OTHER0 + 4), 0x01);
        assert_eq!(read_at(&mut bus, MODE), 0xFF);
    }

    #[test]
    fn output_write_drives_changed_pins_only() {
        let gpio = Arc::new(Mutex::new(Gpio::new()));
        let mut bus = mapped(&gpio);
        let tx = OutPort::new("gpio_out_0").unwrap();
        {
            let mut g = gpio.lock().unwrap();
            let tx = tx.clone();
            let _wire = Wire::new()
                .source(g.out_source(0))
                .sink(move |values, changed| tx.on_input(values, changed));
        }
        bus.write(MODE, &[0xFE]).unwrap();
        Gpio::on_pin(&gpio, 1, &[DigitalLevel::High], 0);
        bus.write(LEVEL, &[0x01]).unwrap();
        assert_eq!(read_at(&mut bus, LEVEL), 0x03);
        assert_eq!(tx.drain_pending(), vec![DigitalLevel::High]);
        bus.write(LEVEL, &[0x00]).unwrap();
        assert_eq!(read_at(&mut bus, LEVEL), 0x02);
        assert_eq!(tx.drain_pending(), vec![DigitalLevel::Low]);
    }

    #[test]
    fn input_updates_level_only_in_input_mode() {
        let gpio = Arc::new(Mutex::new(Gpio::new()));
        let mut bus = mapped(&gpio);
        Gpio::on_pin(&gpio, 3, &[DigitalLevel::High], 0);
        assert_eq!(read_at(&mut bus, LEVEL), 0x08);
        Gpio::on_pin(&gpio, 3, &[DigitalLevel::HiZ], 0);
        assert_eq!(read_at(&mut bus, LEVEL), 0x08);
        bus.write(MODE, &[0xF7]).unwrap();
        Gpio::on_pin(&gpio, 3, &[DigitalLevel::Low], 0);
        assert_eq!(read_at(&mut bus, LEVEL), 0x08);
    }

    #[test]
    fn fixed_bits_are_forced() {
        let gpio = Arc::new(Mutex::new(Gpio::new()));
        let mut bus = mapped(&gpio);
        bus.write(MODE + 10, &[0x00]).unwrap();
        bus.write(MODE + 12, &[0x00]).unwrap();
        bus.write(MODE + 13, &[0x00]).unwrap();
        bus.write(MODE + 15, &[0x00]).unwrap();
        assert_eq!(read_at(&mut bus, MODE + 10), 0x80);
        assert_eq!(read_at(&mut bus, MODE + 12), 0x18);
        assert_eq!(read_at(&mut bus, MODE + 13), PM13_MODE);
        assert_eq!(read_at(&mut bus, MODE + 15), 0x80);

        bus.write(LEVEL + 13, &[0xFF]).unwrap();
        assert_eq!(read_at(&mut bus, LEVEL + 13), 0x01);

        bus.write(LEVEL + 10, &[0xFF]).unwrap();
        bus.write(LEVEL + 15, &[0xFF]).unwrap();
        assert_eq!(read_at(&mut bus, LEVEL + 10), 0x7F);
        assert_eq!(read_at(&mut bus, LEVEL + 15), 0x7F);

        bus.write(PIOR, &[0xFF]).unwrap();
        bus.write(PIDIS, &[0xFF]).unwrap();
        assert_eq!(read_at(&mut bus, PIOR), 0x3F);
        assert_eq!(read_at(&mut bus, PIDIS), 0x01);

        bus.write(OTHER1 + 0x40, &[0x09]).unwrap();
        assert_eq!(read_at(&mut bus, OTHER1 + 0x40), 0x00);
        bus.write(OTHER1 + 0x44, &[0x03]).unwrap();
        assert_eq!(read_at(&mut bus, OTHER1 + 0x44), 0x03);
        bus.write(OTHER1 + 0x49, &[0xFF]).unwrap();
        assert_eq!(read_at(&mut bus, OTHER1 + 0x49), 0x7F);
        bus.write(OTHER1 + 0x4B, &[0x00]).unwrap();
        assert_eq!(read_at(&mut bus, OTHER1 + 0x4B), 0xC0);
        bus.write(OTHER1 + 0x10, &[0xA5]).unwrap();
        assert_eq!(read_at(&mut bus, OTHER1 + 0x10), 0x00);
    }
}
