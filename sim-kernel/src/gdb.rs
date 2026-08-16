//! GDB Remote Serial Protocol stub.
//!
//! Uses [`crate::Command`] / [`crate::Response`] only. [`Command::NotifyHalt`] is
//! issued on guest stop but is a no-op until a multi-board arbiter exists.

use std::collections::HashMap;
use std::io::{self, Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream, ToSocketAddrs};
use std::thread;
use std::time::Duration;

use crate::breakpoint::BreakpointId;
use crate::command::{Command, InspectResult, Response};
use crate::cpu::RegId;
use crate::sim::{SimControl, SimEvents};
use crate::stop::StopReason;

const GDB_REG_COUNT: u32 = 18;
const GDB_REG_BYTES: usize = 4;
const PACKET_MAX: usize = 4096;

const TARGET_XML: &str = r#"<?xml version="1.0"?>
<!DOCTYPE target SYSTEM "gdb-target.dtd">
<target version="1.0">
  <architecture>rl78</architecture>
  <feature name="org.gnu.gdb.rl78.core">
    <reg name="x" bitsize="32"/>
    <reg name="a" bitsize="32"/>
    <reg name="c" bitsize="32"/>
    <reg name="b" bitsize="32"/>
    <reg name="e" bitsize="32"/>
    <reg name="d" bitsize="32"/>
    <reg name="l" bitsize="32"/>
    <reg name="h" bitsize="32"/>
    <reg name="pc" bitsize="32" type="code_ptr"/>
    <reg name="sp" bitsize="32" type="data_ptr"/>
    <reg name="es" bitsize="32"/>
    <reg name="cs" bitsize="32"/>
    <reg name="psw_cy" bitsize="32"/>
    <reg name="psw_isp" bitsize="32"/>
    <reg name="psw_rbs" bitsize="32"/>
    <reg name="psw_ac" bitsize="32"/>
    <reg name="psw_z" bitsize="32"/>
    <reg name="psw_ie" bitsize="32"/>
  </feature>
</target>
"#;

/// Failed to parse a QEMU-style `-gdb` device string.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GdbBindError(pub String);

impl std::fmt::Display for GdbBindError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl std::error::Error for GdbBindError {}

/// Parse QEMU `-gdb` device forms used in v1: `tcp::PORT` and `tcp:HOST:PORT`.
pub fn parse_gdb_dev(dev: &str) -> Result<SocketAddr, GdbBindError> {
    let rest = dev
        .strip_prefix("tcp:")
        .ok_or_else(|| GdbBindError(format!("unsupported gdb device `{dev}` (want tcp::PORT)")))?;
    let addr = if let Some(port) = rest.strip_prefix(':') {
        format!("127.0.0.1:{port}")
    } else {
        rest.to_string()
    };
    addr.to_socket_addrs()
        .map_err(|err| GdbBindError(err.to_string()))?
        .next()
        .ok_or_else(|| GdbBindError(format!("could not resolve `{addr}`")))
}

/// Listen for a GDB client. Serves one connection at a time, then accepts again
/// until the simulation thread goes away.
pub fn listen_gdb(
    ctrl: SimControl,
    bind: SocketAddr,
) -> io::Result<(SocketAddr, thread::JoinHandle<()>)> {
    let listener = TcpListener::bind(bind)?;
    let local = listener.local_addr()?;
    listener.set_nonblocking(false)?;
    let handle = thread::Builder::new()
        .name("gdb".into())
        .spawn(move || gdb_accept_loop(ctrl, listener))
        .expect("failed to spawn gdb thread");
    Ok((local, handle))
}

fn gdb_accept_loop(ctrl: SimControl, listener: TcpListener) {
    while let Ok((stream, _)) = listener.accept() {
        let events = ctrl.subscribe();
        let mut session = GdbSession {
            ctrl: ctrl.clone(),
            events,
            breakpoints: HashMap::new(),
        };
        let _ = session.serve(stream);
    }
}

struct GdbSession {
    ctrl: SimControl,
    events: SimEvents,
    breakpoints: HashMap<u64, BreakpointId>,
}

impl GdbSession {
    fn serve(&mut self, mut stream: TcpStream) -> io::Result<()> {
        stream.set_nodelay(true)?;
        loop {
            let packet = match read_packet(&mut stream)? {
                Some(p) => p,
                None => return Ok(()),
            };
            match packet {
                Incoming::Interrupt => {
                    let _ = self.ctrl.stop();
                    let _ = self.wait_stopped(&mut stream, Duration::from_secs(2));
                    write_packet(&mut stream, "S02")?;
                }
                Incoming::Command(body) => {
                    let reply = if is_resume_packet(&body) {
                        self.resume_on(&mut stream, resume_is_step(&body))
                    } else {
                        self.dispatch(&body)
                    };
                    write_packet(&mut stream, &reply)?;
                    if body == "k" || body == "D" {
                        return Ok(());
                    }
                }
            }
        }
    }

