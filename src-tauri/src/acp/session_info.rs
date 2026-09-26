//! Session-info lookup domain types — backing the `get_session_info` MCP tool.
//!
//! When the user references a session in the composer it serializes into the
//! agent's prompt as `[title](codeg://session/<conversation_id>)`. The agent
//! reads the numeric id out of that link and calls `get_session_info`, which
//! resolves it to the session's metadata + token-usage stats and, on demand, a
//! bounded compacted view of its recent messages.
//!
//! This module holds the layer-shared pieces (mirroring [`crate::acp::question`]
//! / [`crate::acp::feedback`]):
//!   * [`SessionInfo`] / [`SessionMessages`] / [`SessionMessageItem`] — the
//!     self-describing outcome delivered over the broker socket to the tool (so
//!     the companion renders it without re-querying).
//!   * [`SessionInfoAccess`] — the listener-facing trait the production
//!     `DbSessionInfoLookup` (in `crate::commands::session_info`) implements; kept
//!     here so the listener can be unit-tested with an in-memory stub.
//!   * [`SessionInfoRuntimeConfig`] — the hot-swappable "is the feature on?" flag,
//!     read at MCP injection time (mirrors [`crate::acp::question`]).

use std::collections::HashMap;
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use tokio::sync::{oneshot, Mutex, RwLock};

use crate::models::conversation::SessionStats;

/// Hard cap on how many recent turns a single `get_session_info` call may
/// request. The companion clamps the agent-supplied `max_messages` to this, and
/// the resolver treats it as the authoritative ceiling so a pathological value
/// can't blow up the UDS frame or the LLM context.
pub const MAX_SESSION_MESSAGES: u32 = 200;

/// A queue hand-off is only accepted after the existing frontend queue has
/// synchronously accepted it. This is a delivery acknowledgement, not a
/// second queue or persistence layer.
pub const SESSION_MESSAGE_QUEUE_ACK_TIMEOUT: Duration = Duration::from_secs(2);

struct SessionMessageQueueHandoff {
    conversation_id: i32,
    sender: oneshot::Sender<()>,
}

static SESSION_MESSAGE_QUEUE_HANDOFFS: OnceLock<
    Mutex<HashMap<String, SessionMessageQueueHandoff>>,
> = OnceLock::new();

fn session_message_queue_handoffs(
) -> &'static Mutex<HashMap<String, SessionMessageQueueHandoff>> {
    SESSION_MESSAGE_QUEUE_HANDOFFS.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Register one transient queue hand-off and return its request id + waiter.
pub async fn register_session_message_queue_handoff(
    conversation_id: i32,
) -> (String, oneshot::Receiver<()>) {
    let request_id = uuid::Uuid::new_v4().to_string();
    let (sender, receiver) = oneshot::channel();
    session_message_queue_handoffs().lock().await.insert(
        request_id.clone(),
        SessionMessageQueueHandoff {
            conversation_id,
            sender,
        },
    );
    (request_id, receiver)
}

/// Complete a hand-off only when the acknowledgement names the same target
/// conversation. A late or duplicate acknowledgement is harmless.
pub async fn acknowledge_session_message_queue(
    request_id: &str,
    conversation_id: i32,
) -> bool {
    let mut handoffs = session_message_queue_handoffs().lock().await;
    let Some(expected) = handoffs.get(request_id) else {
        return false;
    };
    if expected.conversation_id != conversation_id {
        return false;
    }
    let waiter = handoffs
        .remove(request_id)
        .expect("queue handoff disappeared while holding its mutex");
    waiter.sender.send(()).is_ok()
}

/// Wait for the owner composer to acknowledge insertion into its existing FIFO.
/// Removing the waiter on timeout makes the result fail closed and prevents a
/// later stale event from completing a newer request.
pub async fn wait_for_session_message_queue_handoff(
    request_id: &str,
    receiver: oneshot::Receiver<()>,
) -> bool {
    let confirmed = matches!(
        tokio::time::timeout(SESSION_MESSAGE_QUEUE_ACK_TIMEOUT, receiver).await,
        Ok(Ok(()))
    );
    if !confirmed {
        session_message_queue_handoffs()
            .lock()
            .await
            .remove(request_id);
    }
    confirmed
}

/// One compacted turn from the referenced session's transcript. NEVER carries a
/// raw `MessageTurn` / image bytes — only role + truncated text + tool names — so
/// the UDS frame and the agent's context window stay bounded.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionMessageItem {
    /// `"user"` | `"assistant"` | `"system"`.
    pub role: String,
    /// Concatenated text/thinking content of the turn, truncated to a per-turn
    /// character budget.
    pub text: String,
    /// Tool names invoked in this turn (deduped, first-seen order); empty when
    /// the turn ran no tools.
    pub tools: Vec<String>,
}

