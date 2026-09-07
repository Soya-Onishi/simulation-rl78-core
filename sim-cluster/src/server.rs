//! Singleton cluster server: run Python DSL, validate topology, spawn arbiter.

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{SystemTime, UNIX_EPOCH};

use crate::topology::{LogicalTopology, TopologyError};

/// Options for [`run_server`].
#[derive(Clone, Debug)]
pub struct ServerOptions {
    /// Python topology script path.
    pub script: PathBuf,
    /// Directory containing `topology_dsl` (added to `PYTHONPATH`).
    pub python_path: PathBuf,
    /// Path to the `cluster-arbiter` executable.
    pub arbiter_bin: PathBuf,
    /// Python interpreter (default `python3`).
    pub python_bin: PathBuf,
    /// Lock file used to enforce a single server instance.
    pub lock_path: PathBuf,
}

impl ServerOptions {
    /// Build options with sibling-binary defaults relative to `current_exe`.
    pub fn from_script(script: PathBuf) -> Result<Self, ServerError> {
        let exe = std::env::current_exe().map_err(ServerError::Io)?;
        let exe_dir = exe
            .parent()
            .ok_or_else(|| ServerError::Message("current_exe has no parent".into()))?
            .to_path_buf();
        let arbiter_bin = exe_dir.join("cluster-arbiter");
        let python_path = discover_python_package_root()?;
        let lock_path = default_lock_path();
        Ok(Self {
            script,
            python_path,
            arbiter_bin,
            python_bin: PathBuf::from("python3"),
            lock_path,
        })
    }
}

/// Server failures.
#[derive(Debug, thiserror::Error)]
pub enum ServerError {
    #[error("another cluster-server instance holds the lock at {0}")]
    AlreadyRunning(PathBuf),
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),
    #[error("topology error: {0}")]
    Topology(#[from] TopologyError),
    #[error("python exited with status {status}: {stderr}")]
    PythonFailed { status: String, stderr: String },
    #[error("{0}")]
    Message(String),
}

/// Acquire the singleton lock, run Python, validate JSON, spawn the arbiter.
///
/// Returns when the arbiter process exits.
pub fn run_server(opts: &ServerOptions) -> Result<i32, ServerError> {
    let _lock = ServerLock::acquire(&opts.lock_path)?;

    if !opts.script.is_file() {
        return Err(ServerError::Message(format!(
            "topology script not found: {}",
            opts.script.display()
        )));
    }
    if !opts.arbiter_bin.is_file() {
        return Err(ServerError::Message(format!(
            "cluster-arbiter not found at {} (build the arbiter binary first)",
            opts.arbiter_bin.display()
        )));
    }

    let json = run_python_topology(opts)?;
    let topo = LogicalTopology::from_json_str(&json)?;
    let json_path = write_temp_topology(&topo)?;

    let mut child = Command::new(&opts.arbiter_bin)
        .arg("--topology")
        .arg(&json_path)
        .stdin(Stdio::null())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .spawn()
        .map_err(ServerError::Io)?;

    let status = child.wait().map_err(ServerError::Io)?;
    let _ = fs::remove_file(&json_path);
    Ok(status.code().unwrap_or(1))
}

fn run_python_topology(opts: &ServerOptions) -> Result<String, ServerError> {
    let output = Command::new(&opts.python_bin)
        .env(
            "PYTHONPATH",
            prepend_pythonpath(&opts.python_path, std::env::var_os("PYTHONPATH")),
        )
        .arg("-m")
        .arg("topology_dsl")
        .arg("run")
        .arg(&opts.script)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .map_err(ServerError::Io)?;

    if !output.status.success() {
        return Err(ServerError::PythonFailed {
            status: output.status.to_string(),
            stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
        });
    }
    let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
    let trimmed = stdout.trim();
    if trimmed.is_empty() {
        return Err(ServerError::Message(
            "python topology script produced empty stdout".into(),
        ));
    }
    Ok(trimmed.to_string())
}

fn prepend_pythonpath(extra: &Path, existing: Option<std::ffi::OsString>) -> std::ffi::OsString {
    let mut out = extra.as_os_str().to_os_string();
    if let Some(prev) = existing {
        out.push(":");
        out.push(prev);
    }
    out
}

fn write_temp_topology(topo: &LogicalTopology) -> Result<PathBuf, ServerError> {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let path = std::env::temp_dir().join(format!("sim-cluster-topology-{nanos}.json"));
    let mut file = fs::File::create(&path)?;
    file.write_all(topo.to_json_string()?.as_bytes())?;
    Ok(path)
}

