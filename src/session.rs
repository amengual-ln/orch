//! In-memory sessions. Each controller owns one runner and at most one active turn.
use std::{
    fmt,
    hash::{DefaultHasher, Hash, Hasher},
    path::{Path, PathBuf},
    time::Duration,
};

use anyhow::{Result, anyhow, bail, ensure};
use tokio::{
    sync::{mpsc, oneshot, watch},
    task::JoinHandle,
    time::{Instant, timeout_at},
};

use crate::{
    agy::Agy,
    codex::Codex,
    events::{AgentEvent, FailureKind},
    runner::{TurnResult, validate_cwd},
};

const SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(5);
const MAX_PROMPT_BYTES: usize = 64 * 1024;
const MAX_HISTORY_BYTES: usize = 32 * 1024 * 1024;
const MAX_TURN_EVENTS_BYTES: usize = 8 * 1024 * 1024;
// The runners' largest terminal payload is Codex's 4 MiB result plus separators.
const TERMINAL_RESERVE: usize = 5 * 1024 * 1024;

pub enum Runner {
    Codex(Codex),
    Agy(Agy),
}

impl Runner {
    fn validate(&mut self) -> Result<()> {
        let (cwd, model) = match self {
            Self::Codex(runner) => (&mut runner.cwd, &runner.model),
            Self::Agy(runner) => (&mut runner.cwd, &runner.model),
        };
        ensure!(
            model.as_ref().is_none_or(|model| !model.trim().is_empty()),
            "session model must be nonempty when specified"
        );
        *cwd = validate_cwd(cwd)?;
        Ok(())
    }

    fn cwd(&self) -> &Path {
        match self {
            Self::Codex(runner) => &runner.cwd,
            Self::Agy(runner) => &runner.cwd,
        }
    }

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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Status {
    Idle,
    Running,
    Cancelling,
    Error,
    Closed,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CommandError {
    Busy,
    Closed,
    QueueFull,
    PromptTooLong,
    HistoryFull,
}

impl fmt::Display for CommandError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Busy => "session is busy",
            Self::Closed => "session is closed",
            Self::QueueFull => "session command queue is full",
            Self::PromptTooLong => "prompt exceeds the session size limit",
            Self::HistoryFull => "session history is full",
        })
    }
}

impl std::error::Error for CommandError {}

#[derive(Debug, Clone)]
pub struct Turn {
    pub id: u64,
    pub prompt: String,
    pub events: Vec<AgentEvent>,
}

#[derive(Debug)]
pub struct Snapshot {
    pub status: Status,
    pub provider_session_id: Option<String>,
    pub history: Vec<Turn>,
    bytes: usize,
}

enum Command {
    SendTurn {
        prompt: String,
        reply: oneshot::Sender<Result<u64, CommandError>>,
    },
    Cancel(oneshot::Sender<Result<(), CommandError>>),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Provider {
    Codex { model: Option<String> },
    Agy { model: Option<String> },
}

#[derive(Clone)]
pub struct Session {
    id: String,
    alias: String,
    cwd: PathBuf,
    provider: Provider,
    commands: mpsc::Sender<Command>,
    close: watch::Sender<bool>,
    state: watch::Receiver<Snapshot>,
}

impl Session {
    pub fn id(&self) -> &str {
        &self.id
    }

    pub fn alias(&self) -> &str {
        &self.alias
    }

    pub fn cwd(&self) -> &Path {
        &self.cwd
    }

    pub fn provider(&self) -> &Provider {
        &self.provider
    }

    /// Read state synchronously. The callback must finish before the controller can write.
    pub fn inspect<T>(&self, read: impl FnOnce(&Snapshot) -> T) -> T {
        read(&self.state.borrow())
    }

    /// Coalesce updates for a polling view while keeping the complete history.
    pub fn take_update(&mut self) -> bool {
        self.state.borrow_and_update().has_changed()
    }

    pub async fn changed(&mut self) -> Result<(), CommandError> {
        self.state.changed().await.map_err(|_| CommandError::Closed)
    }

    pub async fn send_turn(&self, prompt: String) -> Result<u64, CommandError> {
        if prompt.len() > MAX_PROMPT_BYTES {
            return Err(CommandError::PromptTooLong);
        }
        match self.inspect(|state| state.status) {
            Status::Running | Status::Cancelling => return Err(CommandError::Busy),
            Status::Closed => return Err(CommandError::Closed),
            Status::Idle | Status::Error => {}
        }
        let (reply, response) = oneshot::channel();
        self.commands
            .try_send(Command::SendTurn { prompt, reply })
            .map_err(|err| match err {
                mpsc::error::TrySendError::Full(_) => CommandError::QueueFull,
                mpsc::error::TrySendError::Closed(_) => CommandError::Closed,
            })?;
        response.await.map_err(|_| CommandError::Closed)?
    }