/// The recent-messages slice of a [`SessionInfo`], present only when the caller
/// asked for messages (`max_messages > 0`) and the transcript parsed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionMessages {
    /// Total number of turns in the session.
    pub total: u32,
    /// How many turns are included in `items` (the most recent ones).
    pub included: u32,
    /// True when older turns were dropped to fit `included` / the char budget.
    pub truncated: bool,
    pub items: Vec<SessionMessageItem>,
}

/// The resolved session description handed back to the `get_session_info` tool.
/// `found == false` is a soft "no such session" result the LLM reads (not an
/// error), produced via [`SessionInfo::not_found`].
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SessionInfo {
    pub found: bool,
    /// The id that was queried (codeg's internal conversation PK). Echoed so the
    /// companion can render a precise not-found message.
    pub session_id: i32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub external_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub agent_type: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub status: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub git_branch: Option<String>,
    /// Absolute path of the session's workspace folder.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub workspace_path: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub workspace_name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub message_count: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub created_at: Option<DateTime<Utc>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub updated_at: Option<DateTime<Utc>>,
    /// The parent conversation id when this session is a delegation child.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub parent_id: Option<i32>,
    /// `true` when this session was spawned by a `delegate_to_agent` call.
    pub is_delegation_child: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stats: Option<SessionStats>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub messages: Option<SessionMessages>,
    /// Human-readable note for a not-found result, or a partial one (e.g. the
    /// transcript parse timed out so `messages` is absent). `None` on a clean
    /// full result.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
}

/// How a `send_session_message` request should enter the target's existing
/// conversation flow.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionMessageAction {
    #[default]
    Send,
    Queue,
    Steer,
}

/// Transient hand-off from an MCP queue choice to the target conversation's
/// existing frontend message queue. It is intentionally not persisted here.
#[derive(Debug, Clone, Serialize)]
pub struct SessionMessageQueued {
    pub request_id: String,
    pub conversation_id: i32,
    pub message: String,
}

/// Outcome of a `send_session_message` request. Failures stay structured so
/// the companion can surface an actionable reason without turning a target
/// session lookup problem into an opaque broker error.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionMessageOutcome {
    pub accepted: bool,
    pub session_id: i32,
    pub status: String,
    pub message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error_code: Option<String>,
    /// Present on a busy refusal so the caller can offer steering only when
    /// this live runtime has a working feedback channel.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub steering_available: Option<bool>,
}

impl SessionMessageOutcome {
    pub fn accepted(session_id: i32) -> Self {
        Self {
            accepted: true,
            session_id,
            status: "accepted".into(),
            message: format!("Message accepted by session #{session_id}; its next agent turn is queued."),
            error_code: None,
            steering_available: None,
        }
    }

    pub fn queued(session_id: i32) -> Self {
        Self {
            accepted: true,
            session_id,
            status: "accepted".into(),
            message: format!(
                "Message queued in session #{session_id}; the conversation's normal queue will send it after the current turn."
            ),
            error_code: None,
            steering_available: None,
        }
    }

    pub fn steered(session_id: i32) -> Self {
        Self {
            accepted: true,
            session_id,
            status: "accepted".into(),
            message: format!("Message steered into session #{session_id}'s current turn."),
            error_code: None,
            steering_available: None,
        }
    }

    pub fn failed(session_id: i32, error_code: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            accepted: false,
            session_id,
            status: "failed".into(),
            message: message.into(),
            error_code: Some(error_code.into()),
            steering_available: None,
        }
    }

    pub fn failed_with_steering(
        session_id: i32,
        error_code: impl Into<String>,
        message: impl Into<String>,
        steering_available: bool,
    ) -> Self {
        let mut outcome = Self::failed(session_id, error_code, message);
        outcome.steering_available = Some(steering_available);
        outcome
    }
}

impl SessionInfo {
    /// A "no session matches this id" outcome — the id was well-formed but no
    /// non-deleted conversation row exists for it.
    pub fn not_found(session_id: i32) -> Self {
        Self {
            found: false,
            session_id,
            note: Some(format!(
                "No session matches id {session_id}. It may have been deleted, \
                 or never imported into codeg."
            )),
            ..Default::default()
        }
    }
}

