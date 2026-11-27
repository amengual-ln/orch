use std::{
    fs, io,
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
    time::Duration,
};

use orch::{
    agy::Agy,
    codex::Codex,
    events::AgentEvent,
    session::{CommandError, Provider, Runner, Session, Sessions, Status},
};
use serde_json::Value;
use tokio::time::{sleep, timeout};

const BOUND: Duration = Duration::from_secs(8);

fn runner(cwd: &Path, agy: bool) -> Runner {
    // Use per-session state on disk to drive the synthetic helper without changing process-wide env.
    let source = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/support/fake_codex.py");
    let script = cwd.join("provider");
    fs::write(
        &script,
        format!(
            "#!/usr/bin/env python3\nimport os, runpy\nfrom pathlib import Path\nc = Path('case')\nos.environ['ORCH_FAKE_CASE'] = c.read_text() if c.exists() else 'normal'\nos.environ['ORCH_FAKE_PIDS'] = 'pids'\nos.environ['ORCH_FAKE_RECORD'] = 'argv.jsonl'\nrunpy.run_path({source:?}, run_name='__main__')\n"
        ),
    )
    .unwrap();
    fs::set_permissions(&script, fs::Permissions::from_mode(0o700)).unwrap();
    if agy {
        Runner::Agy(Agy {
            executable: script,
            cwd: cwd.into(),
            timeout: BOUND,
            model: "gemini-3.8-flash-low".into(),
        })
    } else {
        Runner::Codex(Codex {
            executable: script,
            cwd: cwd.into(),
            timeout: BOUND,
        })
    }
}

fn case(cwd: &Path, value: &str) {
    fs::write(cwd.join("case"), value).unwrap();
}

async fn until(session: &mut Session, expected: Status) {
    timeout(BOUND, async {
        while session.inspect(|state| state.status) != expected {
            session.changed().await.unwrap();
        }
    })
    .await
    .expect("session state deadline");
}

