//! Length-prefixed JSON inspect RPC over a Unix domain socket (server ↔ node).

use std::collections::HashMap;
use std::io::{Read, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::{Deserialize, Serialize};
use sim_kernel::{BreakpointId, Command, Entity, InspectResult, Response, Simulator};

use crate::ipc::board_id_hash;

/// Maximum memory payload per inspect call (gdbstub chunks larger reads).
pub const MEM_CHUNK: u32 = 1024;

/// Socket path under the cluster iceoryx root.
#[must_use]
pub fn inspect_sock_path(iox_root: &Path, board_id: &str) -> PathBuf {
    iox_root.join(format!("inspect-{:016x}.sock", board_id_hash(board_id)))
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum InspectRequest {
    ReadRegs,
    WriteRegs { data: String },
    ReadMem { addr: u64, len: u32 },
    WriteMem { addr: u64, data: String },
    AddBreakpoint { addr: u64 },
    RemoveBreakpoint { addr: u64 },
    ClearBreakpoints,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum InspectReply {
    Ok,
    Regs { data: String, pc: u64 },
    Mem { data: String },
    Breakpoint { id: u64 },
    Error { message: String },
}

/// Node-side listener. One connection handles one request.
pub struct InspectServer {
    listener: UnixListener,
    path: PathBuf,
    breakpoints: HashMap<u64, BreakpointId>,
}

impl InspectServer {
    pub fn bind(iox_root: &Path, board_id: &str) -> std::io::Result<Self> {
        let path = inspect_sock_path(iox_root, board_id);
        let _ = std::fs::remove_file(&path);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let listener = UnixListener::bind(&path)?;
        listener.set_nonblocking(true)?;
        Ok(Self {
            listener,
            path,
            breakpoints: HashMap::new(),
        })
    }

    pub fn service<E: Entity>(&mut self, sim: &mut Simulator<E>) -> std::io::Result<()> {
        match self.listener.accept() {
            Ok((mut stream, _)) => {
                stream.set_nonblocking(false)?;
                let _ = stream.set_read_timeout(Some(Duration::from_secs(2)));
                let _ = stream.set_write_timeout(Some(Duration::from_secs(2)));
                match read_msg::<InspectRequest>(&mut stream) {
                    Ok(req) => {
                        let reply = self.handle(sim, req);
                        let _ = write_msg(&mut stream, &reply);
                    }
                    Err(err) => {
                        let _ = write_msg(
                            &mut stream,
                            &InspectReply::Error {
                                message: err.to_string(),
                            },
                        );
                    }
                }
                Ok(())
            }
            Err(err) if err.kind() == std::io::ErrorKind::WouldBlock => Ok(()),
            Err(err) => Err(err),
        }
    }

    fn handle<E: Entity>(&mut self, sim: &mut Simulator<E>, req: InspectRequest) -> InspectReply {
        match req {
            InspectRequest::ReadRegs => match sim.command(Command::ReadGdbRegs) {
                Response::Inspect(InspectResult::GdbRegs { data, pc }) => InspectReply::Regs {
                    data: encode_hex(&data),
                    pc,
                },
                other => reply_from_response(other),
            },
            InspectRequest::WriteRegs { data } => match decode_hex(&data) {
                Ok(bytes) => {
                    reply_from_response(sim.command(Command::WriteGdbRegs { data: bytes }))
                }
                Err(message) => InspectReply::Error { message },
            },
            InspectRequest::ReadMem { addr, len } => {
                if len == 0 || len > MEM_CHUNK {
                    return InspectReply::Error {
                        message: format!("len must be 1..={MEM_CHUNK}"),
                    };
                }
                match sim.command(Command::ReadMem { addr, len }) {
                    Response::Inspect(InspectResult::Mem { data, .. }) => InspectReply::Mem {
                        data: encode_hex(&data),
                    },
                    other => reply_from_response(other),
                }
            }
            InspectRequest::WriteMem { addr, data } => match decode_hex(&data) {
                Ok(bytes) if bytes.len() <= MEM_CHUNK as usize => {
                    reply_from_response(sim.command(Command::WriteMem { addr, data: bytes }))
                }
                Ok(_) => InspectReply::Error {
                    message: format!("write larger than {MEM_CHUNK}"),
                },
                Err(message) => InspectReply::Error { message },
            },
            InspectRequest::AddBreakpoint { addr } => {
                match sim.command(Command::AddBreakpoint { addr }) {
                    Response::Inspect(InspectResult::Breakpoint { id }) => {
                        self.breakpoints.insert(addr, id);
                        InspectReply::Breakpoint { id: id.0 }
                    }
                    other => reply_from_response(other),
                }
            }
            InspectRequest::RemoveBreakpoint { addr } => {
                let Some(id) = self.breakpoints.remove(&addr) else {
                    return InspectReply::Error {
                        message: "unknown breakpoint".into(),
                    };
                };
                reply_from_response(sim.command(Command::RemoveBreakpoint { id }))
            }
            InspectRequest::ClearBreakpoints => {
                self.breakpoints.clear();
                reply_from_response(sim.command(Command::ClearBreakpoints))
            }
        }
    }
}

impl Drop for InspectServer {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

/// Client RPC (one connection per call).
pub fn inspect_rpc(path: &Path, req: &InspectRequest) -> Result<InspectReply, String> {
    let mut stream = UnixStream::connect(path).map_err(|err| err.to_string())?;
    let _ = stream.set_read_timeout(Some(Duration::from_secs(2)));
    let _ = stream.set_write_timeout(Some(Duration::from_secs(2)));
    write_msg(&mut stream, req).map_err(|err| err.to_string())?;
    read_msg(&mut stream).map_err(|err| err.to_string())
}

fn reply_from_response(resp: Response) -> InspectReply {
    match resp {
        Response::Inspect(InspectResult::Ok) => InspectReply::Ok,
        Response::Inspect(InspectResult::GdbRegs { data, pc }) => InspectReply::Regs {
            data: encode_hex(&data),
            pc,
        },
        Response::Inspect(InspectResult::Mem { data, .. }) => InspectReply::Mem {
            data: encode_hex(&data),
        },
        Response::Inspect(InspectResult::Breakpoint { id }) => {
            InspectReply::Breakpoint { id: id.0 }
        }
        Response::Error(err) => InspectReply::Error {
            message: err.to_string(),
        },
        other => InspectReply::Error {
            message: format!("unexpected {other}"),
        },
    }
}

fn decode_hex(text: &str) -> Result<Vec<u8>, String> {
    if text.len() % 2 != 0 {
        return Err("odd hex length".into());
    }
    (0..text.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&text[i..i + 2], 16).map_err(|err| err.to_string()))
        .collect()
}

fn encode_hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn write_msg<T: Serialize>(stream: &mut UnixStream, value: &T) -> std::io::Result<()> {
    let bytes = serde_json::to_vec(value)
        .map_err(|err| std::io::Error::new(std::io::ErrorKind::InvalidData, err))?;
    stream.write_all(&(bytes.len() as u32).to_le_bytes())?;
    stream.write_all(&bytes)?;
    stream.flush()
}

fn read_msg<T: for<'de> Deserialize<'de>>(stream: &mut UnixStream) -> std::io::Result<T> {
    let mut len_buf = [0u8; 4];
    stream.read_exact(&mut len_buf)?;
    let len = u32::from_le_bytes(len_buf) as usize;
    if len == 0 || len > 64 * 1024 {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "inspect message too large",
        ));
    }
    let mut buf = vec![0u8; len];
    stream.read_exact(&mut buf)?;
    serde_json::from_slice(&buf)
        .map_err(|err| std::io::Error::new(std::io::ErrorKind::InvalidData, err))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sock_path_uses_board_hash() {
        let path = inspect_sock_path(std::path::Path::new("/tmp/iox"), "mcu-a");
        assert!(
            path.file_name()
                .unwrap()
                .to_str()
                .unwrap()
                .starts_with("inspect-")
        );
        assert!(path.extension().unwrap() == "sock");
    }
}
