//! The binary must reap provider groups when its stdin is piped.
#![cfg(all(unix, feature = "cli"))]
use std::io::{BufRead, BufReader, Read, Write};
use std::os::unix::fs::PermissionsExt;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

#[test]
fn parent_signals_cancel_provider_tree_with_piped_stdin() {
    for (signal, provider) in [("-TERM", "claude"), ("-INT", "claude"), ("-TERM", "codex"), ("-INT", "codex")] {
        let dir = tempfile::tempdir().unwrap();
        let script = dir.path().join("fake-claude");
        let ready = dir.path().join("ready");
        let event = if provider == "claude" {
            r#"{"type":"system","session_id":"observed-session"}"#
        } else { r#"{"type":"thread.started","thread_id":"observed-session"}"# };
        std::fs::write(&script, format!("#!/bin/sh\nprintf '%s\\n' '{event}'\nsleep 60 &\necho \"$$ $!\" > \"$FAKE_PID_FILE\"\nwait\n")).unwrap();
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o700)).unwrap();
        let mut child = Command::new(env!("CARGO_BIN_EXE_cli-agents"))
            .args(["--json", "--config-stdin"])
            .stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::null()).spawn().unwrap();
        let config = serde_json::json!({"cli":provider,"task":"wait", "executablePath":script,
            "env":{"FAKE_PID_FILE":ready},"cwd":dir.path()});
        child.stdin.take().unwrap().write_all(config.to_string().as_bytes()).unwrap();
        let deadline = Instant::now() + Duration::from_secs(5);
        while !ready.exists() {
            assert!(Instant::now() < deadline, "provider did not start");
            std::thread::sleep(Duration::from_millis(10));
        }
        let pids = std::fs::read_to_string(&ready).unwrap();
        let mut stdout = BufReader::new(child.stdout.take().unwrap());
        let mut first = String::new();
        stdout.read_line(&mut first).unwrap();
        assert!(first.contains("observed-session"), "provider identity must be observed before cancel");
        assert!(Command::new("kill").args([signal, &child.id().to_string()]).status().unwrap().success());
        loop {
            if child.try_wait().unwrap().is_some() { break; }
            assert!(Instant::now() < deadline, "wrapper did not reap its provider");
            std::thread::sleep(Duration::from_millis(10));
        }
        let mut remaining = String::new();
        stdout.read_to_string(&mut remaining).unwrap();
        let done = remaining.lines().filter_map(|line| serde_json::from_str::<serde_json::Value>(line).ok())
            .find(|event| event["type"] == "done").expect("terminal result");
        assert_eq!(done["result"]["sessionId"], "observed-session");
        assert_eq!(done["result"]["success"], false);
        for pid in pids.split_whitespace() {
            // A killed descendant can briefly be an init-owned zombie; neither
            // a missing process nor a zombie can perform more tool effects.
            let output = Command::new("ps").args(["-o", "stat=", "-p", pid]).output().unwrap();
            let state = String::from_utf8_lossy(&output.stdout);
            assert!(state.trim().is_empty() || state.trim().starts_with('Z'), "provider {pid} still alive: {state}");
        }
    }
}
