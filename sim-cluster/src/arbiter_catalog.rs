//! Collect board probe/action catalogs via `--list-catalog` before runtime spawn.

use std::collections::HashSet;
use std::io::Read;
use std::process::{Child, Command, ExitStatus, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use sim_kernel::{ActionCatalogEntry, ParamSpec};

use crate::action::{ActionAdvertise, ActionUpstream};
use crate::arbiter::{ArbiterError, ArbiterOptions};
use crate::board_runtime::{CatalogAction, CatalogDump};
use crate::ipc::{ArbiterServerActions, HashedId, board_id_hash, probe_id_hash, probe_name_hash};
use crate::probe::{MAX_PROBES_PER_BOARD, PROBE_NAME_CAP, ProbeAdvertisement, ProbeWireKind};
use crate::probe_hub::ArbiterProbeHub;
use crate::topology::LogicalTopology;

/// Wall-clock limit for each board `--list-catalog` child process.
pub const LIST_CATALOG_TIMEOUT: Duration = Duration::from_secs(10);

/// Max bytes kept from `--list-catalog` stdout/stderr (pipe is drained past this).
const CATALOG_OUTPUT_CAP: usize = 1024 * 1024;
/// Max characters embedded into error messages.
const ERROR_SNIPPET: usize = 4 * 1024;

/// One board's catalog captured via `--list-catalog` before runtime spawn.
#[derive(Clone, Debug)]
pub struct CollectedBoardCatalog {
    pub board_id: String,
    pub board_hash: HashedId<u64>,
    pub dump: CatalogDump,
}

pub fn collect_board_catalogs(
    opts: &ArbiterOptions,
    topo: &LogicalTopology,
) -> Result<Vec<CollectedBoardCatalog>, ArbiterError> {
    collect_board_catalogs_with_timeout(opts, topo, LIST_CATALOG_TIMEOUT)
}

pub fn collect_board_catalogs_with_timeout(
    opts: &ArbiterOptions,
    topo: &LogicalTopology,
    timeout: Duration,
) -> Result<Vec<CollectedBoardCatalog>, ArbiterError> {
    let mut collected = Vec::with_capacity(topo.boards.len());
    for board in &topo.boards {
        let bin = crate::arbiter::binary_for_kind(opts, &board.kind)?;
        let mut child = Command::new(bin)
            .arg("--list-catalog")
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|err| {
                ArbiterError::Message(format!(
                    "board '{}': failed to spawn `--list-catalog` ({}): {err}",
                    board.id,
                    bin.display()
                ))
            })?;

        let stdout_pipe = child.stdout.take();
        let stderr_pipe = child.stderr.take();
        let stdout_thr = thread::spawn(move || match stdout_pipe {
            Some(pipe) => read_capped(pipe, CATALOG_OUTPUT_CAP),
            None => CappedRead {
                bytes: Vec::new(),
                truncated: false,
            },
        });
        let stderr_thr = thread::spawn(move || match stderr_pipe {
            Some(pipe) => read_capped(pipe, CATALOG_OUTPUT_CAP),
            None => CappedRead {
                bytes: Vec::new(),
                truncated: false,
            },
        });

        let wait = wait_child_with_timeout(&mut child, timeout);
        match wait {
            WaitOutcome::TimedOut => {
                let _ = child.kill();
                let _ = child.wait();
                let _ = stdout_thr.join();
                let _ = stderr_thr.join();
                return Err(ArbiterError::Message(format!(
                    "board '{}': `--list-catalog` timed out after {}s",
                    board.id,
                    timeout.as_secs()
                )));
            }
            WaitOutcome::IoError(err) => {
                let _ = child.kill();
                let _ = child.wait();
                let _ = stdout_thr.join();
                let _ = stderr_thr.join();
                return Err(ArbiterError::Message(format!(
                    "board '{}': `--list-catalog` wait failed: {err}",
                    board.id
                )));
            }
            WaitOutcome::Exited(status) => {
                let stdout_read = stdout_thr.join().unwrap_or_default();
                let stderr_read = stderr_thr.join().unwrap_or_default();
                let stdout = String::from_utf8_lossy(&stdout_read.bytes);
                let stderr = String::from_utf8_lossy(&stderr_read.bytes);
                let exit_label = exit_status_label(status);
                if !status.success() {
                    return Err(ArbiterError::Message(format!(
                        "board '{}': `--list-catalog` exited with {exit_label}; stderr:\n{}",
                        board.id,
                        error_snippet(&stderr)
                    )));
                }
                if stdout_read.truncated {
                    return Err(ArbiterError::Message(format!(
                        "board '{}': `--list-catalog` stdout exceeds {CATALOG_OUTPUT_CAP} bytes",
                        board.id
                    )));
                }
                let dump = CatalogDump::parse_json(&stdout).map_err(|err| {
                    ArbiterError::Message(format!(
                        "board '{}': `--list-catalog` exited with {exit_label}; stderr:\n{}\nstdout is not CatalogDump JSON ({err}); stdout:\n{}",
                        board.id,
                        error_snippet(&stderr),
                        error_snippet(&stdout)
                    ))
                })?;
                collected.push(CollectedBoardCatalog {
                    board_id: board.id.clone(),
                    board_hash: board_id_hash(&board.id),
                    dump,
                });
            }
        }
    }
    Ok(collected)
}

