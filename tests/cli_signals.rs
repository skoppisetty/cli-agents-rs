//! The binary must reap provider groups when its stdin is piped.
#![cfg(all(unix, feature = "cli"))]
use std::io::Write;
use std::os::unix::fs::PermissionsExt;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

#[test]
fn parent_signals_cancel_provider_tree_with_piped_stdin() {
    for signal in ["-TERM", "-INT"] {
        let dir = tempfile::tempdir().unwrap();
        let script = dir.path().join("fake-claude");
        let ready = dir.path().join("ready");
        std::fs::write(&script, "#!/bin/sh\nsleep 60 &\necho \"$$ $!\" > \"$FAKE_PID_FILE\"\nwait\n").unwrap();
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o700)).unwrap();
        let mut child = Command::new(env!("CARGO_BIN_EXE_cli-agents"))
            .args(["--json", "--config-stdin"])
            .stdin(Stdio::piped()).stdout(Stdio::null()).stderr(Stdio::null()).spawn().unwrap();
        let config = serde_json::json!({"cli":"claude","task":"wait", "executablePath":script,
            "env":{"FAKE_PID_FILE":ready},"cwd":dir.path()});
        child.stdin.take().unwrap().write_all(config.to_string().as_bytes()).unwrap();
        let deadline = Instant::now() + Duration::from_secs(5);
        while !ready.exists() {
            assert!(Instant::now() < deadline, "provider did not start");
            std::thread::sleep(Duration::from_millis(10));
        }
        let pids = std::fs::read_to_string(&ready).unwrap();
        assert!(Command::new("kill").args([signal, &child.id().to_string()]).status().unwrap().success());
        loop {
            if child.try_wait().unwrap().is_some() { break; }
            assert!(Instant::now() < deadline, "wrapper did not reap its provider");
            std::thread::sleep(Duration::from_millis(10));
        }
        for pid in pids.split_whitespace() {
            // A killed descendant can briefly be an init-owned zombie; neither
            // a missing process nor a zombie can perform more tool effects.
            let output = Command::new("ps").args(["-o", "stat=", "-p", pid]).output().unwrap();
            let state = String::from_utf8_lossy(&output.stdout);
            assert!(state.trim().is_empty() || state.trim().starts_with('Z'), "provider {pid} still alive: {state}");
        }
    }
}