/// Listener-facing access to resolve a session by its codeg conversation id or
/// deliver a message to it. The production impl
/// (`crate::commands::session_info::DbSessionInfoLookup`) reads the DB + parses
/// the on-disk transcript and owns the live ACP send path; tests use an
/// in-memory stub. Mirrors [`crate::acp::feedback::SessionFeedbackAccess`] and
/// [`crate::acp::question::SessionQuestionAccess`].
#[async_trait]
pub trait SessionInfoAccess: Send + Sync {
    /// Resolve `session_id` (codeg's internal conversation PK) into a
    /// [`SessionInfo`]. When `max_messages > 0`, include up to that many of the
    /// most recent compacted turns (capped at [`MAX_SESSION_MESSAGES`]). A
    /// missing / deleted id yields [`SessionInfo::not_found`].
    async fn resolve(&self, session_id: i32, max_messages: u32) -> SessionInfo;

    /// Deliver a normal user prompt to an existing conversation. The default
    /// keeps read-only test/runtime implementations source-compatible while
    /// making the write capability explicit for production wiring.
    async fn send_message(
        &self,
        session_id: i32,
        _parent_connection_id: &str,
        _message: String,
    ) -> SessionMessageOutcome {
        SessionMessageOutcome::failed(
            session_id,
            "session_messaging_unavailable",
            "Sending messages to sessions is unavailable in this process.",
        )
    }

    /// Dispatch an explicit follow-up action. Implementations that only know
    /// the historical normal-send path remain source-compatible; production
    /// wiring overrides this to reuse queue and steering entrypoints.
    async fn send_message_with_action(
        &self,
        session_id: i32,
        parent_connection_id: &str,
        message: String,
        action: SessionMessageAction,
    ) -> SessionMessageOutcome {
        match action {
            SessionMessageAction::Send => {
                self.send_message(session_id, parent_connection_id, message)
                    .await
            }
            SessionMessageAction::Queue | SessionMessageAction::Steer => {
                SessionMessageOutcome::failed(
                    session_id,
                    "session_messaging_unavailable",
                    "This process cannot route the requested session message action.",
                )
            }
        }
    }
}

/// The hot-swappable feature config read at MCP injection time. Kept tiny and
/// separate from the other feature configs so the `sessions` tool group toggles
/// independently — `codeg-mcp` is injected when ANY feature is enabled, and each
/// tool is listed only when its own feature is on.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SessionInfoConfig {
    pub enabled: bool,
}

/// Shared, hot-swappable handle to [`SessionInfoConfig`]. Cloned into
/// `DelegationInjection` (read at injection) and `AppState` (updated on save).
/// Byte-for-byte mirror of [`crate::acp::question::QuestionRuntimeConfig`].
#[derive(Clone, Default)]
pub struct SessionInfoRuntimeConfig {
    inner: Arc<RwLock<SessionInfoConfig>>,
}

impl SessionInfoRuntimeConfig {
    pub fn new() -> Self {
        Self::default()
    }

    pub async fn snapshot(&self) -> SessionInfoConfig {
        self.inner.read().await.clone()
    }

    pub async fn set(&self, cfg: SessionInfoConfig) {
        *self.inner.write().await = cfg;
    }

    /// Convenience read used at MCP injection time.
    pub async fn is_enabled(&self) -> bool {
        self.inner.read().await.enabled
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn not_found_is_soft_and_carries_id() {
        let info = SessionInfo::not_found(42);
        assert!(!info.found);
        assert_eq!(info.session_id, 42);
        assert!(info.messages.is_none());
        assert!(info.note.as_deref().unwrap().contains("42"));
    }

    #[test]
    fn not_found_serializes_without_absent_option_fields() {
        // The skip_serializing_if Options keep the not-found envelope compact.
        let v = serde_json::to_value(SessionInfo::not_found(7)).unwrap();
        assert_eq!(v["found"], false);
        assert_eq!(v["session_id"], 7);
        assert!(v.get("title").is_none());
        assert!(v.get("messages").is_none());
        assert!(v.get("note").is_some());
    }

    #[tokio::test]
    async fn runtime_config_round_trips() {
        let cfg = SessionInfoRuntimeConfig::new();
        assert!(!cfg.is_enabled().await);
        cfg.set(SessionInfoConfig { enabled: true }).await;
        assert!(cfg.is_enabled().await);
        assert_eq!(cfg.snapshot().await, SessionInfoConfig { enabled: true });
    }

    #[tokio::test]
    async fn queue_handoff_requires_matching_target_and_confirms_success() {
        let (request_id, receiver) = register_session_message_queue_handoff(42).await;
        assert!(!acknowledge_session_message_queue(&request_id, 7).await);
        assert!(acknowledge_session_message_queue(&request_id, 42).await);
        assert!(wait_for_session_message_queue_handoff(&request_id, receiver).await);
    }

    #[tokio::test]
    async fn queue_handoff_without_a_receiver_fails_closed() {
        let (sender, receiver) = oneshot::channel();
        drop(sender);
        assert!(!wait_for_session_message_queue_handoff("missing", receiver).await);
    }
}
