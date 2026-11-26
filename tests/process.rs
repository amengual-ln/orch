use nix::{
    errno::Errno,
    sys::wait::{WaitPidFlag, waitpid},
    unistd::Pid,
};
use orch::process::{self, Stop};
use serde_json::Value;
use std::{path::PathBuf, time::Duration};
use tokio::{process::Command, sync::watch, time::sleep};

fn fake(case: &str, cwd: &std::path::Path) -> Command {
    let mut command = Command::new("python3");
    command
        .arg(PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/support/fake_codex.py"))
        .env("ORCH_FAKE_CASE", case)
        .current_dir(cwd);
    command
}

#[tokio::test]
async fn drains_both_pipes_and_bounds_oversized_data() {
    let dir = tempfile::tempdir().expect("tempdir");
    for case in ["flood", "oversized", "invalid-json"] {
        let (_sender, mut cancel) = watch::channel(false);
        let mut parser = orch::codex::Parser::default();
        let output = process::run(
            &mut fake(case, dir.path()),
            Duration::from_secs(5),
            &mut cancel,
            |line| {
                parser.line(line);
                Ok(())
            },
        )
        .await
        .expect("child run");
        assert!(matches!(output.stop, Stop::Exited));
        assert!(output.status.success());
        assert!(output.stderr.len() <= 16 * 1024);
        if case == "flood" {
            assert_eq!(output.stderr.len(), 16 * 1024);
        }
        assert_eq!(
            parser.finish(None).expect("complete stream").final_text,
            "OK"
        );
    }
}

async fn assert_not_running(pid: i32) {
    for _ in 0..100 {
        match std::fs::read_to_string(format!("/proc/{pid}/stat")) {
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return,
            Ok(stat)
                if stat
                    .rsplit_once(") ")
                    .is_some_and(|(_, rest)| rest.starts_with('Z')) =>
            {
                return;
            }
            _ => sleep(Duration::from_millis(10)).await,
        }
    }
    panic!("process {pid} is still executing");
}

#[tokio::test]
async fn timeout_cancel_and_normal_leader_exit_clean_the_owned_group() {
    let dir = tempfile::tempdir().expect("tempdir");
    for case in ["timeout", "cancel", "closed-pipes"] {
        let (sender, mut cancel) = watch::channel(false);
        let mut pids = None;
        let output = process::run(
            &mut fake(case, dir.path()),
            Duration::from_millis(500),
            &mut cancel,
            |line| {
                let value: Value = serde_json::from_slice(line)?;
                if value["type"] == "inspection" {
                    pids = Some((
                        value["leader"].as_i64().expect("leader") as i32,
                        value["descendant"].as_i64().expect("descendant") as i32,
                    ));
                    if case == "cancel" {
                        sender.send(true)?;
                    }
                }
                Ok(())
            },
        )
        .await
        .expect("controlled shutdown");
        match case {
            "timeout" => assert!(matches!(output.stop, Stop::Timeout)),
            "cancel" => assert!(matches!(output.stop, Stop::Cancelled)),
            _ => assert!(matches!(output.stop, Stop::Exited)),
        }
        let (leader, descendant) = pids.expect("child reported pids");
        assert_eq!(
            waitpid(Pid::from_raw(leader), Some(WaitPidFlag::WNOHANG)),
            Err(Errno::ECHILD),
            "leader must already be reaped"
        );
        assert_not_running(leader).await;
        assert_not_running(descendant).await;
    }
}

#[tokio::test]
async fn dropping_the_runner_future_kills_the_owned_group() {
    let dir = tempfile::tempdir().expect("tempdir");
    let mut command = fake("cancel", dir.path());
    let (_sender, mut cancel) = watch::channel(false);
    let (ready, receiver) = tokio::sync::oneshot::channel();
    let task = tokio::spawn(async move {
        let mut ready = Some(ready);
        process::run(&mut command, Duration::from_secs(10), &mut cancel, |line| {
            let value: Value = serde_json::from_slice(line)?;
            if value["type"] == "inspection"
                && let Some(ready) = ready.take()
            {
                let _ = ready.send((
                    value["leader"].as_i64().expect("leader") as i32,
                    value["descendant"].as_i64().expect("descendant") as i32,
                ));
            }
            Ok(())
        })
        .await
    });
    let (leader, descendant) = tokio::time::timeout(Duration::from_secs(3), receiver)
        .await
        .expect("ready deadline")
        .expect("pids");
    task.abort();
    assert!(task.await.expect_err("task aborted").is_cancelled());
    assert_not_running(leader).await;
    assert_not_running(descendant).await;
}
