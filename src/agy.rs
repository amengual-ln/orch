use std::{
    path::{Path, PathBuf},
    time::Duration,
};

use anyhow::Result;
use serde_json::Value;
use tokio::{process::Command, sync::watch};

use crate::{
    events::{AgentEvent, FailureKind},
    runner::{self, TurnFailure, TurnParser, TurnResult},
};

const MAX_DELTA_BYTES: usize = 4 * 1024 * 1024;

pub struct Agy {
    pub executable: PathBuf,
    pub cwd: PathBuf,
    pub timeout: Duration,
    pub model: String,
}

impl Agy {
    pub fn command(&self, prompt: &str, resume: Option<&str>) -> Command {
        let mut command = Command::new(&self.executable);
        command
            .current_dir(&self.cwd)
            .arg(format!("--model={}", self.model))
            .args([
                "--mode",
                "plan",
                "--sandbox",
                "--settings",
                r#"{"toolPermission":"strict"}"#,
                "--output-format",
                "stream-json",
            ]);
        if let Some(id) = resume {
            command.arg(format!("--conversation={id}"));
        }
        command.arg(format!("--print={prompt}"));
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
                command.current_dir(&cwd);
                (command, Parser::new(&self.model, cwd, resume))
            },
            resume,
            emit,
        )
        .await
    }
}

struct Parser<'a> {
    model: &'a str,
    cwd: PathBuf,
    resume: Option<&'a str>,
    id: Option<String>,
    delta_bytes: usize,
    result: Option<String>,
    failure: Option<TurnFailure>,
}

impl<'a> Parser<'a> {
    fn new(model: &'a str, cwd: PathBuf, resume: Option<&'a str>) -> Self {
        Self {
            model,
            cwd,
            resume,
            id: None,
            delta_bytes: 0,
            result: None,
            failure: None,
        }
    }

    fn parse(&mut self, value: &Value) -> Result<Option<AgentEvent>, TurnFailure> {
        let protocol = |message: &str| (FailureKind::Protocol, message.to_owned());
        let Some(kind @ ("init" | "step_update" | "result")) = value["event"].as_str() else {
            return Ok(None);
        };
        if self.result.is_some() {
            return Err(protocol("agy emitted events after result"));
        }
        if kind == "init" {
            let id = value["conversation_id"]
                .as_str()
                .filter(|id| !id.is_empty());
            let Some(id) = id.filter(|id| {
                self.id.is_none() && self.resume.is_none_or(|expected| *id == expected)
            }) else {
                return Err(protocol(
                    "agy init has a missing, duplicate or unexpected conversation id",
                ));
            };
            let init = &value["init"];
            let cwd = init["cwd"].as_str().map(Path::new);
            if init["model"] != self.model || cwd != Some(self.cwd.as_path()) {
                return Err(protocol("agy init reported an unexpected model or cwd"));
            }
            if init["permission_mode"] != "strict" {
                return Err((
                    FailureKind::Permission,
                    "agy init did not confirm strict permissions".into(),
                ));
            }
            self.id = Some(id.to_owned());
            return Ok(Some(AgentEvent::SessionInitialized {
                provider_session_id: id.to_owned(),
            }));
        }
        let payload = &value[kind];
        if kind == "result" {
            if !matches!(&payload["denied_actions"], Value::Null | Value::Array(_)) {
                return Err(protocol("agy result has invalid denied_actions"));
            }
            if let Some(actions) = payload["denied_actions"]
                .as_array()
                .filter(|actions| !actions.is_empty())
            {
                let mut message = format!("agy denied {} tool action(s)", actions.len());
                if self.delta_bytes == 0
                    && let Some(text) = payload["response"].as_str().filter(|text| !text.is_empty())
                {
                    message.push('\n');
                    message.push_str(text);
                }
                return Err((FailureKind::Permission, message));
            }
            match payload["status"].as_str() {
                Some("SUCCESS") => {}
                Some(status) => {
                    let message = payload["error"].as_str().unwrap_or(status);
                    return Err((FailureKind::Provider, message.to_owned()));
                }
                None => return Err(protocol("agy result has no status")),
            }
        }
        let id = payload["conversation_id"].as_str();
        if id.is_none() || id != self.id.as_deref() {
            return Err(protocol("agy event has an unexpected conversation id"));
        }
        if kind == "result" {
            let text = payload["response"]
                .as_str()
                .ok_or_else(|| protocol("agy result has no response"))?;
            self.result = Some(text.to_owned());
            if self.delta_bytes == 0 && !text.is_empty() {
                return Ok(Some(AgentEvent::MessageCompleted {
                    message_id: "result".into(),
                    text: text.to_owned(),
                }));
            }
        } else if payload["step_type"] == "agent_response"
            && let Some(text) = payload["text_delta"]
                .as_str()
                .filter(|text| !text.is_empty())
        {
            let step = payload["step_index"]
                .as_u64()
                .ok_or_else(|| protocol("agy text delta has no step index"))?;
            self.delta_bytes += text.len();
            if self.delta_bytes > MAX_DELTA_BYTES {
                return Err(protocol("agy text deltas exceeded memory limit"));
            }
            return Ok(Some(AgentEvent::TextDelta {
                message_id: step.to_string(),
                text: text.to_owned(),
            }));
        }
        Ok(None)
    }
}

impl TurnParser for Parser<'_> {
    const REPORT_FAILURE_ON_NONZERO: bool = true;

    fn line(&mut self, bytes: &[u8]) -> Vec<AgentEvent> {
        if self.failure.is_some() {
            return Vec::new();
        }
        let Ok(value) = serde_json::from_slice(bytes) else {
            tracing::warn!(bytes = bytes.len(), "ignoring invalid provider JSON");
            return Vec::new();
        };
        match self.parse(&value) {
            Ok(event) => event.into_iter().collect(),
            Err(failure) => {
                self.failure = Some(failure);
                Vec::new()
            }
        }
    }

    fn finish(self, _: Option<&str>) -> Result<TurnResult, TurnFailure> {
        if let Some(failure) = self.failure {
            return Err(failure);
        }
        match (self.id, self.result) {
            (Some(provider_session_id), Some(final_text)) => Ok(TurnResult {
                provider_session_id,
                final_text,
            }),
            _ => Err((
                FailureKind::Protocol,
                "agy stream ended without a complete turn".into(),
            )),
        }
    }
}
