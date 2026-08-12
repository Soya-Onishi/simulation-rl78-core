use std::io::Write;
use std::process::{Command, Stdio};

#[test]
fn cli_start_stop_quit() {
    let bin = env!("CARGO_BIN_EXE_simulation-rl78-core");
    let mut child = Command::new(bin)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn CLI");

    {
        let mut stdin = child.stdin.take().expect("stdin");
        writeln!(stdin, "start").unwrap();
        writeln!(stdin, "stop").unwrap();
        writeln!(stdin, "quit").unwrap();
    }

    let output = child.wait_with_output().expect("wait_with_output");
    assert!(
        output.status.success(),
        "stderr={}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("started"), "{stdout}");
    assert!(stdout.contains("stopped: external"), "{stdout}");
    assert!(stdout.contains("quit"), "{stdout}");
}
