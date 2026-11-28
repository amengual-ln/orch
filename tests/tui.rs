use std::{path::Path, time::Duration};

use tokio::{process::Command, time::timeout};

async fn pty(scenario: &str) {
    let script = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/support/tui_pty.py");
    let output = timeout(
        Duration::from_secs(45),
        Command::new("python3")
            .arg(script)
            .arg(env!("CARGO_BIN_EXE_orch"))
            .arg(scenario)
            .kill_on_drop(true)
            .output(),
    )
    .await
    .expect("bounded PTY scenario")
    .expect("run PTY scenario");
    assert!(
        output.status.success(),
        "{scenario}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

#[tokio::test]
async fn two_sessions_from_the_keyboard_keep_draft_scroll_and_isolate_cancel() {
    pty("flow").await;
}

#[tokio::test]
async fn startup_failure_restores_the_partial_terminal() {
    pty("partial").await;
}

#[tokio::test]
async fn external_signal_restores_terminal() {
    pty("interrupted").await;
}
