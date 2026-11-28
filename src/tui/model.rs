use std::{
    env, fs,
    io::{Read, Seek, SeekFrom},
    path::{Path, PathBuf},
};

use serde_json::Value;
use tokio::{
    sync::watch,
    task::{self, JoinHandle},
};

use orch::{
    agy::Agy,
    events::AgentEvent,
    session::{Provider, Session},
};

use super::Options;

const MAX_TAIL: u64 = 512 * 1024;

#[derive(Default)]
pub(super) struct Model {
    rollout: Option<PathBuf>,
    label: Option<String>,
    checked_turn: u64,
    lookup: Option<JoinHandle<Option<Report>>>,
}

struct Report {
    rollout: Option<PathBuf>,
    label: String,
}

impl Model {
    pub(super) fn reported(&self) -> Option<&str> {
        self.label.as_deref()
    }

    pub(super) async fn refresh(
        &mut self,
        session: &Session,
        options: &Options,
        cancel: &watch::Receiver<bool>,
    ) -> bool {
        let updated = if self.lookup.as_ref().is_some_and(JoinHandle::is_finished)
            && let Some(lookup) = self.lookup.take()
            && let Ok(Some(report)) = lookup.await
        {
            self.rollout = report.rollout;
            self.label = Some(report.label);
            true
        } else {
            false
        };
        if self.lookup.is_some() {
            return updated;
        }
        let next = session.inspect(|snapshot| {
            let turn = snapshot.history.last()?;
            if turn.id <= self.checked_turn
                || !matches!(
                    turn.events.last(),
                    Some(AgentEvent::TurnCompleted { .. } | AgentEvent::TurnFailed { .. })
                )
            {
                return None;
            }
            Some((turn.id, snapshot.provider_session_id.clone()?))
        });
        if let Some((turn, id)) = next {
            self.checked_turn = turn;
            match session.provider() {
                Provider::Agy { model: None } if self.label.is_none() => {
                    let runner = Agy {
                        executable: options.agy.clone(),
                        cwd: session.cwd().to_path_buf(),
                        timeout: options.timeout,
                        model: None,
                    };
                    let mut cancel = cancel.clone();
                    self.lookup = Some(task::spawn(async move {
                        let label = runner.reported_model(&id, &mut cancel).await?;
                        Some(Report {
                            rollout: None,
                            label,
                        })
                    }));
                }
                Provider::Agy { .. } => {}
                Provider::Codex { .. } => {
                    let rollout = self.rollout.clone();
                    self.lookup = Some(task::spawn_blocking(move || {
                        let path = rollout.or_else(|| find_rollout(&id))?;
                        read_model(&path).map(|label| Report {
                            rollout: Some(path),
                            label,
                        })
                    }));
                }
            }
        }
        updated
    }

    pub(super) async fn shutdown(&mut self, provider: &Provider) {
        if let Some(task) = self.lookup.take() {
            if matches!(provider, Provider::Agy { .. }) {
                let _ = task.await;
            } else {
                task.abort();
            }
        }
    }
}

fn find_rollout(id: &str) -> Option<PathBuf> {
    // The filename is provider-local. Only accept a UUID-shaped identifier from this session.
    if id.len() != 36
        || !id.bytes().enumerate().all(|(i, b)| {
            if matches!(i, 8 | 13 | 18 | 23) {
                b == b'-'
            } else {
                b.is_ascii_hexdigit()
            }
        })
    {
        return None;
    }
    let home = env::var_os("CODEX_HOME")
        .map(PathBuf::from)
        .or_else(|| env::var_os("HOME").map(|home| PathBuf::from(home).join(".codex")))?;
    let suffix = format!("-{id}.jsonl");
    // A session starts in this Orch instance. Keep the matched path for later turns.
    for year in recent_dirs(&home.join("sessions"), 2) {
        for month in recent_dirs(&year, 2) {
            for day in recent_dirs(&month, 3) {
                if let Some(entry) =
                    fs::read_dir(day)
                        .ok()
                        .into_iter()
                        .flatten()
                        .flatten()
                        .find(|entry| {
                            entry.file_type().is_ok_and(|kind| kind.is_file())
                                && entry.file_name().to_str().is_some_and(|name| {
                                    name.starts_with("rollout-") && name.ends_with(&suffix)
                                })
                        })
                {
                    return Some(entry.path());
                }
            }
        }
    }
    None
}

fn recent_dirs(path: &Path, take: usize) -> impl Iterator<Item = PathBuf> {
    let mut entries: Vec<_> = fs::read_dir(path)
        .ok()
        .into_iter()
        .flatten()
        .flatten()
        .filter(|entry| {
            entry.file_type().is_ok_and(|kind| kind.is_dir())
                && entry
                    .file_name()
                    .as_encoded_bytes()
                    .iter()
                    .all(u8::is_ascii_digit)
        })
        .map(|entry| entry.path())
        .collect();
    entries.sort_unstable();
    entries.into_iter().rev().take(take)
}

fn read_model(path: &Path) -> Option<String> {
    let mut file = fs::File::open(path).ok()?;
    let offset = file.metadata().ok()?.len().saturating_sub(MAX_TAIL);
    file.seek(SeekFrom::Start(offset)).ok()?;
    let mut tail = Vec::new();
    file.take(MAX_TAIL).read_to_end(&mut tail).ok()?;
    for line in tail.rsplit(|byte| *byte == b'\n') {
        // The other rollout event types can contain prompts and responses.
        // Read only the model and effort from Codex turn context.
        let Ok(event) = serde_json::from_slice::<Value>(line) else {
            continue;
        };
        if event["type"] != "turn_context" {
            continue;
        }
        let context = &event["payload"];
        let model = context["model"]
            .as_str()
            .filter(|s| !s.is_empty() && s.len() <= 128)?;
        let effort = context["effort"]
            .as_str()
            .filter(|s| !s.is_empty() && s.len() <= 32);
        return Some(effort.map_or_else(|| model.to_owned(), |effort| format!("{model} {effort}")));
    }
    None
}
