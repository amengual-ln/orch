//! Linux process ownership, independent of the provider protocol.
use std::{io, process::ExitStatus, time::Duration};

use anyhow::{Context, Result};
use nix::{
    errno::Errno,
    sys::signal::{Signal, killpg},
    unistd::Pid,
};
use tokio::{
    io::{AsyncRead, AsyncReadExt},
    process::Command,
    sync::watch,
    time::{sleep, timeout},
};

const MAX_LINE: usize = 1024 * 1024;
const STDERR_LIMIT: usize = 16 * 1024;
const GRACE: Duration = Duration::from_millis(300);

#[derive(Debug)]
pub enum Stop {
    Exited,
    Cancelled,
    Timeout,
    Io(String),
}

#[derive(Debug)]
pub struct Output {
    pub status: ExitStatus,
    pub stop: Stop,
    pub stderr: String,
}

// The group remains owned until cleanup, even when the leader has exited.
struct Group(Option<Pid>);
impl Group {
    fn signal(&self, signal: Signal) -> io::Result<()> {
        let Some(pid) = self.0 else {
            return Ok(());
        };
        match killpg(pid, signal) {
            Ok(()) | Err(Errno::ESRCH) => Ok(()),
            Err(e) => Err(io::Error::from_raw_os_error(e as i32)),
        }
    }
}
impl Drop for Group {
    fn drop(&mut self) {
        // Also covers unwinding or dropping the runner future.
        let _ = self.signal(Signal::SIGKILL);
    }
}

#[derive(Default)]
struct Lines {
    bytes: Vec<u8>,
    discarding: bool,
}
impl Lines {
    fn feed(&mut self, bytes: &[u8], line: &mut impl FnMut(&[u8]) -> Result<()>) -> Result<()> {
        for chunk in bytes.split_inclusive(|b| *b == b'\n') {
            if !self.discarding {
                if self.bytes.len() + chunk.len() > MAX_LINE {
                    self.bytes.clear();
                    self.discarding = true;
                    tracing::warn!(limit = MAX_LINE, "discarding oversized stdout line");
                } else {
                    self.bytes.extend_from_slice(chunk);
                }
            }
            if chunk.last() == Some(&b'\n') {
                self.end(line)?;
            }
        }
        Ok(())
    }
    fn end(&mut self, line: &mut impl FnMut(&[u8]) -> Result<()>) -> Result<()> {
        if !self.discarding && !self.bytes.is_empty() {
            line(&self.bytes)?;
        }
        self.bytes.clear();
        self.discarding = false;
        Ok(())
    }
}

fn tail(buffer: &mut Vec<u8>, data: &[u8]) {
    if buffer.len() + data.len() > STDERR_LIMIT {
        let remove = (buffer.len() + data.len() - STDERR_LIMIT).min(buffer.len());
        buffer.drain(..remove);
    }
    buffer.extend_from_slice(&data[data.len().saturating_sub(STDERR_LIMIT)..]);
}

async fn cancelled(cancel: &mut watch::Receiver<bool>) {
    loop {
        if *cancel.borrow_and_update() {
            return;
        }
        if cancel.changed().await.is_err() {
            return;
        }
    }
}

async fn drain(mut pipe: impl AsyncRead + Unpin) -> Vec<u8> {
    let mut bytes = [0; 8192];
    let mut last = Vec::new();
    while let Ok(n) = pipe.read(&mut bytes).await {
        if n == 0 {
            break;
        }
        tail(&mut last, &bytes[..n]);
    }
    last
}

/// A completed process is not a completed provider turn; the caller checks the protocol.
/// `on_line` must be quick (parsing/dispatch); no provider output is logged verbatim.
pub async fn run(
    command: &mut Command,
    duration: Duration,
    cancel: &mut watch::Receiver<bool>,
    mut on_line: impl FnMut(&[u8]) -> Result<()>,
) -> Result<Output> {
    command
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .process_group(0)
        .kill_on_drop(true);
    let mut child = command.spawn().context("could not launch executable")?;
    let pid = child.id().context("child has no process id")?;
    let mut group = Group(Some(Pid::from_raw(i32::try_from(pid)?)));
    let mut stdout = child.stdout.take().context("missing stdout pipe")?;
    let mut stderr = child.stderr.take().context("missing stderr pipe")?;
    let mut out_buf = [0; 8192];
    let mut err_buf = [0; 8192];
    let mut lines = Lines::default();
    let mut diagnostic = Vec::new();
    let (mut out_eof, mut err_eof) = (false, false);
    let mut status = None;
    let deadline = sleep(duration);
    tokio::pin!(deadline);
    let stop = loop {
        if status.is_some() && out_eof && err_eof {
            break Stop::Exited;
        }
        tokio::select! {
            _ = cancelled(cancel) => break Stop::Cancelled,
            _ = &mut deadline => break Stop::Timeout,
            read = stdout.read(&mut out_buf), if !out_eof => match read {
                Ok(0) => {
                    out_eof = true;
                    if let Err(e) = lines.end(&mut on_line) { break Stop::Io(e.to_string()); }
                }
                Ok(n) => if let Err(e) = lines.feed(&out_buf[..n], &mut on_line) {
                    break Stop::Io(e.to_string());
                },
                Err(e) => break Stop::Io(e.to_string()),
            },
            read = stderr.read(&mut err_buf), if !err_eof => match read {
                Ok(0) => err_eof = true,
                Ok(n) => tail(&mut diagnostic, &err_buf[..n]),
                Err(e) => break Stop::Io(e.to_string()),
            },
            exit = child.wait(), if status.is_none() => match exit {
                Ok(exit) => status = Some(exit),
                Err(e) => break Stop::Io(e.to_string()),
            },
        }
    };

    if !matches!(stop, Stop::Exited) {
        tracing::info!(pid, "interrupting process group");
        group
            .signal(Signal::SIGINT)
            .context("could not interrupt process group")?;
        // Drain while the group gets its grace period, avoiding full-pipe deadlocks.
        let ((), (), bytes) = tokio::join!(
            sleep(GRACE),
            async {
                let _ = timeout(GRACE, drain(&mut stdout)).await;
            },
            async { timeout(GRACE, drain(&mut stderr)).await.unwrap_or_default() }
        );
        tail(&mut diagnostic, &bytes);
    }
    // Normal exit can also leave descendants holding no pipes. Kill that group too.
    group
        .signal(Signal::SIGKILL)
        .context("could not terminate process group")?;
    group.0 = None;
    let status = match status {
        Some(status) => status,
        None => timeout(Duration::from_secs(3), child.wait())
            .await
            .context("child did not exit after SIGKILL")??,
    };
    // Bound draining: a process that escaped the group might still hold the pipes.
    let bytes = timeout(Duration::from_millis(300), drain(&mut stderr))
        .await
        .unwrap_or_default();
    tail(&mut diagnostic, &bytes);
    tracing::info!(pid, code = status.code(), "process reaped");
    Ok(Output {
        status,
        stop,
        stderr: String::from_utf8_lossy(&diagnostic).into_owned(),
    })
}