    fn dispatch(&mut self, body: &str) -> String {
        if body.is_empty() || body == "vMustReplyEmpty" {
            return String::new();
        }
        match body.as_bytes()[0] {
            b'?' => "S05".into(),
            b'g' if body.len() == 1 => self.read_all_regs(),
            b'G' => self.write_all_regs(&body[1..]),
            b'm' => self.read_mem(&body[1..]),
            b'M' => self.write_mem(&body[1..]),
            b'H' => "OK".into(),
            b'k' | b'D' => "OK".into(),
            _ if body.starts_with("Z0,") => self.add_sw_break(&body[3..]),
            _ if body.starts_with("z0,") => self.remove_sw_break(&body[3..]),
            _ if body.starts_with("vCont") => self.vcont_query_or_halt(&body["vCont".len()..]),
            _ if body.starts_with("qSupported") => {
                format!("PacketSize={PACKET_MAX:x};qXfer:features:read+;vContSupported+")
            }
            _ if body.starts_with("qXfer:features:read:target.xml:") => {
                xfer_xml(&body["qXfer:features:read:target.xml:".len()..])
            }
            _ if body.starts_with("qTStatus") || body.starts_with("qC") => "OK".into(),
            _ => String::new(),
        }
    }

    fn vcont_query_or_halt(&mut self, rest: &str) -> String {
        if rest == "?" {
            return "vCont;c;s;t".into();
        }
        String::new()
    }

    fn resume_on(&mut self, stream: &mut TcpStream, step: bool) -> String {
        let send = if step {
            self.ctrl.step()
        } else {
            self.ctrl.start()
        };
        if send.is_err() {
            return "E01".into();
        }
        self.wait_and_notify(stream)
    }

    fn wait_and_notify(&mut self, stream: &mut TcpStream) -> String {
        match self.wait_stopped(stream, Duration::from_secs(3600)) {
            Some(reason) => {
                let _ = self.ctrl.send(Command::NotifyHalt {
                    reason: reason.clone(),
                });
                self.wait_inspect_ok();
                stop_reply(&reason)
            }
            None => "E01".into(),
        }
    }

    fn wait_stopped(&mut self, stream: &mut TcpStream, timeout: Duration) -> Option<StopReason> {
        let _ = stream.set_read_timeout(Some(Duration::from_millis(20)));
        let deadline = std::time::Instant::now() + timeout;
        while std::time::Instant::now() < deadline {
            let mut byte = [0u8; 1];
            match stream.read(&mut byte) {
                Ok(0) => return None,
                Ok(_) if byte[0] == 0x03 => {
                    let _ = self.ctrl.stop();
                }
                _ => {}
            }
            match self.events.recv_timeout(Duration::from_millis(20)) {
                Ok(Response::Stopped(reason)) => {
                    let _ = stream.set_read_timeout(None);
                    return Some(reason);
                }
                Ok(Response::Quit) => return None,
                Ok(_) => {}
                Err(_) => {}
            }
        }
        let _ = stream.set_read_timeout(None);
        None
    }

    fn wait_inspect_ok(&mut self) {
        let deadline = std::time::Instant::now() + Duration::from_secs(1);
        while std::time::Instant::now() < deadline {
            match self.events.recv_timeout(Duration::from_millis(50)) {
                Ok(Response::Inspect(InspectResult::Ok)) | Ok(Response::Error(_)) => return,
                Ok(_) => {}
                Err(_) => return,
            }
        }
    }

    fn rpc(&mut self, cmd: Command) -> Option<Response> {
        self.ctrl.send(cmd).ok()?;
        let deadline = std::time::Instant::now() + Duration::from_secs(2);
        while std::time::Instant::now() < deadline {
            match self.events.recv_timeout(Duration::from_millis(50)) {
                Ok(Response::Started | Response::Stopped(_)) => {}
                Ok(other) => return Some(other),
                Err(_) => {}
            }
        }
        None
    }

    fn read_all_regs(&mut self) -> String {
        let mut out = String::new();
        for id in 0..GDB_REG_COUNT {
            match self.rpc(Command::ReadReg { id: RegId(id) }) {
                Some(Response::Inspect(InspectResult::Reg { value, .. })) => {
                    out.push_str(&hex_le_u32(value as u32));
                }
                _ => out.push_str("xxxxxxxx"),
            }
        }
        out
    }

