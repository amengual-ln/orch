use serde::Serialize;

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum FailureKind {
    Configuration,
    Io,
    Timeout,
    Process,
    Protocol,
    Provider,
}

#[derive(Debug, Clone, Serialize, PartialEq, Eq)]
#[serde(tag = "event", rename_all = "snake_case")]
pub enum AgentEvent {
    TurnStarted,
    SessionInitialized { provider_session_id: String },
    MessageCompleted { message_id: String, text: String },
    TurnCompleted { final_text: String },
    TurnFailed { kind: FailureKind, message: String },
    TurnCancelled,
}

#[derive(Debug, Serialize)]
pub struct Event<'a> {
    pub session_id: &'a str,
    pub turn_id: u64,
    #[serde(flatten)]
    pub payload: AgentEvent,
}