enum WaitOutcome {
    Exited(ExitStatus),
    TimedOut,
    IoError(std::io::Error),
}

fn wait_child_with_timeout(child: &mut Child, timeout: Duration) -> WaitOutcome {
    let deadline = Instant::now() + timeout;
    loop {
        match child.try_wait() {
            Ok(Some(status)) => return WaitOutcome::Exited(status),
            Ok(None) => {
                if Instant::now() >= deadline {
                    return WaitOutcome::TimedOut;
                }
                thread::sleep(Duration::from_millis(10));
            }
            Err(err) => return WaitOutcome::IoError(err),
        }
    }
}

#[derive(Default)]
struct CappedRead {
    bytes: Vec<u8>,
    truncated: bool,
}

fn read_capped(mut reader: impl Read, cap: usize) -> CappedRead {
    let mut out = CappedRead::default();
    let mut chunk = [0u8; 4096];
    loop {
        match reader.read(&mut chunk) {
            Ok(0) => break,
            Ok(n) => {
                if out.bytes.len() < cap {
                    let take = n.min(cap - out.bytes.len());
                    out.bytes.extend_from_slice(&chunk[..take]);
                    if take < n {
                        out.truncated = true;
                    }
                } else {
                    out.truncated = true;
                }
            }
            Err(_) => break,
        }
    }
    out
}

fn error_snippet(text: &str) -> String {
    if text.chars().count() <= ERROR_SNIPPET {
        return text.to_string();
    }
    let truncated: String = text.chars().take(ERROR_SNIPPET).collect();
    format!("{truncated}…(truncated)")
}

fn exit_status_label(status: ExitStatus) -> String {
    status
        .code()
        .map(|c| c.to_string())
        .unwrap_or_else(|| status.to_string())
}

pub fn apply_probe_catalogs(
    hub: &mut ArbiterProbeHub,
    catalogs: &[CollectedBoardCatalog],
) -> Result<(), ArbiterError> {
    for board in catalogs {
        if board.dump.probes.len() > MAX_PROBES_PER_BOARD {
            return Err(ArbiterError::Message(format!(
                "board '{}': `--list-catalog` has {} probes (max {MAX_PROBES_PER_BOARD})",
                board.board_id,
                board.dump.probes.len()
            )));
        }
        let mut seen = HashSet::new();
        for probe in &board.dump.probes {
            if probe.probe_label.len() > PROBE_NAME_CAP {
                return Err(ArbiterError::Message(format!(
                    "board '{}': probe `{}` name exceeds {PROBE_NAME_CAP} UTF-8 bytes",
                    board.board_id, probe.probe_label
                )));
            }
            let expected = probe_name_hash(&probe.probe_label);
            if probe.probe != expected {
                return Err(ArbiterError::Message(format!(
                    "board '{}': probe `{}` hash mismatch (got {}, expected {expected})",
                    board.board_id, probe.probe_label, probe.probe
                )));
            }
            if !seen.insert(probe.probe_label.clone()) {
                return Err(ArbiterError::Message(format!(
                    "board '{}': duplicate probe label `{}`",
                    board.board_id, probe.probe_label
                )));
            }
            let Some(kind) = ProbeWireKind::from_catalog_kind(&probe.kind) else {
                return Err(ArbiterError::Message(format!(
                    "board '{}': unknown probe kind `{}` for `{}`",
                    board.board_id, probe.kind, probe.probe_label
                )));
            };
            hub.record_advertise(
                board.board_id.clone(),
                board.board_hash,
                ProbeAdvertisement {
                    name: probe.probe_label.clone(),
                    name_hash: probe.probe,
                    probe_id: probe_id_hash(board.board_hash, &probe.probe_label),
                    kind,
                },
            );
        }
    }
    Ok(())
}