    fn write_all_regs(&mut self, hex: &str) -> String {
        let bytes = match decode_hex(hex) {
            Some(b) => b,
            None => return "E01".into(),
        };
        for id in 0..GDB_REG_COUNT {
            let off = id as usize * GDB_REG_BYTES;
            if off + GDB_REG_BYTES > bytes.len() {
                break;
            }
            let value = u32::from_le_bytes(bytes[off..off + 4].try_into().unwrap()) as u64;
            if self
                .rpc(Command::WriteReg {
                    id: RegId(id),
                    value,
                })
                .is_none()
            {
                return "E01".into();
            }
        }
        "OK".into()
    }

    fn read_mem(&mut self, spec: &str) -> String {
        let Some((addr, len)) = parse_addr_len(spec) else {
            return "E01".into();
        };
        match self.rpc(Command::ReadMem { addr, len }) {
            Some(Response::Inspect(InspectResult::Mem { data, .. })) => encode_hex(&data),
            _ => "E01".into(),
        }
    }

    fn write_mem(&mut self, spec: &str) -> String {
        let Some((head, hex)) = spec.split_once(':') else {
            return "E01".into();
        };
        let Some((addr, _len)) = parse_addr_len(head) else {
            return "E01".into();
        };
        let Some(data) = decode_hex(hex) else {
            return "E01".into();
        };
        match self.rpc(Command::WriteMem { addr, data }) {
            Some(Response::Inspect(InspectResult::Ok)) => "OK".into(),
            _ => "E01".into(),
        }
    }

    fn add_sw_break(&mut self, spec: &str) -> String {
        let Some((addr, _)) = parse_addr_kind(spec) else {
            return "E01".into();
        };
        match self.rpc(Command::AddBreakpoint { addr }) {
            Some(Response::Inspect(InspectResult::Breakpoint { id })) => {
                self.breakpoints.insert(addr, id);
                "OK".into()
            }
            _ => "E01".into(),
        }
    }

    fn remove_sw_break(&mut self, spec: &str) -> String {
        let Some((addr, _)) = parse_addr_kind(spec) else {
            return "E01".into();
        };
        let Some(id) = self.breakpoints.remove(&addr) else {
            return "OK".into();
        };
        match self.rpc(Command::RemoveBreakpoint { id }) {
            Some(Response::Inspect(InspectResult::Ok)) => "OK".into(),
            _ => "E01".into(),
        }
    }
}

fn stop_reply(reason: &StopReason) -> String {
    match reason {
        StopReason::Breakpoint { .. } => "S05".into(),
        StopReason::Step => "S05".into(),
        StopReason::ExternalStop => "S02".into(),
        StopReason::Halt => "S05".into(),
        StopReason::Unmapped { .. } => "S0b".into(),
    }
}

fn xfer_xml(offset_length: &str) -> String {
    let Some((off, len)) = offset_length.split_once(',') else {
        return "E01".into();
    };
    let Ok(off) = usize::from_str_radix(off, 16) else {
        return "E01".into();
    };
    let Ok(len) = usize::from_str_radix(len, 16) else {
        return "E01".into();
    };
    let xml = TARGET_XML.as_bytes();
    if off >= xml.len() {
        return "l".into();
    }
    let end = (off + len).min(xml.len());
    let chunk = &xml[off..end];
    let prefix = if end == xml.len() { 'l' } else { 'm' };
    format!("{prefix}{}", String::from_utf8_lossy(chunk))
}

fn parse_addr_len(spec: &str) -> Option<(u64, u32)> {
    let (addr, len) = spec.split_once(',')?;
    Some((
        u64::from_str_radix(addr, 16).ok()?,
        u32::from_str_radix(len, 16).ok()?,
    ))
}

fn parse_addr_kind(spec: &str) -> Option<(u64, u32)> {
    parse_addr_len(spec)
}

fn hex_le_u32(value: u32) -> String {
    encode_hex(&value.to_le_bytes())
}

fn encode_hex(data: &[u8]) -> String {
    const HEX: &[u8] = b"0123456789abcdef";
    let mut out = String::with_capacity(data.len() * 2);
    for b in data {
        out.push(HEX[(b >> 4) as usize] as char);
        out.push(HEX[(b & 0xf) as usize] as char);
    }
    out
}

fn decode_hex(s: &str) -> Option<Vec<u8>> {
    if s.len() % 2 != 0 {
        return None;
    }
    let mut out = Vec::with_capacity(s.len() / 2);
    let bytes = s.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        let hi = from_hex(bytes[i])?;
        let lo = from_hex(bytes[i + 1])?;
        out.push((hi << 4) | lo);
        i += 2;
    }
    Some(out)
}

fn from_hex(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}

enum Incoming {
    Interrupt,
    Command(String),
}

fn is_resume_packet(body: &str) -> bool {
    matches!(body.as_bytes().first(), Some(b'c' | b's'))
        || (body.starts_with("vCont") && body != "vCont?")
}

