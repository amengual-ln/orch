use std::{
    fs::{self, OpenOptions},
    io::{self, Write},
    os::unix::fs::{DirBuilderExt, OpenOptionsExt},
    path::PathBuf,
    process::ExitCode,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use anyhow::{Context, Result, ensure};
use clap::{Parser, ValueEnum};
use orch::{
    agy::Agy,
    codex::Codex,
    events::{AgentEvent, Event},
    runner::{TurnResult, validate_cwd},
};
use tokio::{
    signal::unix::{SignalKind, signal},
    sync::watch,
};

#[derive(Clone, ValueEnum)]
enum Provider {
    Codex,
    Agy,
}

#[derive(Parser)]
#[command(version, about = "Headless coding-agent runner (phase 1C)")]
struct Args {
    #[arg(long, value_enum)]
    smoke_test: Provider,
    #[arg(long, default_value = ".")]
    cwd: PathBuf,
    #[arg(long, default_value = "120", value_parser = clap::value_parser!(u64).range(1..=86400))]
    timeout_secs: u64,
    /// Override the executable, useful for a pinned CLI installation.
    #[arg(long, default_value = "codex")]
    codex_bin: PathBuf,
    /// Override the agy executable.
    #[arg(long, default_value = "agy")]
    agy_bin: PathBuf,
    /// Pin the model slug for both agy turns.
    #[arg(long, required_if_eq("smoke_test", "agy"), value_parser = clap::builder::NonEmptyStringValueParser::new())]
    model: Option<String>,
}

fn logging() -> Result<()> {
    // Anchored once, before any session process uses a different working directory.
    let dir = std::env::current_dir()?.join(".orch");
    fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(&dir)?;
    let file = OpenOptions::new()
        .create(true)
        .append(true)
        .mode(0o600)
        .open(dir.join("orch.log"))?;
    tracing_subscriber::fmt()
        .with_ansi(false)
        .with_writer(file)
        .try_init()
        .map_err(|e| anyhow::anyhow!("could not initialize logging: {e}"))?;
    Ok(())
}

enum Runner {
    Codex(Codex),
    Agy(Agy),
}

impl Runner {
    async fn turn(
        &self,
        prompt: &str,
        resume: Option<&str>,
        cancel: &mut watch::Receiver<bool>,
        emit: impl FnMut(AgentEvent) -> Result<()>,
    ) -> Result<TurnResult> {
        match self {
            Self::Codex(runner) => runner.turn(prompt, resume, cancel, emit).await,
            Self::Agy(runner) => runner.turn(prompt, resume, cancel, emit).await,
        }
    }
}

fn resolve_executable(path: PathBuf, flag: &str) -> Result<PathBuf> {
    if path.is_absolute()
        || path
            .parent()
            .is_some_and(|parent| !parent.as_os_str().is_empty())
    {
        return path
            .canonicalize()
            .with_context(|| format!("invalid {flag} path"));
    }
    Ok(path)
}

async fn smoke(args: Args, cancel: &mut watch::Receiver<bool>) -> Result<()> {
    let cwd = validate_cwd(&args.cwd)?;
    let timeout = Duration::from_secs(args.timeout_secs);
    let runner = match args.smoke_test {
        Provider::Codex => Runner::Codex(Codex {
            executable: resolve_executable(args.codex_bin, "--codex-bin")?,
            cwd,
            timeout,
        }),
        Provider::Agy => Runner::Agy(Agy {
            executable: resolve_executable(args.agy_bin, "--agy-bin")?,
            cwd,
            timeout,
            model: args.model.context("--smoke-test agy requires --model")?,
        }),
    };
    let stamp = SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos();
    let session_id = format!("smoke-{}-{stamp}", std::process::id());
    let marker = format!("ORCH_SMOKE_{}_{stamp}", std::process::id());
    let mut stdout = io::stdout().lock();
    let mut send = |turn_id, payload| -> Result<()> {
        serde_json::to_writer(
            &mut stdout,
            &Event {
                session_id: &session_id,
                turn_id,
                payload,
            },
        )?;
        stdout.write_all(b"\n")?;
        stdout.flush()?;
        Ok(())
    };
    let first = runner.turn(
        &format!("No uses herramientas ni leas archivos. Recordá el marcador {marker} para el próximo turno de esta conversación. Respondé únicamente OK."),
        None, cancel, |event| send(1, event),
    ).await?;
    ensure!(
        first.final_text.trim() == "OK",
        "first smoke response was not OK"
    );
    let second = runner.turn(
        "Sin usar herramientas ni leer archivos: ¿cuál era el marcador que te pedí recordar en el turno anterior? Respondé únicamente con ese marcador.",
        Some(&first.provider_session_id), cancel, |event| send(2, event),
    ).await?;
    ensure!(
        second.final_text.trim() == marker,
        "resumed conversation did not recall the marker"
    );
    tracing::info!("two-turn smoke test passed");
    Ok(())
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> ExitCode {
    let args = Args::parse();
    let (sender, mut cancel) = watch::channel(false);
    let result = async {
        logging()?;
        let mut interrupt = signal(SignalKind::interrupt())?;
        let mut terminate = signal(SignalKind::terminate())?;
        let monitor = tokio::spawn(async move {
            tracing::info!("shutdown signal monitor ready");
            tokio::select! { _ = interrupt.recv() => {}, _ = terminate.recv() => {} }
            tracing::info!("shutdown signal received");
            let _ = sender.send(true);
        });
        let result = smoke(args, &mut cancel).await;
        monitor.abort();
        result
    }
    .await;
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            // Headless CLI only; no TUI is active in this phase.
            eprintln!("orch: {error:#}");
            ExitCode::from(if *cancel.borrow() { 130 } else { 1 })
        }
    }
}