async fn pids(cwd: &Path) -> Value {
    timeout(BOUND, async {
        loop {
            if let Some(value) = fs::read_to_string(cwd.join("pids"))
                .ok()
                .and_then(|data| serde_json::from_str(&data).ok())
            {
                return value;
            }
            sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("provider ready")
}

async fn cleaned(pids: &Value) {
    let leader = pids["leader"].as_u64().unwrap();
    let descendant = pids["descendant"].as_u64().unwrap();
    assert!(
        !PathBuf::from(format!("/proc/{leader}")).exists(),
        "leader must have been reaped"
    );
    timeout(BOUND, async {
        loop {
            match fs::read_to_string(format!("/proc/{descendant}/stat")) {
                Err(error) if error.kind() == io::ErrorKind::NotFound => break,
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
    .expect("descendant stopped");
}

fn terminals(session: &Session) -> Vec<(u64, AgentEvent)> {
    session.inspect(|state| {
        state
            .history
            .iter()
            .flat_map(|turn| {
                turn.events
                    .iter()
                    .filter(|event| {
                        matches!(
                            event,
                            AgentEvent::TurnCompleted { .. }
                                | AgentEvent::TurnFailed { .. }
                                | AgentEvent::TurnCancelled
                        )
                    })
                    .map(|event| (turn.id, event.clone()))
            })
            .collect()
    })
}

#[tokio::test]
async fn concurrent_sessions_cancel_selectively_and_shutdown_reaps_the_other() {
    let codex_dir = tempfile::tempdir().unwrap();
    let agy_dir = tempfile::tempdir().unwrap();
    let mut sessions = Sessions::default();
    let mut codex = sessions
        .create("codex", runner(codex_dir.path(), false))
        .unwrap();
    let mut agy = sessions
        .create("agy", runner(agy_dir.path(), true))
        .unwrap();
    assert_ne!(codex.id(), agy.id());
    assert_eq!(codex.provider(), &Provider::Codex);
    assert_eq!(
        agy.provider(),
        &Provider::Agy {
            model: "gemini-3.8-flash-low".into()
        }
    );
    assert_eq!(agy.cwd(), agy_dir.path());

    let (first, duplicate, other) = tokio::join!(
        codex.send_turn("remember".into()),
        codex.send_turn("reject concurrent submission".into()),
        agy.send_turn("hello".into()),
    );
    assert_eq!((first.unwrap(), other.unwrap()), (1, 1));
    assert_eq!(duplicate, Err(CommandError::Busy));
    until(&mut codex, Status::Idle).await;
    until(&mut agy, Status::Idle).await;
    let original_id = codex
        .inspect(|state| state.provider_session_id.clone())
        .unwrap();

    case(codex_dir.path(), "cancel");
    case(agy_dir.path(), "cancel");
    let (first, other) = tokio::join!(
        codex.send_turn("cancel this".into()),
        agy.send_turn("keep this running".into())
    );
    assert_eq!((first.unwrap(), other.unwrap()), (2, 2));
    let (codex_pids, agy_pids) = tokio::join!(pids(codex_dir.path()), pids(agy_dir.path()));
    assert_eq!(
        codex.send_turn("no queue".into()).await,
        Err(CommandError::Busy)
    );
    assert_eq!(
        agy.send_turn("no queue".into()).await,
        Err(CommandError::Busy)
    );

    codex.cancel().await.unwrap();
    until(&mut codex, Status::Idle).await;
    assert_eq!(agy.inspect(|state| state.status), Status::Running);
    assert_eq!(terminals(&codex)[1], (2, AgentEvent::TurnCancelled));
    assert_eq!(terminals(&agy).len(), 1);
    cleaned(&codex_pids).await;

    case(codex_dir.path(), "normal");
    assert_eq!(
        codex.send_turn("resume after cancel".into()).await.unwrap(),
        3
    );
    until(&mut codex, Status::Idle).await;
    codex.inspect(|state| {
        assert_eq!(state.history[2].prompt, "resume after cancel");
        assert_eq!(
            state.provider_session_id.as_deref(),
            Some(original_id.as_str())
        );
        assert!(
            state.history[2]
                .events
                .contains(&AgentEvent::SessionInitialized {
                    provider_session_id: original_id.clone(),
                })
        );
    });
    let launches = fs::read_to_string(codex_dir.path().join("argv.jsonl")).unwrap();
    let last: Value = serde_json::from_str(launches.lines().last().unwrap()).unwrap();
    assert_eq!(last["cwd"], codex_dir.path().to_str().unwrap());
    assert!(
        last["argv"]
            .as_array()
            .unwrap()
            .contains(&Value::from(original_id))
    );

    fs::remove_file(codex_dir.path().join("pids")).unwrap();
    case(codex_dir.path(), "cancel");
    assert_eq!(codex.send_turn("global shutdown".into()).await.unwrap(), 4);
    let last_codex_pids = pids(codex_dir.path()).await;
    sessions.shutdown().await.unwrap();
    cleaned(&last_codex_pids).await;
    assert_eq!(codex.inspect(|state| state.status), Status::Closed);
    assert_eq!(agy.inspect(|state| state.status), Status::Closed);
    assert_eq!(terminals(&codex)[3], (4, AgentEvent::TurnCancelled));
    assert_eq!(terminals(&agy)[1], (2, AgentEvent::TurnCancelled));
    assert_eq!(
        agy.send_turn("closed".into()).await,
        Err(CommandError::Closed)
    );
    assert_eq!(agy.cancel().await, Err(CommandError::Closed));
    cleaned(&agy_pids).await;
}

#[tokio::test]
async fn failures_preserve_the_known_id_and_only_an_explicit_turn_retries() {
    let dir = tempfile::tempdir().unwrap();
    let mut sessions = Sessions::default();
    let mut session = sessions.create("test", runner(dir.path(), false)).unwrap();
    session.cancel().await.unwrap();

    case(dir.path(), "provider-error");
    session.send_turn("initial failure".into()).await.unwrap();
    until(&mut session, Status::Error).await;
    assert_eq!(
        session
            .inspect(|s| s.provider_session_id.clone())
            .as_deref(),
        Some("fixture-thread")
    );
    assert_eq!(terminals(&session).len(), 1);

    case(dir.path(), "wrong-thread");
    assert_eq!(
        session
            .send_turn("wrong conversation".into())
            .await
            .unwrap(),
        2
    );
    until(&mut session, Status::Error).await;
    assert_eq!(
        session
            .inspect(|s| s.provider_session_id.clone())
            .as_deref(),
        Some("fixture-thread")
    );
    assert!(
        matches!(&terminals(&session)[1].1, AgentEvent::TurnFailed { message, .. } if message.contains("different conversation"))
    );
    let launches = fs::read_to_string(dir.path().join("argv.jsonl")).unwrap();
    assert_eq!(launches.lines().count(), 2);

    session.close();
    until(&mut session, Status::Closed).await;
    sessions.shutdown().await.unwrap();
}

#[tokio::test]
async fn validates_alias_cwd_model_and_prompt_before_accepting_a_turn() {
    let dir = tempfile::tempdir().unwrap();
    let mut sessions = Sessions::default();
    let session = sessions.create(" one ", runner(dir.path(), false)).unwrap();
    assert_eq!(session.alias(), "one");
    assert!(sessions.create("one", runner(dir.path(), false)).is_err());
    assert!(sessions.create(" ", runner(dir.path(), false)).is_err());
    let mut invalid = Agy {
        executable: "agy".into(),
        cwd: dir.path().to_path_buf(),
        timeout: BOUND,
        model: " ".into(),
    };
    assert!(
        sessions
            .create("invalid model", Runner::Agy(invalid))
            .is_err()
    );
    invalid = Agy {
        executable: "agy".into(),
        cwd: dir.path().join("missing"),
        timeout: BOUND,
        model: "fake-model".into(),
    };
    assert!(
        sessions
            .create("invalid cwd", Runner::Agy(invalid))
            .is_err()
    );
    assert_eq!(
        session.send_turn("x".repeat(65_537)).await,
        Err(CommandError::PromptTooLong)
    );
    assert!(session.inspect(|state| state.history.is_empty()));
    sessions.shutdown().await.unwrap();
}

#[tokio::test]
async fn history_limits_preserve_the_terminal_event_and_reject_new_turns() {
    let cwd = tempfile::tempdir().unwrap();
    let mut sessions = Sessions::default();
    let Runner::Codex(provider) = runner(cwd.path(), false) else {
        panic!("expected synthetic Codex runner");
    };
    // Synthetic provider repeatedly replaces the same message. The parser is bounded
    // independently from the retained history of revisions.
    fs::write(
        &provider.executable,
        r#"#!/usr/bin/env python3
import json
def emit(value): print(json.dumps(value), flush=True)
emit({"type": "thread.started", "thread_id": "history"})
emit({"type": "turn.started"})
for i in range(12):
    emit({"type": "item.completed", "item": {"type": "agent_message", "id": "m", "text": str(i) + ("x" * 900_000)}})
emit({"type": "turn.completed"})
"#,
    ).unwrap();
    let mut session = sessions.create("history", Runner::Codex(provider)).unwrap();
    for id in 1..=3 {
        assert_eq!(session.send_turn("fill".into()).await.unwrap(), id);
        until(&mut session, Status::Error).await;
    }
    let terminal = terminals(&session);
    assert_eq!(terminal.len(), 3);
    assert!(terminal.iter().all(|(_, event)| matches!(
        event, AgentEvent::TurnFailed { kind: orch::events::FailureKind::Io, message }
        if message.contains("turn event history exceeded memory limit")
    )));
    assert_eq!(
        session.send_turn("overflow".into()).await,
        Err(CommandError::HistoryFull)
    );
    assert_eq!(session.inspect(|s| s.history.len()), 3);
    sessions.shutdown().await.unwrap();
}

#[tokio::test]
async fn cli_concurrent_smoke_reports_two_correlated_conversations() {
    let launch = tempfile::tempdir().unwrap();
    let codex = tempfile::tempdir().unwrap();
    let agy = tempfile::tempdir().unwrap();
    let Runner::Codex(c) = runner(codex.path(), false) else {
        panic!("expected Codex")
    };
    let Runner::Agy(a) = runner(agy.path(), true) else {
        panic!("expected agy")
    };
    let mut command = tokio::process::Command::new(env!("CARGO_BIN_EXE_orch"));
    command
        .args([
            "--smoke-test",
            "concurrent",
            "--timeout-secs",
            "3",
            "--model",
            &a.model,
            "--cwd",
        ])
        .arg(&c.cwd)
        .arg("--agy-cwd")
        .arg(&a.cwd)
        .arg("--codex-bin")
        .arg(c.executable)
        .arg("--agy-bin")
        .arg(a.executable)
        .current_dir(launch.path())
        .kill_on_drop(true);
    let output = timeout(BOUND, command.output()).await.unwrap().unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let events: Vec<Value> = std::str::from_utf8(&output.stdout)
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    for (session, provider_id) in [
        ("session-1", "fixture-thread"),
        ("session-2", "fixture-agy"),
    ] {
        for turn in [1, 2] {
            let events: Vec<_> = events
                .iter()
                .filter(|e| e["session_id"] == session && e["turn_id"] == turn)
                .collect();
            assert_eq!(events[0]["event"], "turn_started");
            assert_eq!(events[1]["provider_session_id"], provider_id);
            assert_eq!(events.last().unwrap()["event"], "turn_completed");
            assert_eq!(
                events
                    .iter()
                    .filter(|e| e["event"] == "turn_completed")
                    .count(),
                1
            );
        }
    }
}

#[test]
fn an_aborted_controller_leaves_a_terminal_result_in_history() {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    let cwd = tempfile::tempdir().unwrap();
    case(cwd.path(), "cancel");
    let (_sessions, session) = runtime.block_on(async {
        let mut sessions = Sessions::default();
        let session = sessions
            .create("abrupt exit", runner(cwd.path(), false))
            .unwrap();
        session.send_turn("active".into()).await.unwrap();
        pids(cwd.path()).await;
        (sessions, session)
    });
    drop(runtime);
    assert_eq!(session.inspect(|state| state.status), Status::Closed);
    assert!(matches!(
        terminals(&session).as_slice(),
        [(
            1,
            AgentEvent::TurnFailed {
                kind: orch::events::FailureKind::Process,
                ..
            }
        )]
    ));
}