    pub async fn cancel(&self) -> Result<(), CommandError> {
        let (reply, response) = oneshot::channel();
        self.commands
            .send(Command::Cancel(reply))
            .await
            .map_err(|_| CommandError::Closed)?;
        response.await.map_err(|_| CommandError::Closed)?
    }

    /// Ask the controller to stop accepting work and close after cleaning up its process.
    pub fn close(&self) {
        self.close.send_replace(true);
    }
}

#[derive(Default)]
pub struct Sessions {
    entries: Vec<(Session, JoinHandle<()>)>,
}

impl Sessions {
    pub fn create(&mut self, alias: &str, mut runner: Runner) -> Result<Session> {
        runner.validate()?;
        let provider = match &runner {
            Runner::Codex(runner) => Provider::Codex {
                model: runner.model.clone(),
            },
            Runner::Agy(runner) => Provider::Agy {
                model: runner.model.clone(),
            },
        };
        let alias = alias.trim();
        let alias = if alias.is_empty() {
            self.make_alias(runner.cwd(), &provider)
        } else {
            ensure!(
                self.alias_available(alias),
                "session alias already exists: {alias}"
            );
            alias.to_owned()
        };
        let runtime = tokio::runtime::Handle::try_current()?;
        let (commands, inbox) = mpsc::channel(8);
        let (close, close_rx) = watch::channel(false);
        let (state_tx, state) = watch::channel(Snapshot {
            status: Status::Idle,
            provider_session_id: None,
            history: Vec::new(),
            bytes: 0,
        });
        let session = Session {
            id: format!("session-{}", self.entries.len() + 1),
            alias,
            cwd: runner.cwd().to_path_buf(),
            provider,
            commands,
            close,
            state,
        };
        let task = runtime.spawn(control(runner, inbox, close_rx, ControllerState(state_tx)));
        self.entries.push((session.clone(), task));
        Ok(session)
    }

    fn alias_available(&self, alias: &str) -> bool {
        self.entries
            .iter()
            .all(|(session, _)| session.alias() != alias)
    }

    fn make_alias(&self, cwd: &Path, provider: &Provider) -> String {
        let project = cwd
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("proyecto");
        let project: String = project
            .chars()
            .map(|ch| {
                if ch.is_alphanumeric() || matches!(ch, '-' | '_') {
                    ch
                } else {
                    '-'
                }
            })
            .collect();
        let (name, model) = match provider {
            Provider::Codex { model } => ("codex", model),
            Provider::Agy { model } => ("agy", model),
        };
        let mut salt = self.entries.len();
        loop {
            let mut hash = DefaultHasher::new();
            (cwd, name, model, salt).hash(&mut hash);
            let alias = format!("{project}-{name}-{:08x}", hash.finish() as u32);
            if self.alias_available(&alias) {
                return alias;
            }
            salt += 1;
        }
    }

    /// Signal all controllers at once, then wait under a single deadline.
    pub async fn shutdown(self) -> Result<()> {
        self.close_all();
        let deadline = Instant::now() + SHUTDOWN_TIMEOUT;
        let mut failure = None;
        let mut sessions = self;
        for (session, task) in &mut sessions.entries {
            match timeout_at(deadline, &mut *task).await {
                Ok(Ok(())) => {}
                Ok(Err(error)) => {
                    failure = Some(anyhow!(
                        "session {} failed to shut down: {error}",
                        session.alias()
                    ));
                }
                Err(_) => {
                    task.abort();
                    failure = Some(anyhow!("session shutdown exceeded its deadline"));
                }
            }
        }
        failure.map_or(Ok(()), Err)
    }