fn resume_is_step(body: &str) -> bool {
    matches!(body.as_bytes().first(), Some(b's'))
        || body
            .strip_prefix("vCont")
            .map(|rest| {
                rest.trim_start_matches(';')
                    .split(';')
                    .any(|a| a.starts_with('s'))
            })
            .unwrap_or(false)
}

fn read_packet(stream: &mut TcpStream) -> io::Result<Option<Incoming>> {
    let mut buf = [0u8; 1];
    loop {
        if stream.read(&mut buf)? == 0 {
            return Ok(None);
        }
        match buf[0] {
            b'+' | b'-' => continue,
            0x03 => return Ok(Some(Incoming::Interrupt)),
            b'$' => break,
            _ => continue,
        }
    }
    let mut payload = Vec::new();
    loop {
        if stream.read(&mut buf)? == 0 {
            return Ok(None);
        }
        if buf[0] == b'#' {
            break;
        }
        payload.push(buf[0]);
        if payload.len() > PACKET_MAX {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "gdb packet too large",
            ));
        }
    }
    let mut csum = [0u8; 2];
    stream.read_exact(&mut csum)?;
    let _ = u8::from_str_radix(std::str::from_utf8(&csum).unwrap_or("00"), 16);
    stream.write_all(b"+")?;
    let body = unescape_gdb(&payload);
    Ok(Some(Incoming::Command(
        String::from_utf8_lossy(&body).into_owned(),
    )))
}

fn unescape_gdb(payload: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(payload.len());
    let mut i = 0;
    while i < payload.len() {
        if payload[i] == b'}' && i + 1 < payload.len() {
            out.push(payload[i + 1] ^ 0x20);
            i += 2;
        } else {
            out.push(payload[i]);
            i += 1;
        }
    }
    out
}

fn write_packet(stream: &mut TcpStream, payload: &str) -> io::Result<()> {
    let mut csum: u8 = 0;
    for b in payload.as_bytes() {
        csum = csum.wrapping_add(*b);
    }
    write!(stream, "${payload}#{csum:02x}")?;
    stream.flush()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bus::MemoryBus;
    use crate::event::EventCtl;
    use crate::machine::Machine;
    use crate::sim::{SimConfig, spawn};
    use crate::testing::ScriptedCpu;
    use std::net::Shutdown;

    fn checksum_ok(packet: &str) -> bool {
        let inner = packet.strip_prefix('$').unwrap();
        let (payload, csum) = inner.split_once('#').unwrap();
        let mut s: u8 = 0;
        for b in payload.as_bytes() {
            s = s.wrapping_add(*b);
        }
        format!("{s:02x}") == csum
    }

    fn read_reply(stream: &mut TcpStream) -> String {
        stream
            .set_read_timeout(Some(Duration::from_secs(2)))
            .unwrap();
        let mut b = [0u8; 1];
        loop {
            stream.read_exact(&mut b).unwrap();
            if b[0] == b'$' {
                break;
            }
        }
        let mut buf = Vec::new();
        loop {
            stream.read_exact(&mut b).unwrap();
            if b[0] == b'#' {
                let mut c = [0u8; 2];
                stream.read_exact(&mut c).unwrap();
                return String::from_utf8(buf).unwrap();
            }
            buf.push(b[0]);
        }
    }

    fn send_cmd(stream: &mut TcpStream, body: &str) -> String {
        let mut csum: u8 = 0;
        for b in body.as_bytes() {
            csum = csum.wrapping_add(*b);
        }
        write!(stream, "${body}#{csum:02x}").unwrap();
        stream.flush().unwrap();
        read_reply(stream)
    }

    #[test]
    fn parse_tcp_port_only() {
        let addr = parse_gdb_dev("tcp::2159").unwrap();
        assert_eq!(addr.port(), 2159);
    }

    #[test]
    fn qsupported_and_g_over_loopback() {
        let (ctrl, _events) = spawn(
            Machine::new(ScriptedCpu::nops(8), MemoryBus::new(), EventCtl::new()),
            SimConfig::default(),
        );
        let (addr, _h) = listen_gdb(ctrl.clone(), "127.0.0.1:0".parse().unwrap()).unwrap();
        let mut stream = TcpStream::connect_timeout(&addr, Duration::from_secs(2)).unwrap();
        let reply = send_cmd(&mut stream, "qSupported:multiprocess+");
        assert!(reply.contains("vContSupported+"), "{reply}");
        assert!(checksum_ok(&format!(
            "${reply}#{:02x}",
            reply.bytes().fold(0u8, |a, b| a.wrapping_add(b))
        )));
        let regs = send_cmd(&mut stream, "g");
        assert_eq!(regs.len(), GDB_REG_COUNT as usize * 8, "{regs}");
        let _ = send_cmd(&mut stream, "k");
        let _ = stream.shutdown(Shutdown::Both);
        let _ = ctrl.quit();
    }
}
