use std::{
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
};

use serde_json::{Value, json};
use tokio::process::Command;

const MODEL: &str = "gemini-3.8-flash-low";
const TURN: &str = include_str!("fixtures/agy-1.5.17/turn-2.jsonl");

fn events(bytes: &[u8]) -> Vec<Value> {
    std::str::from_utf8(bytes)
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect()
}

fn command(dir: &Path) -> Command {
    let script = dir.join("fake-agy");
    std::fs::copy(
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/support/fake_codex.py"),
        &script,
    )
    .unwrap();
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o700)).unwrap();
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_orch"));
    cmd.args([
        "--smoke-test",
        "agy",
        "--model",
        MODEL,
        "--timeout-secs",
        "5",
        "--agy-bin",
    ])
    .arg(script)
    .current_dir(dir)
    .kill_on_drop(true);
    cmd
}

#[tokio::test]
async fn smoke_pins_model_cwd_permissions_and_conversation_on_both_turns() {
    let launch = tempfile::tempdir().unwrap();
    let session = tempfile::tempdir().unwrap();
    let records = launch.path().join("records");
    let output = command(launch.path())
        .args(["--cwd"])
        .arg(session.path())
        .env("ORCH_FAKE_RECORD", &records)
        .output()
        .await
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let events = events(&output.stdout);
    for turn in [1, 2] {
        let turn_events: Vec<_> = events
            .iter()
            .filter(|event| event["turn_id"] == turn)
            .collect();
        assert_eq!(
            turn_events
                .iter()
                .map(|event| event["event"].as_str().unwrap())
                .collect::<Vec<_>>(),
            [
                "turn_started",
                "session_initialized",
                "text_delta",
                "turn_completed"
            ]
        );
        assert_eq!(turn_events[1]["provider_session_id"], "fixture-agy");
        assert_eq!(turn_events[2]["text"], turn_events[3]["final_text"]);
    }
    let records = self::events(&std::fs::read(records).unwrap());
    assert_eq!(records.len(), 2);
    for record in &records {
        assert_eq!(record["cwd"], session.path().to_str().unwrap());
        assert_eq!(
            &record["argv"].as_array().unwrap()[..8],
            [
                "--model=gemini-3.8-flash-low",
                "--mode",
                "plan",
                "--sandbox",
                "--settings",
                r#"{"toolPermission":"strict"}"#,
                "--output-format",
                "stream-json",
            ]
        );
    }
    assert_eq!(records[0]["argv"].as_array().unwrap().len(), 9);
    assert_eq!(records[1]["argv"][8], "--conversation=fixture-agy");
    assert!(
        !records[1]["argv"][9]
            .as_str()
            .unwrap()
            .contains("ORCH_SMOKE_")
    );
}

#[tokio::test]
async fn captured_deltas_and_result_are_distinct() {
    let dir = tempfile::tempdir().unwrap();
    let fixture = dir.path().join("fixture.jsonl");
    std::fs::write(&fixture, TURN).unwrap();
    let output = command(dir.path())
        .env("ORCH_FAKE_AGY_FIXTURE", fixture)
        .output()
        .await
        .unwrap();
    // The fixture response is deliberately different from the smoke CLI's expected OK.
    assert_eq!(output.status.code(), Some(1));
    let events = events(&output.stdout);
    let deltas: Vec<_> = events
        .iter()
        .filter(|e| e["event"] == "text_delta")
        .collect();
    assert_eq!(
        deltas
            .iter()
            .map(|e| e["text"].as_str().unwrap())
            .collect::<Vec<_>>(),
        ["ORCH", "-AGY-1A-9B2D", "\n"]
    );
    assert!(deltas.iter().all(|e| e["message_id"] == "4"));
    assert_eq!(events.last().unwrap()["final_text"], "ORCH-AGY-1A-9B2D\n");
    assert!(!events.iter().any(|e| e["event"] == "message_completed"));
}