    fn close_all(&self) {
        for (session, _) in &self.entries {
            session.close();
        }
    }
}

impl Drop for Sessions {
    fn drop(&mut self) {
        self.close_all();
    }
}

// Also closes the snapshot if a controller is aborted at the global deadline.
struct ControllerState(watch::Sender<Snapshot>);

impl Drop for ControllerState {
    fn drop(&mut self) {
        self.0.send_modify(|snapshot| {
            if matches!(snapshot.status, Status::Running | Status::Cancelling)
                && let Some(turn) = snapshot.history.last_mut()
            {
                turn.events.push(AgentEvent::TurnFailed {
                    kind: FailureKind::Process,
                    message: "session controller stopped before the turn finished".into(),
                });
            }
            snapshot.status = Status::Closed;
        });
    }
}

async fn control(
    runner: Runner,
    mut inbox: mpsc::Receiver<Command>,
    mut closing: watch::Receiver<bool>,
    controller: ControllerState,
) {
    let state = &controller.0;
    loop {
        let command = tokio::select! {
            biased;
            _ = closing.wait_for(|value| *value) => break,
            command = inbox.recv() => command,
        };
        let Some(command) = command else { break };
        let (prompt, reply) = match command {
            Command::SendTurn { prompt, reply } => (prompt, reply),
            Command::Cancel(reply) => {
                let _ = reply.send(Ok(()));
                continue;
            }
        };
        if reply.is_closed() {
            continue;
        }
        let (turn_id, expected_id) = {
            let snapshot = state.borrow();
            if snapshot.bytes
                + prompt.len()
                + size_of::<Turn>()
                + MAX_TURN_EVENTS_BYTES
                + TERMINAL_RESERVE
                > MAX_HISTORY_BYTES
            {
                let _ = reply.send(Err(CommandError::HistoryFull));
                continue;
            }
            (
                snapshot.history.len() as u64 + 1,
                snapshot.provider_session_id.clone(),
            )
        };
        state.send_modify(|snapshot| {
            snapshot.bytes += prompt.len() + size_of::<Turn>();
            snapshot.status = Status::Running;
            snapshot.history.push(Turn {
                id: turn_id,
                prompt: prompt.clone(),
                events: Vec::new(),
            });
        });
        let _ = reply.send(Ok(turn_id));

        let (cancel, mut cancel_rx) = watch::channel(false);
        let mut turn_bytes = 0;
        let turn = runner.turn(&prompt, expected_id.as_deref(), &mut cancel_rx, |event| {
            record(state, &mut turn_bytes, event)
        });
        tokio::pin!(turn);
        let mut close_requested = false;
        loop {
            tokio::select! {
                biased;
                _ = &mut turn => break,
                _ = closing.wait_for(|value| *value), if !close_requested => {
                    close_requested = true;
                    cancel_turn(state, &cancel);
                }
                command = inbox.recv(), if !close_requested => match command {
                    Some(Command::SendTurn { reply, .. }) => {
                        let _ = reply.send(Err(CommandError::Busy));
                    }
                    Some(Command::Cancel(reply)) => {
                        cancel_turn(state, &cancel);
                        let _ = reply.send(Ok(()));
                    }
                    None => {
                        close_requested = true;
                        cancel_turn(state, &cancel);
                    }
                },
            }
        }
        if close_requested {
            break;
        }
    }
}

fn cancel_turn(state: &watch::Sender<Snapshot>, cancel: &watch::Sender<bool>) {
    state.send_modify(|snapshot| snapshot.status = Status::Cancelling);
    cancel.send_replace(true);
}

fn record(
    state: &watch::Sender<Snapshot>,
    turn_bytes: &mut usize,
    event: AgentEvent,
) -> Result<()> {
    let (text_bytes, next) = match &event {
        AgentEvent::TurnCompleted { final_text } => (final_text.len(), Some(Status::Idle)),
        AgentEvent::TurnFailed { message, .. } => (message.len(), Some(Status::Error)),
        AgentEvent::TurnCancelled => (0, Some(Status::Idle)),
        AgentEvent::TextDelta { message_id, text }
        | AgentEvent::MessageCompleted { message_id, text } => {
            (message_id.len() + text.len(), None)
        }
        AgentEvent::SessionInitialized {
            provider_session_id,
        } => (provider_session_id.len(), None),
        AgentEvent::TurnStarted => (0, None),
    };
    let bytes = text_bytes + size_of::<AgentEvent>();
    if next.is_none() {
        *turn_bytes += bytes;
        ensure!(
            *turn_bytes <= MAX_TURN_EVENTS_BYTES,
            "turn event history exceeded memory limit"
        );
    }
    let recorded = state.send_if_modified(|snapshot| {
        let Some(turn) = snapshot.history.last_mut() else {
            return false;
        };
        if let AgentEvent::SessionInitialized {
            provider_session_id,
        } = &event
        {
            // A failed resume must not replace the known conversation.
            if snapshot.provider_session_id.is_none() {
                snapshot.provider_session_id = Some(provider_session_id.clone());
            }
        }
        if let Some(status) = next {
            snapshot.status = status;
        }
        snapshot.bytes += bytes;
        turn.events.push(event);
        true
    });
    if !recorded {
        bail!("turn is missing from session history");
    }
    Ok(())
}