fn default_lock_path() -> PathBuf {
    std::env::var_os("XDG_RUNTIME_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(std::env::temp_dir)
        .join("sim-cluster-server.lock")
}

fn discover_python_package_root() -> Result<PathBuf, ServerError> {
    let mut candidates = Vec::new();
    if let Ok(manifest_dir) = std::env::var("CARGO_MANIFEST_DIR") {
        candidates.push(PathBuf::from(manifest_dir).join("../python"));
    }
    if let Ok(exe) = std::env::current_exe() {
        let mut dir = exe.parent().map(Path::to_path_buf);
        for _ in 0..6 {
            if let Some(d) = dir {
                candidates.push(d.join("python"));
                dir = d.parent().map(Path::to_path_buf);
            }
        }
    }
    for c in candidates {
        if c.join("topology_dsl").is_dir() {
            return Ok(c.canonicalize().unwrap_or(c));
        }
    }
    Err(ServerError::Message(
        "could not locate python/topology_dsl; set ServerOptions.python_path".into(),
    ))
}

/// Exclusive singleton lock via `flock(LOCK_EX)`.
///
/// The lock is released when the process exits (including crash / SIGKILL),
/// so a leftover lock file cannot permanently block a new `cluster-server`.
#[derive(Debug)]
struct ServerLock {
    path: PathBuf,
    file: fs::File,
}

impl ServerLock {
    fn acquire(path: &Path) -> Result<Self, ServerError> {
        use std::os::unix::io::AsRawFd;

        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        let mut file = fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(path)
            .map_err(ServerError::Io)?;

        let rc = unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
        if rc != 0 {
            let err = std::io::Error::last_os_error();
            if err.kind() == std::io::ErrorKind::WouldBlock
                || err.raw_os_error() == Some(libc::EWOULDBLOCK)
                || err.raw_os_error() == Some(libc::EAGAIN)
            {
                return Err(ServerError::AlreadyRunning(path.to_path_buf()));
            }
            return Err(ServerError::Io(err));
        }

        file.set_len(0).map_err(ServerError::Io)?;
        writeln!(file, "{}", std::process::id()).map_err(ServerError::Io)?;
        Ok(Self {
            path: path.to_path_buf(),
            file,
        })
    }
}

impl Drop for ServerLock {
    fn drop(&mut self) {
        use std::os::unix::io::AsRawFd;
        let _ = unsafe { libc::flock(self.file.as_raw_fd(), libc::LOCK_UN) };
        let _ = fs::remove_file(&self.path);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    #[test]
    fn python_round_trip_via_module() {
        let python_root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../python");
        let python_root = python_root.canonicalize().expect("python dir");
        let mut script = tempfile::NamedTempFile::new().unwrap();
        write!(
            script,
            r#"
from topology_dsl import Topology, connect, emit

t = Topology(margin_ns=1000, headroom_threshold_ns=200)
a = t.board("a", kind="rl78", elf="a.elf")
b = t.board("b", kind="rl78")
a.endpoint("uart0_tx", direction="out", payload="uart")
a.endpoint("uart0_rx", direction="in", payload="uart")
b.endpoint("uart0_tx", direction="out", payload="uart")
b.endpoint("uart0_rx", direction="in", payload="uart")
connect(a.port("uart0_tx"), b.port("uart0_rx"))
connect(b.port("uart0_tx"), a.port("uart0_rx"), uart_ring_len=128)
emit(t)
"#
        )
        .unwrap();

        let opts = ServerOptions {
            script: script.path().to_path_buf(),
            python_path: python_root,
            arbiter_bin: PathBuf::from("/nonexistent"),
            python_bin: PathBuf::from("python3"),
            lock_path: PathBuf::from("/tmp/unused"),
        };
        let json = run_python_topology(&opts).expect("python run");
        let topo = LogicalTopology::from_json_str(&json).expect("parse");
        assert_eq!(topo.boards.len(), 2);
        assert_eq!(topo.edges.len(), 2);
        assert_eq!(topo.margin_ns, 1000);
    }

    #[test]
    fn server_spawns_arbiter_with_example_script() {
        let manifest = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
        let python_root = manifest.join("../python").canonicalize().unwrap();
        let script = python_root.join("examples/two_board_uart.py");
        let arbiter = manifest
            .join("../target/debug/cluster-arbiter")
            .canonicalize()
            .unwrap_or_else(|_| {
                // Fall back to CARGO_BIN_EXE when available (integration-style).
                PathBuf::from(option_env!("CARGO_BIN_EXE_cluster-arbiter").unwrap_or(""))
            });
        if !arbiter.is_file() {
            // Unit tests may run before bins are linked; skip rather than fail CI noise.
            eprintln!("skip: cluster-arbiter not built at {}", arbiter.display());
            return;
        }

        let lock = tempfile::NamedTempFile::new().unwrap();
        let lock_path = lock.path().to_path_buf();
        drop(lock);

        let opts = ServerOptions {
            script,
            python_path: python_root,
            arbiter_bin: arbiter,
            python_bin: PathBuf::from("python3"),
            lock_path,
        };
        let code = run_server(&opts).expect("run_server");
        assert_eq!(code, 0);
    }

    #[test]
    fn stale_lock_file_is_reclaimed() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("server.lock");
        fs::write(&path, "1\n").unwrap();
        let lock = ServerLock::acquire(&path).expect("reclaim stale lock file");
        let err = ServerLock::acquire(&path).unwrap_err();
        assert!(matches!(err, ServerError::AlreadyRunning(_)));
        drop(lock);
        let _relock = ServerLock::acquire(&path).expect("lock after drop");
    }
}
