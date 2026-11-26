use orch::{codex::Codex, events::AgentEvent};
use serde_json::Value;
use std::{os::unix::fs::PermissionsExt, path::PathBuf, time::Duration};
use tokio::{process::Command, sync::watch};

fn executable(dir: &std::path::Path) -> PathBuf {
    let path = dir.join("fake-codex");
    std::fs::copy(
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/support/fake_codex.py"),
        &path,
    )
    .expect("copy fake");
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o700)).expect("executable");
    path
}

#[tokio::test]
async fn cli_smoke_uses_explicit_resume_cwd_policy_and_correlated_events() {
    let launch = tempfile::tempdir().expect("launch cwd");
    let session = tempfile::tempdir().expect("session cwd");
    let exe = executable(launch.path());
    let records = launch.path().join("records.jsonl");
    let output = Command::new(env!("CARGO_BIN_EXE_orch"))
        .args(["--smoke-test", "codex", "--codex-bin"])
        .arg(exe)
        .arg("--cwd")
        .arg(session.path())
        .env("ORCH_FAKE_RECORD", &records)
        .current_dir(launch.path())
        .output()
        .await
        .expect("orch CLI");
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let lines: Vec<Value> = String::from_utf8_lossy(&output.stdout)
        .lines()
        .map(|s| serde_json::from_str(s).expect("normalized event"))
        .collect();
    assert_eq!(
        lines
            .iter()
            .filter(|v| v["event"] == "turn_completed")
            .count(),
        2
    );
    assert!(
        lines
            .iter()
            .all(|v| v["session_id"] == lines[0]["session_id"])
    );
    assert!(lines.iter().any(|v| v["turn_id"] == 2));
    let records: Vec<Value> = std::fs::read_to_string(records)
        .expect("records")
        .lines()
        .map(|s| serde_json::from_str(s).expect("record"))
        .collect();
    assert_eq!(records.len(), 2);
    for record in &records {
        assert_eq!(record["cwd"], session.path().to_str().expect("utf8 path"));
        let args = record["argv"].as_array().expect("argv");
        assert_eq!(
            &args[..5],
            &["-a", "never", "--sandbox", "read-only", "exec"]
        );
        assert!(args.iter().any(|s| s == "--ignore-user-config"));
    }
    let args = records[1]["argv"].as_array().expect("resume argv");
    assert!(args.iter().any(|s| s == "resume"));
    assert!(args.iter().any(|s| s == "fixture-thread"));
    assert!(
        !args
            .last()
            .expect("second prompt")
            .as_str()
            .expect("prompt")
            .contains("ORCH_SMOKE_")
    );
    assert!(launch.path().join(".orch/orch.log").exists());
    assert!(!session.path().join(".orch").exists());
}

#[tokio::test]
async fn failures_emit_exactly_one_terminal_and_no_false_success() {
    let dir = tempfile::tempdir().expect("tempdir");
    for (executable, cwd) in [
        (dir.path().join("missing-codex"), dir.path().to_path_buf()),
        (PathBuf::from("codex"), dir.path().join("missing-cwd")),
    ] {
        let runner = Codex {
            executable,
            cwd,
            timeout: Duration::from_secs(1),
        };
        let (_sender, mut cancel) = watch::channel(false);
        let mut events = Vec::new();
        assert!(
            runner
                .turn("prompt", None, &mut cancel, |event| {
                    events.push(event);
                    Ok(())
                })
                .await
                .is_err()
        );
        assert_eq!(
            events
                .iter()
                .filter(|e| matches!(e, AgentEvent::TurnFailed { .. }))
                .count(),
            1
        );
        assert!(
            !events
                .iter()
                .any(|e| matches!(e, AgentEvent::TurnCompleted { .. }))
        );
    }
    let exe = executable(dir.path());
    for case in [
        "incomplete",
        "nonzero",
        "auth-failure",
        "provider-error",
        "wrong-thread",
    ] {
        let output = Command::new(env!("CARGO_BIN_EXE_orch"))
            .args(["--smoke-test", "codex", "--codex-bin"])
            .arg(&exe)
            .env("ORCH_FAKE_CASE", case)
            .current_dir(dir.path())
            .output()
            .await
            .expect("failed CLI");
        assert!(!output.status.success(), "case: {case}");
        let events: Vec<Value> = String::from_utf8_lossy(&output.stdout)
            .lines()
            .map(|s| serde_json::from_str(s).expect("event"))
            .collect();
        assert_eq!(
            events
                .iter()
                .filter(|e| e["event"] == "turn_failed")
                .count(),
            1,
            "case: {case}"
        );
        let failed_turn = events.last().expect("terminal event")["turn_id"].clone();
        assert!(
            !events
                .iter()
                .any(|e| e["turn_id"] == failed_turn && e["event"] == "turn_completed"),
            "case: {case}"
        );
    }
}

