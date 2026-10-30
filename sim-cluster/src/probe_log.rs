//! Node-side probe history and the 100ms coarse window.
//!
//! Detail stays until the simulation is deleted. Each probe keeps at most
//! [`PROBE_MEM_CAP`] samples in memory; older ones are appended to a file
//! under the temp directory. A failed append marks the log truncated and
//! later samples are dropped. The simulation itself keeps running.

use std::collections::{HashMap, VecDeque};
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

/// Samples kept in memory for one probe before the oldest is spilled.
pub const PROBE_MEM_CAP: usize = 4096;

/// Wall-clock period of the coarse stream. Not virtual time.
pub const COARSE_INTERVAL: Duration = Duration::from_millis(100);

const SPILL_REC: usize = 8 + 8 + 1 + 2;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ProbeKind {
    Digital,
    Uart,
    Analog,
}

impl ProbeKind {
    #[must_use]
    pub fn from_wire(kind: u8) -> Option<Self> {
        match kind {
            1 => Some(Self::Digital),
            2 => Some(Self::Uart),
            3 => Some(Self::Analog),
            _ => None,
        }
    }

    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Digital => "digital",
            Self::Uart => "uart",
            Self::Analog => "analog",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProbePointName {
    pub probe_id: u64,
    pub board_id: String,
    pub name: String,
    pub kind: ProbeKind,
}

/// One stored sample. `value` is a [`sim_kernel::DigitalLevel`] as `u16`, a UART
/// data word, or analog millivolts (`µV / 1000`, clamped to `u16`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ProbeRecord {
    pub probe_id: u64,
    pub virtual_time_ns: u64,
    pub kind: ProbeKind,
    pub value: u16,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct ProbeView {
    pub virtual_time_ns: u64,
    pub board: String,
    pub probe: String,
    pub kind: String,
    pub value: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct CoarseBatch {
    pub seq: u64,
    pub samples: Vec<ProbeView>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProbeRef {
    pub board: String,
    pub name: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct ProbeCatalogItem {
    pub board: String,
    pub probe: String,
    pub kind: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct ProbeWatchView {
    pub probes: Vec<ProbeRef>,
}

/// One time-sliced CSV drain from the worker.
#[derive(Clone, Debug)]
pub struct CsvPoll {
    pub records: Vec<ProbeRecord>,
    pub done: bool,
    pub failed: bool,
    pub truncated: bool,
}

/// Virtual-time window for a detail export.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ProbeRange {
    pub from_ns: Option<u64>,
    pub to_ns: Option<u64>,
}

impl ProbeRange {
    /// `to` without `from` is rejected. Both absent selects the whole log.
    pub fn parse(from_ns: Option<&str>, to_ns: Option<&str>) -> Result<Self, &'static str> {
        let from_ns = parse_ns(from_ns)?;
        let to_ns = parse_ns(to_ns)?;
        if to_ns.is_some() && from_ns.is_none() {
            return Err("end without start");
        }
        if let (Some(from_ns), Some(to_ns)) = (from_ns, to_ns)
            && from_ns > to_ns
        {
            return Err("start is after end");
        }
        Ok(Self { from_ns, to_ns })
    }

    #[must_use]
    pub fn contains(self, virtual_time_ns: u64) -> bool {
        if let Some(from_ns) = self.from_ns
            && virtual_time_ns < from_ns
        {
            return false;
        }
        if let Some(to_ns) = self.to_ns
            && virtual_time_ns > to_ns
        {
            return false;
        }
        true
    }
}

fn parse_ns(text: Option<&str>) -> Result<Option<u64>, &'static str> {
    let Some(text) = text.map(str::trim).filter(|text| !text.is_empty()) else {
        return Ok(None);
    };
    text.parse::<u64>()
        .map(Some)
        .map_err(|_| "invalid virtual time")
}

/// Snapshot the worker can hand to the HTTP thread without holding the log.
#[derive(Clone, Debug)]
pub struct ProbeExport {
    pub truncated: bool,
    pub path: PathBuf,
    pub file_len: u64,
    pub memory: Vec<ProbeRecord>,
    pub catalog: Vec<ProbePointName>,
}

pub struct ProbeLog {
    cap: usize,
    catalog: HashMap<u64, ProbePointName>,
    memory: HashMap<u64, VecDeque<ProbeRecord>>,
    spill_path: PathBuf,
    spill: Option<File>,
    truncated: bool,
    running: bool,
    coarse_since: Instant,
    digital_last: HashMap<u64, ProbeRecord>,
    analog_last: HashMap<u64, ProbeRecord>,
    uart_window: Vec<ProbeRecord>,
    latest: HashMap<u64, ProbeRecord>,
}

impl ProbeLog {
    #[must_use]
    pub fn open(sim_id: &str, board_id: &str) -> Self {
        Self::with_cap(sim_id, board_id, PROBE_MEM_CAP)
    }

    #[must_use]
    pub fn with_cap(sim_id: &str, board_id: &str, cap: usize) -> Self {
        let board_hash = crate::ipc::board_id_hash(board_id);
        let spill_path =
            std::env::temp_dir().join(format!("sim-cluster-probes-{sim_id}-{board_hash:016x}.bin"));
        Self {
            cap: cap.max(1),
            catalog: HashMap::new(),
            memory: HashMap::new(),
            spill_path,
            spill: None,
            truncated: false,
            running: false,
            coarse_since: Instant::now(),
            digital_last: HashMap::new(),
            analog_last: HashMap::new(),
            uart_window: Vec::new(),
            latest: HashMap::new(),
        }
    }

    pub fn note_catalog(&mut self, point: ProbePointName) {
        self.catalog.insert(point.probe_id, point);
    }

    /// While `running` is false, samples are stored but not placed in the coarse window.
    /// Leaving `running` returns whatever the open window still holds.
    pub fn set_running(&mut self, running: bool) -> Vec<ProbeRecord> {
        let flushed = if self.running && !running {
            self.emit_coarse()
        } else {
            Vec::new()
        };
        if !self.running && running {
            self.coarse_since = Instant::now();
            self.digital_last.clear();
            self.analog_last.clear();
            self.uart_window.clear();
        }
        self.running = running;
        flushed
    }

    pub fn push(&mut self, record: ProbeRecord) {
        if self.truncated && self.at_cap(record.probe_id) {
            return;
        }
        if self.at_cap(record.probe_id) && !self.spill_oldest(record.probe_id) {
            self.truncated = true;
            return;
        }
        self.memory
            .entry(record.probe_id)
            .or_default()
            .push_back(record);
        self.latest.insert(record.probe_id, record);
        if self.running {
            match record.kind {
                ProbeKind::Digital => {
                    self.digital_last.insert(record.probe_id, record);
                }
                ProbeKind::Analog => {
                    self.analog_last.insert(record.probe_id, record);
                }
                ProbeKind::Uart => self.uart_window.push(record),
            }
        }
    }

    pub fn poll_coarse(&mut self, now: Instant) -> Vec<ProbeRecord> {
        if !self.running {
            return Vec::new();
        }
        if now.duration_since(self.coarse_since) < COARSE_INTERVAL {
            return Vec::new();
        }
        let records = self.emit_coarse();
        self.coarse_since = now;
        records
    }

    #[must_use]
    pub fn latest_of(&self, ids: &[u64]) -> Vec<ProbeRecord> {
        let mut records: Vec<ProbeRecord> = ids
            .iter()
            .filter_map(|id| self.latest.get(id).copied())
            .collect();
        records.sort_by_key(|rec| (rec.virtual_time_ns, rec.probe_id));
        records
    }

    #[must_use]
    pub fn latest_all(&self) -> Vec<ProbeRecord> {
        let mut records: Vec<ProbeRecord> = self.latest.values().copied().collect();
        records.sort_by_key(|rec| (rec.virtual_time_ns, rec.probe_id));
        records
    }

    #[must_use]
    pub fn truncated(&self) -> bool {
        self.truncated
    }

    /// Memory samples plus the spill length at this instant.
    ///
    /// The file is append-only. Readers must ignore bytes past `file_len`.
    pub fn export(&mut self) -> io::Result<ProbeExport> {
        if let Some(file) = self.spill.as_mut() {
            file.flush()?;
        }
        let file_len = fs::metadata(&self.spill_path)
            .map(|meta| meta.len())
            .unwrap_or(0);
        let mut memory = Vec::new();
        for queue in self.memory.values() {
            memory.extend(queue.iter().copied());
        }
        Ok(ProbeExport {
            truncated: self.truncated,
            path: self.spill_path.clone(),
            file_len,
            memory,
            catalog: self.catalog.values().cloned().collect(),
        })
    }

    fn at_cap(&self, probe_id: u64) -> bool {
        self.memory
            .get(&probe_id)
            .is_some_and(|queue| queue.len() >= self.cap)
    }

    fn spill_oldest(&mut self, probe_id: u64) -> bool {
        let Some(oldest) = self
            .memory
            .get_mut(&probe_id)
            .and_then(|queue| queue.pop_front())
        else {
            return true;
        };
        if self.append_spill(&oldest) {
            true
        } else {
            self.memory.entry(probe_id).or_default().push_front(oldest);
            false
        }
    }

    fn append_spill(&mut self, record: &ProbeRecord) -> bool {
        if self.spill.is_none() {
            if let Some(parent) = self.spill_path.parent()
                && !parent.as_os_str().is_empty()
                && fs::create_dir_all(parent).is_err()
            {
                return false;
            }
            match OpenOptions::new()
                .create(true)
                .append(true)
                .open(&self.spill_path)
            {
                Ok(file) => self.spill = Some(file),
                Err(_) => return false,
            }
        }
        let Some(file) = self.spill.as_mut() else {
            return false;
        };
        let mut buf = [0u8; SPILL_REC];
        buf[0..8].copy_from_slice(&record.virtual_time_ns.to_le_bytes());
        buf[8..16].copy_from_slice(&record.probe_id.to_le_bytes());
        buf[16] = match record.kind {
            ProbeKind::Digital => 1,
            ProbeKind::Uart => 2,
            ProbeKind::Analog => 3,
        };
        buf[17..19].copy_from_slice(&record.value.to_le_bytes());
        file.write_all(&buf).is_ok()
    }

    fn emit_coarse(&mut self) -> Vec<ProbeRecord> {
        let mut records: Vec<ProbeRecord> = self.digital_last.drain().map(|(_, rec)| rec).collect();
        records.extend(self.analog_last.drain().map(|(_, rec)| rec));
        records.append(&mut self.uart_window);
        if records.is_empty() {
            return Vec::new();
        }
        records.sort_by_key(|rec| (rec.virtual_time_ns, rec.probe_id));
        records
    }
}

#[must_use]
pub fn record_view(catalog: &HashMap<u64, ProbePointName>, record: &ProbeRecord) -> ProbeView {
    let (board, probe) = catalog
        .get(&record.probe_id)
        .map(|point| (point.board_id.clone(), point.name.clone()))
        .unwrap_or_else(|| (String::new(), format!("{:016x}", record.probe_id)));
    ProbeView {
        virtual_time_ns: record.virtual_time_ns,
        board,
        probe,
        kind: record.kind.as_str().to_string(),
        value: display_value(record.kind, record.value),
    }
}

impl Drop for ProbeLog {
    fn drop(&mut self) {
        self.spill.take();
        let _ = fs::remove_file(&self.spill_path);
    }
}

#[must_use]
pub fn display_value(kind: ProbeKind, value: u16) -> String {
    match kind {
        ProbeKind::Digital => match value {
            0 => "HiZ",
            1 => "High",
            2 => "Low",
            _ => "HiZ",
        }
        .to_string(),
        ProbeKind::Uart => value.to_string(),
        ProbeKind::Analog => value.to_string(),
    }
}

/// JSON `value` for `/probes/samples`: digital is a level name, uart/analog are numbers.
#[must_use]
pub fn json_value(kind: ProbeKind, value: u16) -> serde_json::Value {
    match kind {
        ProbeKind::Digital => serde_json::Value::String(display_value(kind, value)),
        ProbeKind::Uart | ProbeKind::Analog => value.into(),
    }
}

/// Read spilled records up to `file_len`, then the in-memory snapshot.
pub fn load_export(export: &ProbeExport) -> io::Result<Vec<ProbeRecord>> {
    let mut records = read_spill(&export.path, export.file_len)?;
    records.extend(export.memory.iter().copied());
    Ok(records)
}

fn read_spill(path: &Path, file_len: u64) -> io::Result<Vec<ProbeRecord>> {
    if file_len == 0 || !path.exists() {
        return Ok(Vec::new());
    }
    let mut file = File::open(path)?;
    let mut buf = vec![0u8; usize::try_from(file_len).unwrap_or(usize::MAX)];
    let mut filled = 0;
    while filled < buf.len() {
        match file.read(&mut buf[filled..])? {
            0 => break,
            n => filled += n,
        }
    }
    let mut records = Vec::new();
    let mut offset = 0;
    while offset + SPILL_REC <= filled {
        let virtual_time_ns = u64::from_le_bytes(buf[offset..offset + 8].try_into().unwrap());
        let probe_id = u64::from_le_bytes(buf[offset + 8..offset + 16].try_into().unwrap());
        let kind = ProbeKind::from_wire(buf[offset + 16]).unwrap_or(ProbeKind::Digital);
        let value = u16::from_le_bytes(buf[offset + 17..offset + 19].try_into().unwrap());
        records.push(ProbeRecord {
            probe_id,
            virtual_time_ns,
            kind,
            value,
        });
        offset += SPILL_REC;
    }
    Ok(records)
}

#[must_use]
pub fn csv_header() -> String {
    "virtual_time_ns,board,probe,kind,value\n".into()
}

#[must_use]
pub fn csv_record_line(catalog: &[ProbePointName], rec: &ProbeRecord) -> String {
    let point = catalog.iter().find(|point| point.probe_id == rec.probe_id);
    let board = point.map(|p| p.board_id.as_str()).unwrap_or("");
    let probe = point.map(|p| p.name.as_str()).unwrap_or("");
    let probe = if probe.is_empty() {
        format!("{:016x}", rec.probe_id)
    } else {
        probe.to_string()
    };
    format!(
        "{},{},{},{},{}\n",
        rec.virtual_time_ns,
        csv_field(board),
        csv_field(&probe),
        rec.kind.as_str(),
        csv_field(&display_value(rec.kind, rec.value)),
    )
}

#[must_use]
#[allow(dead_code)]
pub fn render_csv(
    catalog: &[ProbePointName],
    records: &[ProbeRecord],
    range: ProbeRange,
) -> String {
    let mut out = csv_header();
    for rec in records {
        if range.contains(rec.virtual_time_ns) {
            out.push_str(&csv_record_line(catalog, rec));
        }
    }
    out
}

fn csv_field(text: &str) -> String {
    if text.contains([',', '"', '\n', '\r']) {
        format!("\"{}\"", text.replace('"', "\"\""))
    } else {
        text.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn point(id: u64, kind: ProbeKind) -> ProbePointName {
        ProbePointName {
            probe_id: id,
            board_id: "board".into(),
            name: format!("p{id}"),
            kind,
        }
    }

    fn rec(id: u64, vt: u64, kind: ProbeKind, value: u16) -> ProbeRecord {
        ProbeRecord {
            probe_id: id,
            virtual_time_ns: vt,
            kind,
            value,
        }
    }

    #[test]
    fn range_rejects_end_without_start() {
        assert!(ProbeRange::parse(None, Some("10")).is_err());
        let all = ProbeRange::parse(None, None).unwrap();
        assert!(all.contains(0) && all.contains(u64::MAX));
        let tail = ProbeRange::parse(Some("5"), None).unwrap();
        assert!(!tail.contains(4) && tail.contains(5));
        let window = ProbeRange::parse(Some("5"), Some("7")).unwrap();
        assert!(window.contains(5) && window.contains(7) && !window.contains(8));
    }

    #[test]
    fn coarse_keeps_last_digital_and_every_uart_word() {
        let mut log = ProbeLog::with_cap("coarse-test", "board", 8);
        log.note_catalog(point(1, ProbeKind::Digital));
        log.note_catalog(point(2, ProbeKind::Uart));
        log.set_running(true);
        log.push(rec(1, 10, ProbeKind::Digital, 2));
        log.push(rec(1, 20, ProbeKind::Digital, 1));
        log.push(rec(2, 15, ProbeKind::Uart, 0x41));
        log.push(rec(2, 16, ProbeKind::Uart, 0x1FF));
        log.coarse_since = Instant::now()
            .checked_sub(COARSE_INTERVAL)
            .unwrap_or_else(Instant::now);
        let records = log.poll_coarse(Instant::now());
        let values: Vec<_> = records
            .iter()
            .map(|rec| (rec.probe_id, rec.value))
            .collect();
        assert_eq!(values, vec![(2, 0x41), (2, 0x1FF), (1, 1)]);
        log.coarse_since = Instant::now()
            .checked_sub(COARSE_INTERVAL)
            .unwrap_or_else(Instant::now);
        assert!(log.poll_coarse(Instant::now()).is_empty());
        let latest = log.latest_all();
        let latest_ids: Vec<_> = latest.iter().map(|rec| (rec.probe_id, rec.value)).collect();
        assert_eq!(latest_ids, vec![(2, 0x1FF), (1, 1)]);
        assert_eq!(log.latest_of(&[1]).len(), 1);
    }

    #[test]
    fn spill_keeps_oldest_on_disk_and_csv_is_ordered() {
        let mut log = ProbeLog::with_cap("spill-test", "board", 2);
        log.note_catalog(point(1, ProbeKind::Digital));
        log.push(rec(1, 1, ProbeKind::Digital, 0));
        log.push(rec(1, 2, ProbeKind::Digital, 1));
        log.push(rec(1, 3, ProbeKind::Digital, 2));
        let export = log.export().unwrap();
        let loaded = load_export(&export).unwrap();
        let csv = render_csv(
            &export.catalog,
            &loaded,
            ProbeRange::parse(None, None).unwrap(),
        );
        assert!(csv.contains("1,board,p1,digital,HiZ\n"));
        assert!(csv.contains("2,board,p1,digital,High\n"));
        assert!(csv.contains("3,board,p1,digital,Low\n"));
        let hi = csv.find("High").unwrap();
        let low = csv.find("Low").unwrap();
        assert!(hi < low);
    }

    #[test]
    fn disk_failure_drops_later_samples() {
        let mut log = ProbeLog::with_cap("fail-test", "board", 1);
        log.spill_path = PathBuf::from("/dev/null/sim-cluster-probes-fail.bin");
        log.push(rec(1, 1, ProbeKind::Digital, 1));
        log.push(rec(1, 2, ProbeKind::Digital, 2));
        assert!(log.truncated);
        let export = log.export().unwrap();
        assert_eq!(export.memory.len(), 1);
        assert_eq!(export.memory[0].virtual_time_ns, 1);
    }

    #[test]
    fn json_value_types_match_kind() {
        assert_eq!(json_value(ProbeKind::Digital, 1), serde_json::json!("High"));
        assert_eq!(json_value(ProbeKind::Uart, 65), serde_json::json!(65));
        assert_eq!(json_value(ProbeKind::Analog, 3300), serde_json::json!(3300));
    }
}
