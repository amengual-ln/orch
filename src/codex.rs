use std::{path::PathBuf, time::Duration};

use anyhow::Result;
use serde_json::Value;
use tokio::{process::Command, sync::watch};

use crate::{
    events::{AgentEvent, FailureKind},
    runner::{self, TurnParser},
};

pub use crate::runner::{TurnResult, validate_cwd};

const MAX_TEXT: usize = 4 * 1024 * 1024;

#[derive(Default)]
pub struct Parser {
    pub provider_session_id: Option<String>,
    turn_started: bool,
    completed: bool,
    messages: Vec<(String, String)>,
    failure: Option<(FailureKind, String)>,
}

impl Parser {
    /// Unknown events and invalid lines are ignored; they never imply success.
    pub fn line(&mut self, bytes: &[u8]) -> Vec<AgentEvent> {
        let value: Value = match serde_json::from_slice(bytes) {
            Ok(value) => value,
            Err(_) => {
                tracing::warn!(bytes = bytes.len(), "ignoring invalid provider JSON");
                return Vec::new();
            }
        };
        let mut events = Vec::new();
        let kind = value["type"].as_str().unwrap_or("");
        if self.completed
            && matches!(
                kind,
                "thread.started"
                    | "turn.started"
                    | "item.completed"
                    | "turn.completed"
                    | "turn.failed"
                    | "error"
            )
        {
            self.fail(
                FailureKind::Protocol,
                "provider emitted events after turn completion",
            );
            return events;
        }
        match kind {
            "thread.started" => {
                if let Some(id) = value["thread_id"].as_str().filter(|s| !s.is_empty()) {
                    if self.provider_session_id.is_some() {
                        self.fail(FailureKind::Protocol, "duplicate thread.started");
                    } else {
                        self.provider_session_id = Some(id.to_owned());
                        events.push(AgentEvent::SessionInitialized {
                            provider_session_id: id.to_owned(),
                        });
                    }
                } else {
                    self.fail(FailureKind::Protocol, "thread.started has no thread_id");
                }
            }
            "turn.started" => {
                if self.turn_started || self.provider_session_id.is_none() {
                    self.fail(FailureKind::Protocol, "unexpected turn.started");
                }
                self.turn_started = true;
            }
            "item.completed" if value["item"]["type"] == "agent_message" => {
                if !self.turn_started {
                    self.fail(FailureKind::Protocol, "message before turn.started");
                }
                match (value["item"]["id"].as_str(), value["item"]["text"].as_str()) {
                    (Some(id), Some(text)) => {
                        let previous = self.messages.iter().position(|(key, _)| key == id);
                        let other_bytes: usize = self
                            .messages
                            .iter()
                            .filter(|(key, _)| key != id)
                            .map(|(key, text)| key.len() + text.len())
                            .sum();
                        if other_bytes
                            .saturating_add(id.len())
                            .saturating_add(text.len())
                            > MAX_TEXT
                            || self.messages.len() >= 4096
                        {
                            self.fail(FailureKind::Protocol, "turn response exceeded memory limit");
                        } else if !previous.is_some_and(|i| self.messages[i].1 == text) {
                            match previous {
                                Some(i) => self.messages[i].1 = text.to_owned(),
                                None => self.messages.push((id.to_owned(), text.to_owned())),
                            }
                            events.push(AgentEvent::MessageCompleted {
                                message_id: id.to_owned(),
                                text: text.to_owned(),
                            });
                        }
                    }
                    _ => self.fail(FailureKind::Protocol, "agent_message has no id/text"),
                }
            }
            "turn.completed" => self.completed = true,
            "turn.failed" | "error" => {
                let message = value["error"]["message"]
                    .as_str()
                    .or_else(|| value["message"].as_str())
                    .unwrap_or("provider reported an error");
                self.fail(FailureKind::Provider, message);
            }
            _ => tracing::debug!("ignoring provider event"),
        }
        events
    }

    fn fail(&mut self, kind: FailureKind, message: &str) {
        if self.failure.is_none() {
            self.failure = Some((kind, message.to_owned()));
        }
    }

    pub fn finish(
        &self,
        expected_id: Option<&str>,
    ) -> std::result::Result<TurnResult, (FailureKind, String)> {
        if let Some(failure) = &self.failure {
            return Err(failure.clone());
        }
        if !self.completed || !self.turn_started || self.messages.is_empty() {
            return Err((
                FailureKind::Protocol,
                "provider stream ended without a complete turn and agent message".into(),
            ));
        }
        let id = self.provider_session_id.as_deref().ok_or_else(|| {
            (
                FailureKind::Protocol,
                "provider did not identify the conversation".into(),
            )
        })?;
        if expected_id.is_some_and(|expected| expected != id) {
            return Err((
                FailureKind::Protocol,
                "resume returned a different conversation id".into(),
            ));
        }
        Ok(TurnResult {
            provider_session_id: id.to_owned(),
            final_text: self
                .messages
                .iter()
                .map(|(_, text)| text.as_str())
                .collect::<Vec<_>>()
                .join("\n\n"),
        })
    }
}

impl TurnParser for Parser {
    fn line(&mut self, bytes: &[u8]) -> Vec<AgentEvent> {
        Parser::line(self, bytes)
    }

    fn finish(self, expected_id: Option<&str>) -> Result<TurnResult, runner::TurnFailure> {
        Parser::finish(&self, expected_id)
    }
}

pub struct Codex {
    pub executable: PathBuf,
    pub cwd: PathBuf,
    pub timeout: Duration,
    pub model: Option<String>,
}

impl Codex {
    pub fn command(&self, prompt: &str, resume: Option<&str>) -> Command {
        let mut command = Command::new(&self.executable);
        command
            .current_dir(&self.cwd)
            .args(["-a", "never", "--sandbox", "read-only", "exec"]);
        if resume.is_some() {
            command.arg("resume");
        }
        command.args(["--ignore-user-config", "--json", "--skip-git-repo-check"]);
        if let Some(model) = &self.model {
            command.arg(format!("--model={model}"));
        }
        if let Some(id) = resume {
            command.arg(id);
        }
        command.arg("--").arg(prompt);
        command
    }

    pub async fn turn(
        &self,
        prompt: &str,
        resume: Option<&str>,
        cancel: &mut watch::Receiver<bool>,
        emit: impl FnMut(AgentEvent) -> Result<()>,
    ) -> Result<TurnResult> {
        runner::turn(
            &self.cwd,
            self.timeout,
            cancel,
            |cwd| {
                let mut command = self.command(prompt, resume);
                command.current_dir(cwd);
                (command, Parser::default())
            },
            resume,
            emit,
        )
        .await
    }
}