#[tokio::test]
async fn cli_interrupt_and_terminate_cancel_and_clean_children() {
    use nix::{
        sys::signal::{Signal, kill},
        unistd::Pid,
    };
    use tokio::time::{sleep, timeout};
    for signal in [Signal::SIGINT, Signal::SIGTERM] {
        let dir = tempfile::tempdir().expect("tempdir");
        let exe = executable(dir.path());
        let records = dir.path().join("pids.json");
        let child = Command::new(env!("CARGO_BIN_EXE_orch"))
            .args(["--smoke-test", "codex", "--codex-bin"])
            .arg(exe)
            .env("ORCH_FAKE_CASE", "cancel")
            .env("ORCH_FAKE_PIDS", &records)
            .current_dir(dir.path())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .expect("CLI process");
        timeout(Duration::from_secs(3), async {
            loop {
                if std::fs::read_to_string(&records)
                    .ok()
                    .and_then(|s| serde_json::from_str::<Value>(&s).ok())
                    .is_some()
                {
                    break;
                }
                sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("provider ready");
        kill(Pid::from_raw(child.id().expect("CLI pid") as i32), signal).expect("send signal");
        let output = timeout(Duration::from_secs(5), child.wait_with_output())
            .await
            .expect("bounded shutdown")
            .expect("CLI output");
        assert_eq!(output.status.code(), Some(130));
        let events: Vec<Value> = String::from_utf8_lossy(&output.stdout)
            .lines()
            .map(|s| serde_json::from_str(s).expect("event"))
            .collect();
        assert_eq!(
            events
                .iter()
                .filter(|e| e["event"] == "turn_cancelled")
                .count(),
            1
        );
        assert!(!events.iter().any(|e| e["event"] == "turn_completed"));
        let pids: Value = serde_json::from_str(&std::fs::read_to_string(records).expect("pids"))
            .expect("pids JSON");
        for key in ["leader", "descendant"] {
            let pid = pids[key].as_i64().expect("pid");
            timeout(Duration::from_secs(2), async {
                loop {
                    match std::fs::read_to_string(format!("/proc/{pid}/stat")) {
                        Err(e) if e.kind() == std::io::ErrorKind::NotFound => break,
                        Ok(stat)
                            if stat
                                .rsplit_once(") ")
                                .is_some_and(|(_, rest)| rest.starts_with('Z')) =>
                        {
                            break;
                        }
                        _ => sleep(Duration::from_millis(10)).await,
                    }
                }
            })
            .await
            .expect("owned process no longer executing");
        }
    }
}

#[tokio::test]
async fn a_failing_event_consumer_cannot_produce_a_successful_turn() {
    let dir = tempfile::tempdir().expect("tempdir");
    let runner = Codex {
        executable: executable(dir.path()),
        cwd: dir.path().to_path_buf(),
        timeout: Duration::from_secs(2),
    };
    let (_sender, mut cancel) = watch::channel(false);
    let mut events = Vec::new();
    let result = runner
        .turn("prompt", None, &mut cancel, |event| {
            if matches!(event, AgentEvent::MessageCompleted { .. }) {
                anyhow::bail!("event consumer closed");
            }
            events.push(event);
            Ok(())
        })
        .await;
    assert!(result.is_err());
    assert!(
        matches!(events.last(), Some(AgentEvent::TurnFailed { kind: orch::events::FailureKind::Io, message }) if message.contains("event consumer closed"))
    );
    assert!(
        !events
            .iter()
            .any(|event| matches!(event, AgentEvent::TurnCompleted { .. }))
    );
}

#[tokio::test]
async fn cli_timeout_emits_one_failure_and_stops_before_resume() {
    let dir = tempfile::tempdir().expect("tempdir");
    let exe = executable(dir.path());
    let output = Command::new(env!("CARGO_BIN_EXE_orch"))
        .args([
            "--smoke-test",
            "codex",
            "--timeout-secs",
            "1",
            "--codex-bin",
        ])
        .arg(exe)
        .env("ORCH_FAKE_CASE", "timeout")
        .current_dir(dir.path())
        .output()
        .await
        .expect("CLI timeout");
    assert_eq!(output.status.code(), Some(1));
    let events: Vec<Value> = String::from_utf8_lossy(&output.stdout)
        .lines()
        .map(|line| serde_json::from_str(line).expect("event"))
        .collect();
    assert_eq!(
        events
            .iter()
            .filter(|event| event["event"] == "turn_failed")
            .count(),
        1
    );
    let failed = events.last().expect("terminal event");
    assert_eq!(failed["event"], "turn_failed");
    assert_eq!(failed["kind"], "timeout");
    assert!(events.iter().all(|event| event["turn_id"] == 1));
    assert!(
        !events
            .iter()
            .any(|event| event["event"] == "turn_completed")
    );
}
