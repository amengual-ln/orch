mod tui;

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
    session::{Runner, Session, Sessions},
};
use tokio::{
    signal::unix::{SignalKind, signal},
    sync::watch,
};

#[derive(Clone, ValueEnum)]
enum Provider {
    Codex,
    Agy,
    Concurrent,
}

#[derive(Parser)]
#[command(version, about = "Keyboard-driven coding-agent sessions")]
struct Args {
    #[arg(long, value_enum)]
    smoke_test: Option<Provider>,
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
    #[arg(long, required_if_eq_any([("smoke_test", "agy"), ("smoke_test", "concurrent")]), value_parser = clap::builder::NonEmptyStringValueParser::new())]
    model: Option<String>,
    /// agy working directory for the concurrent smoke test.
    #[arg(long, required_if_eq("smoke_test", "concurrent"))]
    agy_cwd: Option<PathBuf>,
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

fn emit(session_id: &str, turn_id: u64, payload: AgentEvent) -> Result<()> {
    let mut stdout = io::stdout().lock();
    serde_json::to_writer(
        &mut stdout,
        &Event {
            session_id,
            turn_id,
            payload,
        },
    )?;
    stdout.write_all(b"\n")?;
    stdout.flush()?;
    Ok(())
}

async fn smoke_turn(
    session: &mut Session,
    prompt: String,
    cancel: &mut watch::Receiver<bool>,
) -> Result<String> {
    ensure!(!*cancel.borrow(), "smoke test cancelled");
    let turn_id = session.send_turn(prompt).await?;
    let mut seen = 0;
    let mut stopping = false;
    loop {
        let events = session
            .inspect(|snapshot| {
                snapshot
                    .history
                    .last()
                    .map(|turn| turn.events[seen..].to_vec())
            })
            .context("accepted turn is missing from history")?;
        seen += events.len();
        for event in events {
            emit(session.id(), turn_id, event.clone())?;
            match event {
                AgentEvent::TurnCompleted { final_text } => return Ok(final_text),
                AgentEvent::TurnFailed { message, .. } => anyhow::bail!(message),
                AgentEvent::TurnCancelled => anyhow::bail!("turn cancelled"),
                _ => {}
            }
        }
        tokio::select! {
            result = session.changed() => result?,
            _ = cancel.wait_for(|value| *value), if !stopping => {
                stopping = true;
                session.close();
            }
        }
    }
}

async fn smoke_session(
    mut session: Session,
    stamp: u128,
    mut cancel: watch::Receiver<bool>,
) -> Result<()> {
    let marker = format!("ORCH_SMOKE_{}_{stamp}", std::process::id());
    let first = smoke_turn(
        &mut session,
        format!("No uses herramientas ni leas archivos. Recordá el marcador {marker} para el próximo turno de esta conversación. Respondé únicamente OK."),
        &mut cancel,
    ).await?;
    ensure!(first.trim() == "OK", "first smoke response was not OK");
    let second = smoke_turn(
        &mut session,
        "Sin usar herramientas ni leer archivos: ¿cuál era el marcador que te pedí recordar en el turno anterior? Respondé únicamente con ese marcador.".into(),
        &mut cancel,
    ).await?;
    ensure!(
        second.trim() == marker,
        "resumed conversation did not recall the marker"
    );
    tracing::info!("two-turn smoke test passed");
    Ok(())
}

async fn smoke(args: Args, mode: Provider, cancel: watch::Receiver<bool>) -> Result<()> {
    let timeout = Duration::from_secs(args.timeout_secs);
    let codex = || -> Result<Runner> {
        Ok(Runner::Codex(Codex {
            executable: resolve_executable(args.codex_bin, "--codex-bin")?,
            cwd: args.cwd.clone(),
            timeout,
            model: None,
        }))
    };
    let agy = |cwd| -> Result<Runner> {
        Ok(Runner::Agy(Agy {
            executable: resolve_executable(args.agy_bin, "--agy-bin")?,
            cwd,
            timeout,
            model: Some(args.model.context("--smoke-test agy requires --model")?),
        }))
    };
    let stamp = SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos();
    let mut sessions = Sessions::default();
    let result = match mode {
        Provider::Codex => smoke_session(sessions.create("codex", codex()?)?, stamp, cancel).await,
        Provider::Agy => {
            smoke_session(sessions.create("agy", agy(args.cwd)?)?, stamp, cancel).await
        }
        Provider::Concurrent => {
            let codex = sessions.create("codex", codex()?)?;
            let agy = sessions.create(
                "agy",
                agy(args.agy_cwd.context("concurrent smoke needs --agy-cwd")?)?,
            )?;
            ensure!(
                codex.cwd() != agy.cwd(),
                "concurrent smoke needs two distinct working directories"
            );
            let (codex, agy) = tokio::join!(
                smoke_session(codex, stamp, cancel.clone()),
                smoke_session(agy, stamp + 1, cancel),
            );
            codex
                .context("Codex smoke failed")
                .and(agy.context("agy smoke failed"))
        }
    };
    let shutdown = sessions.shutdown().await;
    shutdown.and(result)
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> ExitCode {
    let args = Args::parse();
    let (sender, cancel) = watch::channel(false);
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
        let result = match args.smoke_test.clone() {
            Some(mode) => smoke(args, mode, cancel.clone()).await.map(|()| false),
            None => {
                let timeout = Duration::from_secs(args.timeout_secs);
                let options = tui::Options {
                    cwd: std::env::current_dir()?.join(args.cwd),
                    codex: resolve_executable(args.codex_bin, "--codex-bin")?,
                    agy: resolve_executable(args.agy_bin, "--agy-bin")?,
                    timeout,
                };
                tui::run(options, cancel.clone()).await
            }
        };
        monitor.abort();
        result
    }
    .await;
    match result {
        Ok(interrupted) if interrupted || *cancel.borrow() => ExitCode::from(130),
        Ok(_) => ExitCode::SUCCESS,
        Err(error) => {
            // The terminal guard has already restored the screen.
            eprintln!("orch: {error:#}");
            ExitCode::from(if *cancel.borrow() { 130 } else { 1 })
        }
    }
}