pub fn publish_action_catalogs(
    server: &ArbiterServerActions,
    catalogs: &[CollectedBoardCatalog],
) -> Result<(), ArbiterError> {
    for board in catalogs {
        for action in &board.dump.actions {
            let sample = action_advertise_from_catalog(&board.board_id, board.board_hash, action)?;
            server.publish_upstream(&ActionUpstream::Advertise(sample))?;
        }
    }
    server.publish_upstream(&ActionUpstream::CatalogEnd)?;
    Ok(())
}

fn action_advertise_from_catalog(
    board_id: &str,
    board_hash: HashedId<u64>,
    action: &CatalogAction,
) -> Result<ActionAdvertise, ArbiterError> {
    let entry = ActionCatalogEntry {
        device: action.device_label.clone(),
        device_hash: action.device.get(),
        action: action.action_label.clone(),
        action_hash: action.action.get(),
        description: action.description.clone(),
        params: action
            .params
            .iter()
            .map(|param| ParamSpec {
                name: param.name_label.clone(),
                name_hash: param.name.get(),
                unit: param.unit.clone(),
                description: param.description.clone(),
                min: param.min,
                max: param.max,
                default: param.default,
            })
            .collect(),
    };
    ActionAdvertise::new(board_hash, &entry).map_err(|err| {
        ArbiterError::Message(format!(
            "board '{board_id}': action '{}/{}': {err}",
            action.device_label, action.action_label
        ))
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::topology::BoardSpec;
    use std::fs;
    use std::path::{Path, PathBuf};

    /// Write a fake board binary without opening `path` for writing.
    ///
    /// Contents go to a sibling temp path first; that inode is then renamed onto
    /// `path`. Spawning `path` therefore never races with a write handle on the
    /// same path (avoids Linux `ETXTBSY`).
    fn write_exec(path: &Path, body: &str) {
        let staging = path.with_extension("writing");
        fs::write(&staging, body).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mut perms = fs::metadata(&staging).unwrap().permissions();
            perms.set_mode(0o755);
            fs::set_permissions(&staging, perms).unwrap();
        }
        fs::rename(&staging, path).unwrap();
    }

    fn one_board_topo(kind: &str) -> LogicalTopology {
        LogicalTopology {
            boards: vec![BoardSpec {
                id: "mcu-a".into(),
                kind: kind.into(),
                elf: None,
                endpoints: vec![],
            }],
            edges: vec![],
            margin_ns: 1000,
            headroom_threshold_ns: 100,
            ready_timeout_ms: 1000,
        }
    }

    fn opts_with_bin(bin: PathBuf) -> ArbiterOptions {
        ArbiterOptions {
            topology_path: PathBuf::from("t.json"),
            node_bin: bin.clone(),
            plant_bin: bin,
            cluster_key: "k".into(),
            iox_root: PathBuf::from("/tmp/iox"),
        }
    }

    #[test]
    fn apply_probe_catalogs_fills_hub_from_dump() {
        let board_hash = board_id_hash("mcu-a");
        let dump = CatalogDump {
            probes: vec![crate::board_runtime::CatalogProbe {
                probe: probe_name_hash("uart0"),
                probe_label: "uart0".into(),
                kind: "uart".into(),
            }],
            actions: vec![],
        };
        let catalogs = vec![CollectedBoardCatalog {
            board_id: "mcu-a".into(),
            board_hash,
            dump,
        }];
        let mut hub = ArbiterProbeHub::new("test");
        apply_probe_catalogs(&mut hub, &catalogs).unwrap();
        let entry = hub.catalog().values().next().unwrap();
        assert_eq!(entry.board_id, "mcu-a");
        assert_eq!(entry.board_hash, board_hash);
        assert_eq!(entry.name, "uart0");
        assert_eq!(entry.name_hash, probe_name_hash("uart0"));
        assert_eq!(entry.probe_id, probe_id_hash(board_hash, "uart0"));
        assert_eq!(entry.kind, ProbeWireKind::Uart);
    }

    #[test]
    fn apply_probe_catalogs_rejects_hash_mismatch() {
        let dump = CatalogDump {
            probes: vec![crate::board_runtime::CatalogProbe {
                probe: HashedId::new(0x0123_4567_89ab_cdef),
                probe_label: "uart0".into(),
                kind: "uart".into(),
            }],
            actions: vec![],
        };
        let catalogs = vec![CollectedBoardCatalog {
            board_id: "mcu-a".into(),
            board_hash: board_id_hash("mcu-a"),
            dump,
        }];
        let mut hub = ArbiterProbeHub::new("test");
        let err = apply_probe_catalogs(&mut hub, &catalogs).unwrap_err();
        assert!(err.to_string().contains("hash mismatch"), "{err}");
    }

    #[test]
    fn apply_probe_catalogs_rejects_unknown_kind() {
        let dump = CatalogDump {
            probes: vec![crate::board_runtime::CatalogProbe {
                probe: probe_name_hash("p0"),
                probe_label: "p0".into(),
                kind: "nope".into(),
            }],
            actions: vec![],
        };
        let catalogs = vec![CollectedBoardCatalog {
            board_id: "mcu-a".into(),
            board_hash: board_id_hash("mcu-a"),
            dump,
        }];
        let mut hub = ArbiterProbeHub::new("test");
        let err = apply_probe_catalogs(&mut hub, &catalogs).unwrap_err();
        assert!(err.to_string().contains("unknown probe kind"), "{err}");
    }

    #[test]
    fn collect_board_catalogs_parses_stdout_json() {
        let dir = tempfile::tempdir().unwrap();
        let bin = dir.path().join("fake-board");
        let probe = probe_name_hash("p0");
        let json = format!(
            r#"{{"probes":[{{"probe":"{probe}","probe_label":"p0","kind":"digital"}}],"actions":[]}}"#
        );
        write_exec(&bin, &format!("#!/bin/sh\nprintf '%s\\n' '{json}'\n"));
        let opts = opts_with_bin(bin);
        let topo = one_board_topo("rl78");
        let catalogs =
            collect_board_catalogs_with_timeout(&opts, &topo, Duration::from_secs(2)).unwrap();
        assert_eq!(catalogs.len(), 1);
        assert_eq!(catalogs[0].board_id, "mcu-a");
        assert_eq!(catalogs[0].dump.probes[0].probe_label, "p0");
    }

    #[test]
    fn collect_board_catalogs_drains_large_stdout() {
        let dir = tempfile::tempdir().unwrap();
        let bin = dir.path().join("fake-board");
        // Valid JSON with >64KiB of insignificant whitespace so the pipe must be drained
        // while the child is still writing.
        write_exec(
            &bin,
            "#!/bin/sh\nprintf '%s' '{\"probes\":[]'\ndd if=/dev/zero bs=70000 count=1 2>/dev/null | tr '\\0' ' '\nprintf '%s\\n' ',\"actions\":[]}'\n",
        );
        let opts = opts_with_bin(bin);
        let topo = one_board_topo("rl78");
        let catalogs =
            collect_board_catalogs_with_timeout(&opts, &topo, Duration::from_secs(5)).unwrap();
        assert!(catalogs[0].dump.probes.is_empty());
        assert!(catalogs[0].dump.actions.is_empty());
    }

    #[test]
    fn collect_board_catalogs_fails_on_nonzero_exit() {
        let dir = tempfile::tempdir().unwrap();
        let bin = dir.path().join("fake-board");
        write_exec(&bin, "#!/bin/sh\necho boom >&2\nexit 7\n");
        let opts = opts_with_bin(bin);
        let topo = one_board_topo("rl78");
        let err =
            collect_board_catalogs_with_timeout(&opts, &topo, Duration::from_secs(2)).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("mcu-a"), "{msg}");
        assert!(msg.contains("exited with 7"), "{msg}");
        assert!(msg.contains("boom"), "{msg}");
    }

    #[test]
    fn collect_board_catalogs_fails_on_invalid_json() {
        let dir = tempfile::tempdir().unwrap();
        let bin = dir.path().join("fake-board");
        write_exec(&bin, "#!/bin/sh\necho warn >&2\necho not-json\n");
        let opts = opts_with_bin(bin);
        let topo = one_board_topo("rl78");
        let err =
            collect_board_catalogs_with_timeout(&opts, &topo, Duration::from_secs(2)).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("mcu-a"), "{msg}");
        assert!(msg.contains("exited with 0"), "{msg}");
        assert!(msg.contains("warn"), "{msg}");
        assert!(msg.contains("not-json"), "{msg}");
    }

    #[test]
    fn collect_board_catalogs_times_out_without_output() {
        let dir = tempfile::tempdir().unwrap();
        let bin = dir.path().join("fake-board");
        write_exec(
            &bin,
            "#!/bin/sh\necho SECRET\necho SECRET >&2\nexec sleep 5\n",
        );
        let opts = opts_with_bin(bin);
        let topo = one_board_topo("rl78");
        let err = collect_board_catalogs_with_timeout(&opts, &topo, Duration::from_millis(50))
            .unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("mcu-a"), "{msg}");
        assert!(msg.contains("timed out"), "{msg}");
        assert!(!msg.contains("SECRET"), "{msg}");
    }
}
