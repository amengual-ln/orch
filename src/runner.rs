use std::{
    path::{Path, PathBuf},
    time::Duration,
};

use anyhow::{Context, Result, bail};
use tokio::{process::Command, sync::watch};

use crate::{
    events::{AgentEvent, FailureKind},
    process::{self, Stop},
};

#[derive(Debug)]
pub struct TurnResult {
    pub provider_session_id: String,
    pub final_text: String,
}

pub(crate) type TurnFailure = (FailureKind, String);

pub(crate) trait TurnParser {
    const REPORT_FAILURE_ON_NONZERO: bool = false;

    fn line(&mut self, bytes: &[u8]) -> Vec<AgentEvent>;
    fn finish(self, expected_id: Option<&str>) -> std::result::Result<TurnResult, TurnFailure>;
}

pub fn validate_cwd(path: &Path) -> Result<PathBuf> {
    let cwd = path
        .canonicalize()
        .with_context(|| format!("invalid cwd: {}", path.display()))?;
    anyhow::ensure!(cwd.is_dir(), "cwd is not a directory: {}", cwd.display());
    std::fs::read_dir(&cwd).with_context(|| format!("cwd is not accessible: {}", cwd.display()))?;
    Ok(cwd)
}

pub(crate) async fn turn<P: TurnParser>(
    cwd: &Path,
    timeout: Duration,
    cancel: &mut watch::Receiver<bool>,
    prepare: impl FnOnce(PathBuf) -> (Command, P),
    resume: Option<&str>,
    mut emit: impl FnMut(AgentEvent) -> Result<()>,
) -> Result<TurnResult> {
    emit(AgentEvent::TurnStarted)?;
    let outcome = async {
        let cwd = validate_cwd(cwd).map_err(|e| (FailureKind::Configuration, e.to_string()))?;
        let (mut command, mut parser) = prepare(cwd);
        let executable = command.as_std().get_program().to_os_string();
        process::run(&mut command, timeout, cancel, |line| {
            for event in parser.line(line) {
                emit(event)?;
            }
            Ok(())
        })
        .await
        .map(|output| (output, parser))
        .map_err(|e| {
            (
                FailureKind::Process,
                format!("{}: {e:#}", Path::new(&executable).display()),
            )
        })
    }
    .await;
    let terminal = match outcome {
        Err(failure) => Err(failure),
        Ok((output, parser)) => match output.stop {
            Stop::Cancelled => {
                emit(AgentEvent::TurnCancelled)?;
                bail!("turn cancelled");
            }
            stop => {
                let failure = match stop {
                    Stop::Timeout => Some((FailureKind::Timeout, "turn timed out".into())),
                    Stop::Io(message) => Some((FailureKind::Io, message)),
                    _ => None,
                };
                let result = match failure {
                    Some(failure) => Err(failure),
                    None => match parser.finish(resume) {
                        error @ Err((FailureKind::Provider | FailureKind::Permission, _))
                            if P::REPORT_FAILURE_ON_NONZERO =>
                        {
                            error
                        }
                        _ if !output.status.success() => Err((
                            FailureKind::Process,
                            format!("provider exited with {}", output.status),
                        )),
                        result => result,
                    },
                };
                result.map_err(|(kind, mut message)| {
                    if !output.stderr.is_empty() {
                        message.push_str("; stderr (last 16 KiB): ");
                        message.push_str(&output.stderr);
                    }
                    (kind, message)
                })
            }
        },
    };
    match terminal {
        Ok(result) => {
            emit(AgentEvent::TurnCompleted {
                final_text: result.final_text.clone(),
            })?;
            Ok(result)
        }
        Err((kind, message)) => {
            tracing::warn!(failure_kind = ?kind, "turn failed");
            emit(AgentEvent::TurnFailed {
                kind,
                message: message.clone(),
            })?;
            bail!(message)
        }
    }
}