#[tokio::test]
async fn protocol_permission_provider_and_exit_failures_are_terminal() {
    let dir = tempfile::tempdir().unwrap();
    let fixture = dir.path().join("fixture.jsonl");
    let first = include_str!("fixtures/agy-1.5.17/turn-1.jsonl");
    let rejected = include_str!("fixtures/agy-1.5.17/permission-denied.jsonl");
    // Mutations of the captured events are synthetic protocol failures.
    let oversized_delta = first
        .lines()
        .nth(2)
        .unwrap()
        .replace("OK", &"x".repeat(900_000));
    let flooding = format!(
        "{}\n{}\n{}\n",
        first.lines().next().unwrap(),
        [oversized_delta.as_str(); 5].join("\n"),
        first.lines().last().unwrap()
    );
    let cases = [
        (flooding, "0", "protocol"),
        (
            first.replace("/tmp/orch-agy-fixture/cwd", "/wrong/cwd"),
            "0",
            "protocol",
        ),
        (rejected.to_owned(), "0", "permission"),
        (
            rejected.replace("\"response\":\"\"", "\"response\":\"partial response\""),
            "0",
            "permission",
        ),
        (
            include_str!("fixtures/agy-1.5.17/invalid-model.jsonl").to_owned(),
            "1",
            "provider",
        ),
        (
            include_str!("fixtures/agy-1.5.17/invalid-input.jsonl").to_owned(),
            "1",
            "provider",
        ),
        (first.to_owned(), "7", "process"),
        (first.replace(MODEL, "unexpected-model"), "0", "protocol"),
        (first.replace("\"strict\"", "\"auto\""), "0", "permission"),
        (
            first.replace("\"step_index\":1", "\"unknown\":1"),
            "0",
            "protocol",
        ),
        (
            first.replace("\"response\":", "\"unknown\":"),
            "0",
            "protocol",
        ),
        (
            first.replace("\"status\":", "\"unknown\":"),
            "0",
            "protocol",
        ),
        (
            first.replace("\"status\":", "\"denied_actions\":true,\"status\":"),
            "0",
            "protocol",
        ),
        (first.lines().last().unwrap().to_owned(), "0", "protocol"),
        (
            first.lines().take(3).collect::<Vec<_>>().join("\n"),
            "0",
            "protocol",
        ),
        (
            format!("{first}{}", first.lines().last().unwrap()),
            "0",
            "protocol",
        ),
        (
            format!("{}\n{first}", first.lines().next().unwrap()),
            "0",
            "protocol",
        ),
    ];
    for (data, status, kind) in cases {
        std::fs::write(&fixture, &data).unwrap();
        let output = command(dir.path())
            .env("ORCH_FAKE_AGY_FIXTURE", &fixture)
            .env("ORCH_FAKE_EXIT", status)
            .output()
            .await
            .unwrap();
        assert_eq!(output.status.code(), Some(1));
        let events = events(&output.stdout);
        let terminal = events.last().unwrap();
        assert_eq!(terminal["event"], "turn_failed", "{terminal}");
        assert_eq!(terminal["kind"], kind, "{terminal}");
        assert_eq!(
            events
                .iter()
                .filter(|e| e["event"].as_str().unwrap().starts_with("turn_"))
                .count(),
            2
        );
        if data.contains("partial response") {
            assert!(
                terminal["message"]
                    .as_str()
                    .unwrap()
                    .contains("partial response")
            );
        }
    }
}

#[tokio::test]
async fn handles_unknown_and_invalid_lines_without_adding_duplicate_full_messages() {
    let dir = tempfile::tempdir().unwrap();
    let mut cmd = command(dir.path());
    let exe = dir.path().join("fake-agy");
    let capture: Vec<_> = TURN.lines().collect();
    let mut init: Value = serde_json::from_str(capture[0]).unwrap();
    init["init"]["cwd"] = json!(dir.path());
    let script = format!(
        "#!/bin/sh\ncat <<'EOF'\n{init}\nnot-json\n{{\"event\":\"future\"}}\n{}\nEOF\n",
        capture.last().unwrap()
    );
    std::fs::write(&exe, script).unwrap();
    std::fs::set_permissions(&exe, std::fs::Permissions::from_mode(0o700)).unwrap();
    let output = cmd.output().await.unwrap();
    let events = events(&output.stdout);
    let message = events
        .iter()
        .find(|e| e["event"] == "message_completed")
        .unwrap();
    assert_eq!(message["text"], events.last().unwrap()["final_text"]);
}
