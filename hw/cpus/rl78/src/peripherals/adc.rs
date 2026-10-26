//! A/D converter (QEMU `hw/rl78/adc.c`).
//!
//! Absolute SFR bases are applied by the SoC map, not this module.

use std::sync::{Arc, Mutex};

use sim_kernel::{
    AnalogVoltage, BusError, EventCtl, EventCtx, EventId, MemoryBus, MemoryMapped, Resettable,
    SimEvent, SourcePort, Tick,
};

use crate::peripherals::clock::{ClockOutputs, Cycles};

/// Analog input slots (`ANI0`–`ANI14`, `ANI16`–`ANI26`, `TSCAP`).
pub const SOURCE_NUM: usize = 27;
const SCAN_CHANNELS: usize = 4;
/// Conversion reference used by QEMU (`voltage / 5.0`).
const REF_UV: i64 = 5_000_000;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ConvertMode {
    Select,
    Scan,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum TriggerMode {
    Software,
    HardwareNowait,
    HardwareWait,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum OperationMode {
    Continuous,
    Oneshot,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum HwTrigger {
    Tau,
    Rtc,
    Itl32,
    Elcl,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ReferenceVoltage {
    Vdd,
    Port,
    Internal,
    Discharge,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ReferenceGnd {
    Vss,
    Port,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Resolution {
    Bits8,
    Bits10,
    Bits12,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
enum InputSource {
    Ani0 = 0,
    Ani1 = 1,
    Ani2 = 2,
    Ani3 = 3,
    Ani4 = 4,
    Ani5 = 5,
    Ani6 = 6,
    Ani7 = 7,
    Ani8 = 8,
    Ani9 = 9,
    Ani10 = 10,
    Ani11 = 11,
    Ani12 = 12,
    Ani13 = 13,
    Ani14 = 14,
    Ani16 = 15,
    Ani17 = 16,
    Ani18 = 17,
    Ani19 = 18,
    Ani20 = 19,
    Ani21 = 20,
    Ani22 = 21,
    Ani23 = 22,
    Ani24 = 23,
    Ani25 = 24,
    Ani26 = 25,
    Tscap = 26,
    Sensor = 27,
    Internal = 28,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum TestTarget {
    Normal,
    GndRef,
    VddRef,
}

/// One MMIO window of the ADC (QEMU `mmio[0..3]`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum AdcWindow {
    /// `ADCR` / `ADCRH` at `0xFFF1E`, 2 bytes.
    Adcr,
    /// `ADM0` / `ADS` / `ADM1` at `0xFFF30`, 3 bytes.
    Adm,
    /// `ADM2` / `ADUL` / `ADLL` / `ADTES` / `ADCR0–3` at `0xF0010`, 0x18 bytes.
    Ext,
}

impl AdcWindow {
    #[must_use]
    fn len(self) -> u64 {
        match self {
            Self::Adcr => 0x02,
            Self::Adm => 0x03,
            Self::Ext => 0x18,
        }
    }
}

pub struct AdcUnit {
    conversion_running: bool,
    comparator_enabled: bool,
    convert_mode: ConvertMode,
    lv: u8,
    fr: u8,
    trigger_mode: TriggerMode,
    operation_mode: OperationMode,
    hw_trigger: HwTrigger,
    lowspeed_clock: bool,
    reference_voltage: ReferenceVoltage,
    reference_gnd: ReferenceGnd,
    interrupt_in_range: bool,
    use_snooze: bool,
    resolution: Resolution,
    input_source: InputSource,
    adul: u8,
    adll: u8,
    adcr: u16,
    scan_adcr: [u16; SCAN_CHANNELS],
    scan_index: u8,
    test_target: TestTarget,
    voltages: [AnalogVoltage; SOURCE_NUM],
    irq_out: SourcePort<()>,
    ctl: EventCtl,
    clock: ClockOutputs,
    expire_id: Option<EventId>,
    generation: u32,
}

impl AdcUnit {
    #[must_use]
    pub fn new(ctl: EventCtl, clock: ClockOutputs) -> Self {
        let mut adc = Self {
            conversion_running: false,
            comparator_enabled: false,
            convert_mode: ConvertMode::Select,
            lv: 0,
            fr: 0,
            trigger_mode: TriggerMode::Software,
            operation_mode: OperationMode::Continuous,
            hw_trigger: HwTrigger::Tau,
            lowspeed_clock: false,
            reference_voltage: ReferenceVoltage::Vdd,
            reference_gnd: ReferenceGnd::Vss,
            interrupt_in_range: true,
            use_snooze: false,
            resolution: Resolution::Bits10,
            input_source: InputSource::Ani0,
            adul: 0xFF,
            adll: 0x00,
            adcr: 0,
            scan_adcr: [0; SCAN_CHANNELS],
            scan_index: 0,
            test_target: TestTarget::Normal,
            voltages: [AnalogVoltage(0); SOURCE_NUM],
            irq_out: SourcePort::new(),
            ctl,
            clock,
            expire_id: None,
            generation: 0,
        };
        adc.restore_reset();
        adc
    }

    #[must_use]
    pub fn irq_source(&mut self) -> &mut SourcePort<()> {
        &mut self.irq_out
    }

    #[must_use]
    pub fn is_conversion_running(&self) -> bool {
        self.conversion_running
    }

    #[must_use]
    pub fn adcr(&self) -> u16 {
        self.adcr
    }

    /// Host-side analog level for slot `index` (`adc-result[n]` in QEMU).
    pub fn set_voltage(&mut self, index: usize, voltage: AnalogVoltage) {
        if let Some(slot) = self.voltages.get_mut(index) {
            *slot = voltage;
        }
    }

    /// Wire sink for analog input `index`.
    pub fn on_channel(
        this: &Arc<Mutex<Self>>,
        index: usize,
        values: &[AnalogVoltage],
        changed: usize,
    ) {
        let Some(&voltage) = values.get(changed) else {
            return;
        };
        this.lock().expect("adc").set_voltage(index, voltage);
    }

    fn restore_reset(&mut self) {
        self.cancel_pending();
        self.conversion_running = false;
        self.comparator_enabled = false;
        self.convert_mode = ConvertMode::Select;
        self.lv = 0;
        self.fr = 0;
        self.trigger_mode = TriggerMode::Software;
        self.lowspeed_clock = false;
        self.operation_mode = OperationMode::Continuous;
        self.hw_trigger = HwTrigger::Tau;
        self.reference_voltage = ReferenceVoltage::Vdd;
        self.reference_gnd = ReferenceGnd::Vss;
        self.interrupt_in_range = true;
        self.use_snooze = false;
        self.resolution = Resolution::Bits10;
        self.adcr = 0;
        self.scan_adcr = [0; SCAN_CHANNELS];
        self.scan_index = 0;
        self.input_source = InputSource::Ani0;
        self.adul = 0xFF;
        self.adll = 0x00;
        self.test_target = TestTarget::Normal;
    }

    fn cancel_pending(&mut self) {
        self.generation = self.generation.wrapping_add(1);
        if let Some(id) = self.expire_id.take() {
            self.ctl.cancel(id);
        }
    }

    fn clock_divider(&self) -> u64 {
        if self.lowspeed_clock {
            match self.fr {
                4 => 2,
                5 => 1,
                _ => 4,
            }
        } else {
            match self.fr {
                1 => 16,
                2 => 8,
                3 => 4,
                4 => 2,
                5 => 1,
                _ => 32,
            }
        }
    }

    fn conversion_cycles(&self) -> u64 {
        match self.lv {
            1 => 181,
            2 => 80,
            3 => 107,
            _ => 64,
        }
    }

    fn wakeup_cycles(&self) -> u64 {
        if self.comparator_enabled {
            return 1;
        }
        if self.lowspeed_clock {
            match self.fr {
                5 => 6,
                _ => 4,
            }
        } else {
            match self.fr {
                2 => 6,
                3 => 10,
                4 => 18,
                5 => 34,
                _ => 4,
            }
        }
    }

    fn interrupt_delay_cycles(&self) -> u64 {
        if self.comparator_enabled || self.operation_mode == OperationMode::Continuous {
            1
        } else {
            4
        }
    }

    fn first_duration(&self) -> Option<Tick> {
        let total = self
            .conversion_cycles()
            .saturating_add(self.wakeup_cycles())
            .saturating_add(self.interrupt_delay_cycles());
        self.duration(total)
    }

    fn next_duration(&self) -> Option<Tick> {
        let total = self
            .conversion_cycles()
            .saturating_add(self.interrupt_delay_cycles());
        self.duration(total)
    }

    fn duration(&self, adc_clocks: u64) -> Option<Tick> {
        self.clock
            .f_clk()
            .cycles_to_tick(Cycles::from_count(adc_clocks).saturating_mul(self.clock_divider()))
    }

    fn start(&mut self, inner: &Arc<Mutex<AdcUnit>>) {
        self.cancel_pending();
        self.scan_index = 0;
        self.schedule(inner, self.first_duration());
    }

    fn schedule(&mut self, inner: &Arc<Mutex<AdcUnit>>, period: Option<Tick>) {
        let Some(period) = period else {
            return;
        };
        if period.is_zero() {
            return;
        }
        let at = self.ctl.now().saturating_add(period);
        let id = self.ctl.schedule(
            at,
            Box::new(AdcComplete {
                inner: Arc::clone(inner),
                generation: self.generation,
                at,
            }),
        );
        self.expire_id = Some(id);
    }

    fn fetch_result(&self) -> u16 {
        match self.input_source {
            InputSource::Tscap | InputSource::Sensor | InputSource::Internal => return 0,
            _ => {}
        }
        let index = (self.input_source as u8).saturating_add(self.scan_index) as usize;
        let Some(voltage) = self.voltages.get(index) else {
            return 0;
        };
        let uv = voltage.0.max(0);
        let (scale, shift, max) = match self.resolution {
            Resolution::Bits8 => (256_i64, 8, 255_u16),
            Resolution::Bits10 => (1024, 6, 1023),
            Resolution::Bits12 => (4096, 0, 4095),
        };
        let raw = u16::try_from(uv.saturating_mul(scale) / REF_UV).unwrap_or(u16::MAX);
        raw.min(max) << shift
    }

    fn range_threshold(&self, range: u8) -> u16 {
        match self.resolution {
            Resolution::Bits8 | Resolution::Bits10 => u16::from(range) << 8,
            Resolution::Bits12 => u16::from(range) << 4,
        }
    }

    fn complete(&mut self, inner: &Arc<Mutex<AdcUnit>>, at: Tick) {
        self.expire_id = None;
        let result = self.fetch_result();
        if self.scan_index == 0 {
            self.adcr = result;
        }
        let adul = self.range_threshold(self.adul);
        let adll = self.range_threshold(self.adll);
        let is_valid = if self.interrupt_in_range {
            adll <= self.adcr && self.adcr <= adul
        } else {
            self.adcr < adll || adul <= self.adcr
        };
        if is_valid {
            self.scan_adcr[self.scan_index as usize] = result;
        }
        let mut next = self.scan_index;
        if self.convert_mode == ConvertMode::Scan {
            next = (next + 1) % SCAN_CHANNELS as u8;
        }
        self.scan_index = next;
        if next == 0 && is_valid {
            self.irq_out.drive(());
        }
        if self.operation_mode == OperationMode::Oneshot && next == 0 {
            self.conversion_running = false;
            return;
        }
        let Some(period) = self.next_duration() else {
            return;
        };
        if period.is_zero() {
            return;
        }
        let mut deadline = at.saturating_add(period);
        let now = self.ctl.now();
        while deadline <= now {
            deadline = deadline.saturating_add(period);
        }
        let id = self.ctl.schedule(
            deadline,
            Box::new(AdcComplete {
                inner: Arc::clone(inner),
                generation: self.generation,
                at: deadline,
            }),
        );
        self.expire_id = Some(id);
    }

    fn write_adm0(&mut self, value: u8, inner: &Arc<Mutex<AdcUnit>>) {
        self.convert_mode = if value & (1 << 6) != 0 {
            ConvertMode::Scan
        } else {
            ConvertMode::Select
        };
        self.lv = (value >> 1) & 0x3;
        self.fr = (value >> 3) & 0x7;
        self.conversion_running = value & (1 << 7) != 0;
        self.comparator_enabled = value & (1 << 0) != 0;
        if self.conversion_running {
            self.start(inner);
        } else {
            self.cancel_pending();
        }
    }

    fn write_adm1(&mut self, value: u8) {
        self.trigger_mode = match value >> 6 {
            2 => TriggerMode::HardwareNowait,
            3 => TriggerMode::HardwareWait,
            _ => TriggerMode::Software,
        };
        self.lowspeed_clock = value & (1 << 3) != 0;
        self.operation_mode = if value & (1 << 5) != 0 {
            OperationMode::Oneshot
        } else {
            OperationMode::Continuous
        };
        self.hw_trigger = match value & 0x7 {
            2 => HwTrigger::Rtc,
            3 => HwTrigger::Itl32,
            4 => HwTrigger::Elcl,
            _ => HwTrigger::Tau,
        };
    }

    fn write_adm2(&mut self, value: u8) {
        self.reference_voltage = match (value >> 6) & 0x3 {
            1 => ReferenceVoltage::Port,
            2 => ReferenceVoltage::Internal,
            3 => ReferenceVoltage::Discharge,
            _ => ReferenceVoltage::Vdd,
        };
        self.reference_gnd = if value & (1 << 5) != 0 {
            ReferenceGnd::Port
        } else {
            ReferenceGnd::Vss
        };
        self.interrupt_in_range = value & (1 << 3) == 0;
        self.use_snooze = value & (1 << 2) != 0;
        self.resolution = match value & 0x3 {
            1 => Resolution::Bits8,
            2 => Resolution::Bits12,
            _ => Resolution::Bits10,
        };
    }

    fn write_ads(&mut self, value: u8, inner: &Arc<Mutex<AdcUnit>>) {
        let ads = value & 0x1F;
        let adiss = value & (1 << 7) != 0;
        self.input_source = if adiss {
            match ads {
                1 => InputSource::Internal,
                _ => InputSource::Sensor,
            }
        } else {
            match ads {
                1 => InputSource::Ani1,
                2 => InputSource::Ani2,
                3 => InputSource::Ani3,
                4 => InputSource::Ani4,
                5 => InputSource::Ani5,
                6 => InputSource::Ani6,
                7 => InputSource::Ani7,
                8 => InputSource::Ani8,
                9 => InputSource::Ani9,
                10 => InputSource::Ani10,
                11 => InputSource::Ani11,
                12 => InputSource::Ani12,
                13 => InputSource::Ani13,
                14 => InputSource::Ani14,
                16 => InputSource::Ani16,
                17 => InputSource::Ani17,
                18 => InputSource::Ani18,
                19 => InputSource::Ani19,
                20 => InputSource::Ani20,
                21 => InputSource::Ani21,
                22 => InputSource::Ani22,
                23 => InputSource::Ani23,
                24 => InputSource::Ani24,
                25 => InputSource::Ani25,
                26 => InputSource::Ani26,
                30 => InputSource::Tscap,
                _ => InputSource::Ani0,
            }
        };
        if self.conversion_running {
            self.start(inner);
        }
    }

    fn write_adul(&mut self, value: u8) {
        self.adul = value;
    }

    fn write_adll(&mut self, value: u8) {
        self.adll = value;
    }

    fn write_adtes(&mut self, value: u8) {
        self.test_target = match value & 0x3 {
            2 => TestTarget::GndRef,
            3 => TestTarget::VddRef,
            _ => TestTarget::Normal,
        };
    }

    fn read_adm0(&self) -> u8 {
        let mut value = 0;
        if self.conversion_running {
            value |= 1 << 7;
        }
        if self.convert_mode == ConvertMode::Scan {
            value |= 1 << 6;
        }
        value |= (self.fr & 0x7) << 3;
        value |= (self.lv & 0x3) << 1;
        if self.comparator_enabled {
            value |= 1;
        }
        value
    }

    fn read_adm1(&self) -> u8 {
        let mut value = match self.trigger_mode {
            TriggerMode::Software => 0,
            TriggerMode::HardwareNowait => 2 << 6,
            TriggerMode::HardwareWait => 3 << 6,
        };
        if self.operation_mode == OperationMode::Oneshot {
            value |= 1 << 5;
        }
        if self.lowspeed_clock {
            value |= 1 << 3;
        }
        value |= match self.hw_trigger {
            HwTrigger::Tau => 0,
            HwTrigger::Rtc => 2,
            HwTrigger::Itl32 => 3,
            HwTrigger::Elcl => 4,
        };
        value
    }

    fn read_adm2(&self) -> u8 {
        let mut value = match self.reference_voltage {
            ReferenceVoltage::Vdd => 0,
            ReferenceVoltage::Port => 1 << 6,
            ReferenceVoltage::Internal => 2 << 6,
            ReferenceVoltage::Discharge => 3 << 6,
        };
        if self.reference_gnd == ReferenceGnd::Port {
            value |= 1 << 5;
        }
        if !self.interrupt_in_range {
            value |= 1 << 3;
        }
        if self.use_snooze {
            value |= 1 << 2;
        }
        value |= match self.resolution {
            Resolution::Bits10 => 0,
            Resolution::Bits8 => 1,
            Resolution::Bits12 => 2,
        };
        value
    }

    fn read_ads(&self) -> u8 {
        match self.input_source {
            InputSource::Ani0 => 0,
            InputSource::Ani1 => 1,
            InputSource::Ani2 => 2,
            InputSource::Ani3 => 3,
            InputSource::Ani4 => 4,
            InputSource::Ani5 => 5,
            InputSource::Ani6 => 6,
            InputSource::Ani7 => 7,
            InputSource::Ani8 => 8,
            InputSource::Ani9 => 9,
            InputSource::Ani10 => 10,
            InputSource::Ani11 => 11,
            InputSource::Ani12 => 12,
            InputSource::Ani13 => 13,
            InputSource::Ani14 => 14,
            InputSource::Ani16 => 16,
            InputSource::Ani17 => 17,
            InputSource::Ani18 => 18,
            InputSource::Ani19 => 19,
            InputSource::Ani20 => 20,
            InputSource::Ani21 => 21,
            InputSource::Ani22 => 22,
            InputSource::Ani23 => 23,
            InputSource::Ani24 => 24,
            InputSource::Ani25 => 25,
            InputSource::Ani26 => 26,
            InputSource::Tscap => 30,
            InputSource::Sensor => 0x80,
            InputSource::Internal => 0x81,
        }
    }

    fn read_adcrh(&self) -> u8 {
        match self.resolution {
            Resolution::Bits12 => ((self.adcr >> 4) & 0xFF) as u8,
            _ => ((self.adcr >> 8) & 0xFF) as u8,
        }
    }

    fn read_adcrn(&self, select: usize) -> u16 {
        self.scan_adcr.get(select).copied().unwrap_or(0)
    }

    fn read_adcrnh(&self, select: usize) -> u8 {
        let value = self.read_adcrn(select);
        match self.resolution {
            Resolution::Bits12 => ((value >> 4) & 0xFF) as u8,
            _ => ((value >> 8) & 0xFF) as u8,
        }
    }

    fn read_adcr_window(&self, offset: u64, buf: &mut [u8]) -> Result<(), BusError> {
        match (offset, buf.len()) {
            (0, 2) => {
                buf.copy_from_slice(&self.adcr.to_le_bytes());
                Ok(())
            }
            (1, 1) => {
                buf[0] = self.read_adcrh();
                Ok(())
            }
            _ => Err(oob(offset, buf.len())),
        }
    }

    fn read_adm_window(&self, offset: u64, buf: &mut [u8]) -> Result<(), BusError> {
        if buf.len() != 1 {
            return Err(oob(offset, buf.len()));
        }
        buf[0] = match offset {
            0 => self.read_adm0(),
            1 => self.read_ads(),
            2 => self.read_adm1(),
            _ => return Err(oob(offset, buf.len())),
        };
        Ok(())
    }

    fn read_ext_window(&self, offset: u64, buf: &mut [u8]) -> Result<(), BusError> {
        match (offset, buf.len()) {
            (0, 1) => {
                buf[0] = self.read_adm2();
                Ok(())
            }
            (1, 1) => {
                buf[0] = self.adul;
                Ok(())
            }
            (2, 1) => {
                buf[0] = self.adll;
                Ok(())
            }
            (3, 1) => {
                buf[0] = match self.test_target {
                    TestTarget::Normal => 0,
                    TestTarget::GndRef => 2,
                    TestTarget::VddRef => 3,
                };
                Ok(())
            }
            (0x10 | 0x12 | 0x14 | 0x16, 2) => {
                let select = ((offset - 0x10) / 2) as usize;
                buf.copy_from_slice(&self.read_adcrn(select).to_le_bytes());
                Ok(())
            }
            (0x11 | 0x13 | 0x15 | 0x17, 1) => {
                let select = ((offset - 0x11) / 2) as usize;
                buf[0] = self.read_adcrnh(select);
                Ok(())
            }
            (o, 1) if o < AdcWindow::Ext.len() => {
                buf[0] = 0;
                Ok(())
            }
            _ => Err(oob(offset, buf.len())),
        }
    }

    fn write_adm_window(
        &mut self,
        offset: u64,
        buf: &[u8],
        inner: &Arc<Mutex<AdcUnit>>,
    ) -> Result<(), BusError> {
        if buf.len() != 1 {
            return Err(oob(offset, buf.len()));
        }
        match offset {
            0 => self.write_adm0(buf[0], inner),
            1 => self.write_ads(buf[0], inner),
            2 => self.write_adm1(buf[0]),
            _ => return Err(oob(offset, buf.len())),
        }
        Ok(())
    }

    fn write_ext_window(&mut self, offset: u64, buf: &[u8]) -> Result<(), BusError> {
        match (offset, buf.len()) {
            (0, 1) => self.write_adm2(buf[0]),
            (1, 1) => self.write_adul(buf[0]),
            (2, 1) => self.write_adll(buf[0]),
            (3, 1) => self.write_adtes(buf[0]),
            (0x10 | 0x12 | 0x14 | 0x16, 2) | (0x11 | 0x13 | 0x15 | 0x17, 1) => {}
            (o, 1) if o < AdcWindow::Ext.len() => {}
            _ => return Err(oob(offset, buf.len())),
        }
        Ok(())
    }
}

impl Resettable for AdcUnit {
    fn reset(&mut self, _bus: &mut MemoryBus) {
        self.restore_reset();
    }
}

struct AdcComplete {
    inner: Arc<Mutex<AdcUnit>>,
    generation: u32,
    at: Tick,
}

impl SimEvent for AdcComplete {
    fn fire(&mut self, _ctx: &mut EventCtx<'_>) {
        let mut g = self.inner.lock().expect("adc");
        if g.generation != self.generation || !g.conversion_running {
            g.expire_id = None;
            return;
        }
        let inner = Arc::clone(&self.inner);
        g.complete(&inner, self.at);
    }
}

pub(crate) struct AdcMmio {
    inner: Arc<Mutex<AdcUnit>>,
    window: AdcWindow,
}

impl AdcMmio {
    #[must_use]
    pub(crate) fn new(inner: Arc<Mutex<AdcUnit>>, window: AdcWindow) -> Self {
        Self { inner, window }
    }
}

impl MemoryMapped for AdcMmio {
    fn len(&self) -> u64 {
        self.window.len()
    }

    fn read(&mut self, offset: u64, buf: &mut [u8]) -> Result<(), BusError> {
        let g = self.inner.lock().expect("adc");
        match self.window {
            AdcWindow::Adcr => g.read_adcr_window(offset, buf),
            AdcWindow::Adm => g.read_adm_window(offset, buf),
            AdcWindow::Ext => g.read_ext_window(offset, buf),
        }
    }

    fn write(&mut self, offset: u64, buf: &[u8]) -> Result<(), BusError> {
        let mut g = self.inner.lock().expect("adc");
        let inner = Arc::clone(&self.inner);
        match self.window {
            AdcWindow::Adcr => match (offset, buf.len()) {
                (0, 2) | (1, 1) => Ok(()),
                _ => Err(oob(offset, buf.len())),
            },
            AdcWindow::Adm => g.write_adm_window(offset, buf, &inner),
            AdcWindow::Ext => g.write_ext_window(offset, buf),
        }
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
    use crate::peripherals::clock::ClockGenerator;
    use sim_kernel::{EventCtl, EventCtx, MemoryBus, MemoryMapBuilder, Resettable, Tick, Wire};

    const ADCR: u64 = 0xFFF1E;
    const ADM0: u64 = 0xFFF30;
    const ADS: u64 = 0xFFF31;
    const ADM1: u64 = 0xFFF32;
    const ADM2: u64 = 0xF0010;
    const ADUL: u64 = 0xF0011;
    const ADLL: u64 = 0xF0012;
    const ADTES: u64 = 0xF0013;
    const ADCR0: u64 = 0xF0020;

    fn running_clock() -> ClockOutputs {
        let mut clock = ClockGenerator::new();
        Resettable::reset(&mut clock, &mut MemoryBus::new());
        clock.outputs()
    }

    fn mapped(adc: &Arc<Mutex<AdcUnit>>) -> MemoryBus {
        MemoryMapBuilder::new()
            .map(
                ADCR,
                Box::new(AdcMmio::new(Arc::clone(adc), AdcWindow::Adcr)),
            )
            .unwrap()
            .map(
                ADM0,
                Box::new(AdcMmio::new(Arc::clone(adc), AdcWindow::Adm)),
            )
            .unwrap()
            .map(
                ADM2,
                Box::new(AdcMmio::new(Arc::clone(adc), AdcWindow::Ext)),
            )
            .unwrap()
            .build()
            .unwrap()
    }

    fn read_u8(bus: &mut MemoryBus, addr: u64) -> u8 {
        let mut buf = [0u8; 1];
        bus.read(addr, &mut buf).unwrap();
        buf[0]
    }

    fn read_u16(bus: &mut MemoryBus, addr: u64) -> u16 {
        let mut buf = [0u8; 2];
        bus.read(addr, &mut buf).unwrap();
        u16::from_le_bytes(buf)
    }

    fn pump(bus: &mut MemoryBus, ctl: &EventCtl, now: Tick) {
        ctl.set_now(now);
        loop {
            let due = ctl.events().pop_due(now);
            let Some((_, mut event)) = due else {
                break;
            };
            let mut ctx = EventCtx {
                now,
                bus,
                stop: None,
            };
            event.fire(&mut ctx);
        }
    }

    #[test]
    fn reset_values_match_hardware() {
        let adc = Arc::new(Mutex::new(AdcUnit::new(EventCtl::new(), running_clock())));
        let mut bus = mapped(&adc);
        assert_eq!(read_u8(&mut bus, ADM0), 0x00);
        assert_eq!(read_u8(&mut bus, ADM1), 0x00);
        assert_eq!(read_u8(&mut bus, ADM2), 0x00);
        assert_eq!(read_u8(&mut bus, ADS), 0x00);
        assert_eq!(read_u8(&mut bus, ADUL), 0xFF);
        assert_eq!(read_u8(&mut bus, ADLL), 0x00);
        assert_eq!(read_u8(&mut bus, ADTES), 0x00);
        assert_eq!(read_u16(&mut bus, ADCR), 0);
        bus.write(ADM1, &[0x20]).unwrap();
        Resettable::reset(&mut *adc.lock().unwrap(), &mut bus);
        assert_eq!(read_u8(&mut bus, ADM1), 0x00);
    }

    #[test]
    fn select_oneshot_converts_midscale() {
        let ctl = EventCtl::new();
        let adc = Arc::new(Mutex::new(AdcUnit::new(ctl.clone(), running_clock())));
        adc.lock().unwrap().set_voltage(0, AnalogVoltage(2_500_000));
        let mut bus = mapped(&adc);
        bus.write(ADM0, &[0x00]).unwrap();
        bus.write(ADM1, &[0x20]).unwrap();
        bus.write(ADM2, &[0x00]).unwrap();
        bus.write(ADS, &[0x00]).unwrap();
        bus.write(ADM0, &[0x80]).unwrap();
        pump(&mut bus, &ctl, Tick(1));
        assert_eq!(read_u8(&mut bus, ADM0), 0x80);
        pump(&mut bus, &ctl, Tick(72_000));
        assert_eq!(read_u8(&mut bus, ADM0), 0x00);
        assert_eq!(read_u16(&mut bus, ADCR) >> 6, 0x0200);
    }

    #[test]
    fn scan_oneshot_clears_adcs_after_four() {
        let ctl = EventCtl::new();
        let adc = Arc::new(Mutex::new(AdcUnit::new(ctl.clone(), running_clock())));
        for i in 0..4 {
            adc.lock()
                .unwrap()
                .set_voltage(i, AnalogVoltage(i64::from(i as u32) * 1_250_000));
        }
        let mut bus = mapped(&adc);
        bus.write(ADM0, &[0x40]).unwrap();
        bus.write(ADM1, &[0x20]).unwrap();
        bus.write(ADS, &[0x00]).unwrap();
        bus.write(ADM0, &[0xC0]).unwrap();
        pump(&mut bus, &ctl, Tick(1));
        assert_eq!(read_u8(&mut bus, ADM0), 0xC0);
        pump(&mut bus, &ctl, Tick(72_000));
        assert_eq!(read_u8(&mut bus, ADM0), 0xC0);
        pump(&mut bus, &ctl, Tick(140_000));
        assert_eq!(read_u8(&mut bus, ADM0), 0xC0);
        pump(&mut bus, &ctl, Tick(208_000));
        assert_eq!(read_u8(&mut bus, ADM0), 0xC0);
        pump(&mut bus, &ctl, Tick(276_000));
        assert_eq!(read_u8(&mut bus, ADM0), 0x40);
        assert_eq!(read_u16(&mut bus, ADCR0) >> 6, 0);
        assert_eq!(read_u16(&mut bus, ADCR0 + 2) >> 6, 0x0100);
    }

    #[test]
    fn select_continuous_keeps_adcs() {
        let ctl = EventCtl::new();
        let adc = Arc::new(Mutex::new(AdcUnit::new(ctl.clone(), running_clock())));
        let mut bus = mapped(&adc);
        bus.write(ADM0, &[0x00]).unwrap();
        bus.write(ADM1, &[0x00]).unwrap();
        bus.write(ADM0, &[0x80]).unwrap();
        pump(&mut bus, &ctl, Tick(1));
        pump(&mut bus, &ctl, Tick(72_000));
        assert_eq!(read_u8(&mut bus, ADM0), 0x80);
        pump(&mut bus, &ctl, Tick(140_000));
        assert_eq!(read_u8(&mut bus, ADM0), 0x80);
    }

    #[test]
    fn analog_wire_updates_channel() {
        let ctl = EventCtl::new();
        let adc = Arc::new(Mutex::new(AdcUnit::new(ctl.clone(), running_clock())));
        let mut src = SourcePort::new();
        let sink = Arc::clone(&adc);
        let _wire = Wire::new()
            .source(&mut src)
            .sink(move |values, changed| AdcUnit::on_channel(&sink, 1, values, changed));
        src.drive(AnalogVoltage(5_000_000));
        let mut bus = mapped(&adc);
        bus.write(ADM1, &[0x20]).unwrap();
        bus.write(ADS, &[0x01]).unwrap();
        bus.write(ADM0, &[0x80]).unwrap();
        pump(&mut bus, &ctl, Tick(72_000));
        assert_eq!(read_u16(&mut bus, ADCR) >> 6, 0x03FF);
    }

    #[test]
    fn oneshot_pulses_irq() {
        let ctl = EventCtl::new();
        let adc = Arc::new(Mutex::new(AdcUnit::new(ctl.clone(), running_clock())));
        adc.lock().unwrap().set_voltage(0, AnalogVoltage(2_500_000));
        let seen = Arc::new(Mutex::new(0u32));
        let recorded = Arc::clone(&seen);
        let _irq = {
            let mut g = adc.lock().unwrap();
            Wire::new()
                .source(g.irq_source())
                .sink(move |_, _| *recorded.lock().unwrap() += 1)
        };
        let mut bus = mapped(&adc);
        bus.write(ADM1, &[0x20]).unwrap();
        bus.write(ADM0, &[0x80]).unwrap();
        pump(&mut bus, &ctl, Tick(72_000));
        assert_eq!(*seen.lock().unwrap(), 1);
    }
}
