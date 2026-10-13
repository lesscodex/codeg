//! `get_session_info` backing logic + settings persistence.
//!
//! Two surfaces live here, mirroring `crate::commands::question`:
//!
//!   * [`DbSessionInfoLookup`] — the production [`SessionInfoAccess`] impl the
//!     delegation listener calls to resolve a referenced session
//!     (`codeg://session/<id>`) into metadata + token stats and, on demand, a
//!     bounded compacted view of its recent messages. It reuses
//!     [`get_folder_conversation_core`] (which reads the conversation row, uses
//!     its bound `external_id` + `agent_type` to pick the right parser, and parses
//!     the on-disk transcript off the runtime via `spawn_blocking`).
//!   * The `session_info.enabled` settings knob (**default true**) — read at MCP
//!     injection time via [`SessionInfoRuntimeConfig`]. Persist + apply + broadcast
//!     follows the exact shape of the ask-question / feedback toggles.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use sea_orm::DatabaseConnection;
use serde::{Deserialize, Serialize};

use crate::acp::session_info::{
    SessionInfo, SessionInfoAccess, SessionInfoConfig, SessionInfoRuntimeConfig, SessionMessageItem,
    SessionMessageOutcome, SessionMessages, MAX_SESSION_MESSAGES,
};
use crate::acp::delegation::spawner::ConnectionSpawner;
use crate::acp::manager::{ConnectionManager, ConnectionManagerSpawner};
use crate::acp::types::ConnectionStatus;
use crate::app_error::AppCommandError;
use crate::commands::conversations::get_folder_conversation_core;
use crate::db::service::{
    app_metadata_service, conversation_selector_preference_service, conversation_service,
    folder_service,
};
use crate::db::AppDatabase;
use crate::models::agent::AgentType;
use crate::models::message::{ContentBlock, MessageTurn, TurnRole};
use crate::web::event_bridge::{emit_event, EventEmitter, SESSION_INFO_SETTINGS_CHANGED_EVENT};

/// Upper bound on a single compacted turn's text. Keeps one chatty turn from
/// dominating the budget; longer text is truncated with an ellipsis marker.
const PER_TURN_CHARS: usize = 1_500;

/// Overall character budget across all included turns (text AND tool names).
/// Walked newest-first so the most recent context survives; older turns are
/// dropped once this is exhausted.
const OVERALL_CHARS: usize = 16_000;

/// Per-turn caps on the collected tool names, so a turn with thousands of
/// `ToolUse` blocks (or one with a pathologically long tool name) can't inflate
/// the payload past the text budget. Tool-name chars are ALSO charged against
/// [`OVERALL_CHARS`].
const MAX_TOOLS_PER_TURN: usize = 16;
const MAX_TOOL_NAME_CHARS: usize = 64;

/// How long the transcript parse may run before the resolver gives up and returns
/// metadata only. The session keeps existing; the agent simply gets no messages
/// this call (with a `note` explaining why). Generous for large session files,
/// bounded so a pathological file can't pin the round-trip.
///
/// NOTE: this bounds the *response latency*, not the underlying work — the parse
/// runs on tokio's blocking pool (`spawn_blocking` inside
/// `get_folder_conversation_core`), which cannot be canceled, so a timed-out parse
/// keeps running to completion in the background. [`MAX_CONCURRENT_PARSES`] is what
/// actually protects the blocking pool from a flood of large-session reads.
const PARSE_TIMEOUT: Duration = Duration::from_secs(8);

/// Cap on concurrent transcript parses across all `get_session_info` calls. A
/// small bound keeps a burst of reads on large sessions from saturating tokio's
/// blocking pool (each parse pins one blocking thread, uncancelable). Excess
/// concurrent callers do NOT queue — `bounded_parse` uses a non-blocking
/// `try_acquire`, so a call that finds every slot taken returns `Busy` and
/// degrades to metadata-only immediately.
const MAX_CONCURRENT_PARSES: usize = 4;

const MCP_FORWARD_SEPARATOR: &str = "\n\n---\n\n";

async fn current_parent_conversation_id(
    manager: &ConnectionManager,
    parent_connection_id: &str,
) -> Option<i32> {
    let state = manager.get_state(parent_connection_id).await?;
    let state = state.read().await;
    state.conversation_id
}

fn format_mcp_forwarded_message(source_conversation_id: Option<i32>, message: String) -> String {
    let source = source_conversation_id.map_or_else(
        || "Source session ID unavailable".to_string(),
        |conversation_id| format!("Source session: codeg://session/{conversation_id}"),
    );
    format!(
        "[Forwarded from another session through MCP; not typed directly by the user in this receiving conversation. {source}]{}{}",
        MCP_FORWARD_SEPARATOR, message
    )
}

/// Production [`SessionInfoAccess`]: resolves a session by codeg conversation id
/// against the DB + on-disk transcript. Wraps an `Arc<AppDatabase>` (like
/// `DbDepthLookup` / `DbChildStatusLookup`) plus a semaphore bounding concurrent
/// transcript parses. Construct via [`DbSessionInfoLookup::new`].
pub struct DbSessionInfoLookup {
    pub db: Arc<AppDatabase>,
    /// Limits concurrent `get_folder_conversation_core` parses (see
    /// [`MAX_CONCURRENT_PARSES`]).
    parse_limit: Arc<tokio::sync::Semaphore>,
    /// Optional live-connection access. Read-only callers/tests can keep using
    /// `new`; production wiring uses `new_with_messaging` for
    /// `send_session_message`.
    manager: Option<Arc<ConnectionManager>>,
    data_dir: Option<Arc<PathBuf>>,
}

#[derive(Debug, Clone)]
struct LiveTarget {
    connection_id: String,
    allow_unlinked: bool,
}

impl DbSessionInfoLookup {
    pub fn new(db: Arc<AppDatabase>) -> Self {
        Self {
            db,
            parse_limit: Arc::new(tokio::sync::Semaphore::new(MAX_CONCURRENT_PARSES)),
            manager: None,
            data_dir: None,
        }
    }

    pub fn new_with_messaging(
        db: Arc<AppDatabase>,
        manager: Arc<ConnectionManager>,
        data_dir: PathBuf,
    ) -> Self {
        Self {
            db,
            parse_limit: Arc::new(tokio::sync::Semaphore::new(MAX_CONCURRENT_PARSES)),
            manager: Some(manager),
            data_dir: Some(Arc::new(data_dir)),
        }
    }

    async fn find_live_target(
        &self,
        manager: &ConnectionManager,
        session_id: i32,
        agent_type: AgentType,
        external_id: &str,
        workspace_path: &str,
    ) -> Result<Option<LiveTarget>, SessionMessageOutcome> {
        if let Some(connection_id) = manager.find_connection_by_conversation_id(session_id).await
        {
            match Self::validate_connection(
                manager,
                &connection_id,
                session_id,
                agent_type,
                external_id,
                workspace_path,
            )
            .await
            {
                Ok(allow_unlinked) => {
                    return Ok(Some(LiveTarget {
                        connection_id,
                        allow_unlinked,
                    }))
                }
                Err(outcome)
                    if outcome.error_code.as_deref() == Some("connection_unavailable") => {}
                Err(outcome) => return Err(outcome),
            }
        }

        if let Some(connection_id) = manager
            .find_connection_by_external_id(external_id, agent_type)
            .await
        {
            match Self::validate_connection(
                manager,
                &connection_id,
                session_id,
                agent_type,
                external_id,
                workspace_path,
            )
            .await
            {
                Ok(allow_unlinked) => {
                    return Ok(Some(LiveTarget {
                        connection_id,
                        allow_unlinked,
                    }))
                }
                Err(outcome)
                    if outcome.error_code.as_deref() == Some("connection_unavailable") => {}
                Err(outcome) => return Err(outcome),
            }
        }

        Ok(None)
    }

    async fn validate_connection(
        manager: &ConnectionManager,
        connection_id: &str,
        session_id: i32,
        agent_type: AgentType,
        external_id: &str,
        workspace_path: &str,
    ) -> Result<bool, SessionMessageOutcome> {
        let Some(state) = manager.get_state(connection_id).await else {
            return Err(SessionMessageOutcome::failed(
                session_id,
                "connection_unavailable",
                format!(
                    "The live connection for session #{session_id} disappeared before delivery; try again."
                ),
            ));
        };
        let state = state.read().await;
        if matches!(
            state.status,
            ConnectionStatus::Disconnected | ConnectionStatus::Error
        ) {
            return Err(SessionMessageOutcome::failed(
                session_id,
                "connection_unavailable",
                format!(
                    "The agent connection for session #{session_id} is no longer usable; Codeg will try to resume it."
                ),
            ));
        }
        if state.agent_type != agent_type
            || state.external_id.as_deref() != Some(external_id)
            || state.working_dir.as_deref() != Some(Path::new(workspace_path))
            || state
                .conversation_id
                .is_some_and(|bound_id| bound_id != session_id)
        {
            return Err(SessionMessageOutcome::failed(
                session_id,
                "identity_mismatch",
                format!(
                    "The live connection does not identify the requested agent session #{session_id}; no message was sent."
                ),
            ));
        }
        if state.turn_in_flight {
            return Err(SessionMessageOutcome::failed(
                session_id,
                "target_busy",
                format!(
                    "The target session #{session_id} already has an agent turn in progress; retry after it finishes."
                ),
            ));
        }
        Ok(state.conversation_id.is_none())
    }
}

#[async_trait]
impl SessionInfoAccess for DbSessionInfoLookup {
    async fn resolve(&self, session_id: i32, max_messages: u32) -> SessionInfo {
        let conn = &self.db.conn;
        // get_by_id is the authoritative existence + metadata source (cheap PK
        // lookup, already filters soft-deleted). Resolve it FIRST so a missing
        // session is cleanly "not found" rather than conflated with a parse error.
        let summary = match conversation_service::get_by_id(conn, session_id).await {
            Ok(s) => s,
            Err(_) => return SessionInfo::not_found(session_id),
        };

        let (workspace_path, workspace_name) =
            match folder_service::get_folder_by_id(conn, summary.folder_id).await {
                Ok(Some(folder)) => (Some(folder.path), Some(folder.name)),
                _ => (None, None),
            };

        let mut info = SessionInfo {
            found: true,
            session_id: summary.id,
            external_id: summary.external_id.clone(),
            agent_type: Some(agent_type_str(summary.agent_type)),
            title: summary.title.clone().filter(|t| !t.trim().is_empty()),
            status: Some(summary.status.clone()),
            model: summary.model.clone(),
            git_branch: summary.git_branch.clone(),
            workspace_path,
            workspace_name,
            message_count: Some(summary.message_count),
            created_at: Some(summary.created_at),
            updated_at: Some(summary.updated_at),
            parent_id: summary.parent_id,
            is_delegation_child: summary.parent_id.is_some(),
            stats: None,
            messages: None,
            note: None,
        };

        if max_messages == 0 {
            return info;
        }
        let max = max_messages.min(MAX_SESSION_MESSAGES);

        // Parse the transcript off the hot path, gated by `parse_limit` and bounded
        // by PARSE_TIMEOUT (see `bounded_parse`). A busy / timeout / parse error all
        // degrade to metadata-only with an explanatory note rather than failing the
        // whole tool call.
        let conn_owned = self.db.conn.clone();
        let parse =
            async move { get_folder_conversation_core(&conn_owned, session_id).await };
        match bounded_parse(self.parse_limit.clone(), PARSE_TIMEOUT, parse).await {
            ParseSlot::Ready(Ok((detail, parsed_title))) => {
                if info.title.is_none() {
                    info.title = parsed_title.filter(|t| !t.trim().is_empty());
                }
                // get_folder_conversation_core sets summary.message_count to the
                // parsed turn count — more accurate than the stored row.
                info.message_count = Some(detail.summary.message_count);
                info.stats = detail.session_stats;
                info.messages = Some(compact_turns(&detail.turns, max));
            }
            ParseSlot::Ready(Err(_)) => {
                info.note = Some(
                    "Recent messages are unavailable — the session transcript could not be parsed."
                        .to_string(),
                );
            }
            ParseSlot::Busy => {
                info.note = Some(
                    "Recent messages are unavailable — too many session reads are in progress. \
                     Retry, or call again with max_messages: 0 for metadata only."
                        .to_string(),
                );
            }
            ParseSlot::TimedOut => {
                info.note = Some(
                    "Recent messages are unavailable — reading the session transcript timed out. \
                     Retry, or call again with max_messages: 0 for metadata only."
                        .to_string(),
                );
            }
        }
        info
    }

    async fn send_message(
        &self,
        session_id: i32,
        parent_connection_id: &str,
        message: String,
    ) -> SessionMessageOutcome {
        let Some(manager) = self.manager.clone() else {
            return SessionMessageOutcome::failed(
                session_id,
                "session_messaging_unavailable",
                "Sending messages to sessions is unavailable in this process.",
            );
        };
        let Some(data_dir) = self.data_dir.clone() else {
            return SessionMessageOutcome::failed(
                session_id,
                "session_messaging_unavailable",
                "The session resume runtime is not configured; relaunch the Codeg process and try again.",
            );
        };
        let forwarded_message = format_mcp_forwarded_message(
            current_parent_conversation_id(&manager, parent_connection_id).await,
            message,
        );

        let summary = match conversation_service::get_by_id(&self.db.conn, session_id).await {
            Ok(summary) => summary,
            Err(_) => {
                return SessionMessageOutcome::failed(
                    session_id,
                    "target_not_found",
                    format!(
                        "No non-deleted Codeg conversation matches session #{session_id}; verify the codeg://session/<id> reference."
                    ),
                )
            }
        };
        let Some(external_id) = summary
            .external_id
            .clone()
            .filter(|id| !id.trim().is_empty())
        else {
            return SessionMessageOutcome::failed(
                session_id,
                "missing_external_session",
                format!(
                    "Session #{session_id} has no persisted external agent session id and cannot be resumed safely."
                ),
            );
        };
        let Some(folder) = folder_service::get_folder_by_id(&self.db.conn, summary.folder_id)
            .await
            .ok()
            .flatten()
        else {
            return SessionMessageOutcome::failed(
                session_id,
                "target_workspace_missing",
                format!(
                    "Session #{session_id}'s workspace folder is missing; restore the folder before sending a message."
                ),
            );
        };

        let target = match self
            .find_live_target(
                &manager,
                session_id,
                summary.agent_type,
                &external_id,
                &folder.path,
            )
            .await
        {
            Ok(Some(target)) => (target.connection_id, None, target.allow_unlinked),
            Ok(None) => {
                let preferences = match conversation_selector_preference_service::get(
                    &self.db.conn,
                    session_id,
                )
                .await
                {
                    Ok(preferences) => preferences.unwrap_or(
                        conversation_selector_preference_service::ConversationSelectorPreferences {
                            mode_id: None,
                            config_values: std::collections::BTreeMap::new(),
                        },
                    ),
                    Err(error) => {
                        return SessionMessageOutcome::failed(
                            session_id,
                            "resume_failed",
                            format!(
                                "Could not load selector preferences for session #{session_id}: {error}"
                            ),
                        )
                    }
                };
                let spawner = ConnectionManagerSpawner {
                    manager: manager.clone(),
                    db: self.db.clone(),
                    data_dir,
                };
                let resumed = match spawner
                    .spawn_for_resume(
                        parent_connection_id,
                        summary.agent_type,
                        Some(folder.path.clone()),
                        &external_id,
                        preferences.mode_id,
                        preferences.config_values,
                    )
                    .await
                {
                    Ok(spawned) => spawned,
                    Err(error) => {
                        return SessionMessageOutcome::failed(
                            session_id,
                            "resume_failed",
                            format!(
                                "Could not resume session #{session_id} in {}: {error}. Verify the agent installation and workspace, then try again.",
                                folder.path
                            ),
                        )
                    }
                };
                let lease = if resumed.reused {
                    None
                } else {
                    manager.prompt_connection_lease(&resumed.connection_id).await
                };
                if let Err(error) = manager
                    .set_selector_preferences_target(&resumed.connection_id, session_id)
                    .await
                {
                    if let Some(lease) = lease.as_ref() {
                        let _ = manager
                            .disconnect_if_unclaimed(&resumed.connection_id, lease)
                            .await;
                    }
                    return SessionMessageOutcome::failed(
                        session_id,
                        "identity_mismatch",
                        format!(
                            "Could not bind selector preferences to session #{session_id}: {error}"
                        ),
                    );
                }
                if let Err(error) = crate::acp::lifecycle::persist_selector_preferences(
                    &self.db.conn,
                    &manager,
                    &resumed.connection_id,
                )
                .await
                {
                    tracing::warn!(
                        session_id,
                        %error,
                        "[session-info] failed to persist selector preferences after resume"
                    );
                }
                let allow_unlinked = match Self::validate_connection(
                    &manager,
                    &resumed.connection_id,
                    session_id,
                    summary.agent_type,
                    &external_id,
                    &folder.path,
                )
                .await
                {
                    Ok(allow_unlinked) => allow_unlinked,
                    Err(outcome) => {
                        if let Some(lease) = lease.as_ref() {
                            let _ = manager
                                .disconnect_if_unclaimed(&resumed.connection_id, lease)
                                .await;
                        }
                        return outcome;
                    }
                };
                (resumed.connection_id, lease, allow_unlinked)
            }
            Err(outcome) => return outcome,
        };

        let (connection_id, lease, allow_unlinked) = target;
        let send = manager
            .send_prompt_linked_for_session(
                &self.db,
                &connection_id,
                vec![crate::acp::types::PromptInputBlock::Text {
                    text: forwarded_message,
                }],
                summary.folder_id,
                session_id,
                crate::acp::manager::PromptSessionIdentity {
                    agent_type: summary.agent_type,
                    external_id: external_id.clone(),
                    working_dir: PathBuf::from(&folder.path),
                    conversation_id: session_id,
                    allow_unlinked,
                },
            )
            .await;
        match send {
            Ok(Some(bound_id)) if bound_id == session_id => {
                SessionMessageOutcome::accepted(session_id)
            }
            Ok(Some(bound_id)) => {
                if let Some(lease) = lease.as_ref() {
                    let _ = manager
                        .disconnect_if_unclaimed(&connection_id, lease)
                        .await;
                }
                SessionMessageOutcome::failed(
                    session_id,
                    "identity_mismatch",
                    format!(
                        "The target connection bound the prompt to session #{bound_id} instead of session #{session_id}; no message was accepted in the requested conversation."
                    ),
                )
            }
            Ok(None) => {
                if let Some(lease) = lease.as_ref() {
                    let _ = manager
                        .disconnect_if_unclaimed(&connection_id, lease)
                        .await;
                }
                SessionMessageOutcome::failed(
                    session_id,
                    "identity_mismatch",
                    format!(
                        "The target connection accepted no conversation binding for session #{session_id}; no message was accepted."
                    ),
                )
            }
            Err(error) => {
                if let Some(lease) = lease.as_ref() {
                    let _ = manager
                        .disconnect_if_unclaimed(&connection_id, lease)
                        .await;
                }
                let (code, detail) = if matches!(
                    &error,
                    crate::acp::error::AcpError::TurnInProgress
                ) {
                    ("target_busy", error.to_string())
                } else if matches!(
                    &error,
                    crate::acp::error::AcpError::SessionIdentityMismatch
                ) {
                    ("identity_mismatch", error.to_string())
                } else {
                    (error.code().unwrap_or("delivery_failed"), error.to_string())
                };
                SessionMessageOutcome::failed(
                    session_id,
                    code,
                    format!(
                        "The message was not accepted by session #{session_id}: {detail}. Retry when the target agent is connected."
                    ),
                )
            }
        }
    }
}

/// Outcome of a permit-gated, timeout-bounded parse (see [`bounded_parse`]).
enum ParseSlot<T> {
    /// The parse completed within the timeout; carries its result.
    Ready(T),
    /// No parse slot was free — work was NOT started (caller should degrade now).
    Busy,
    /// A slot was taken and the parse started, but the caller's wait elapsed. The
    /// detached worker keeps its permit until the (uncancelable) work finishes, so
    /// the slot count keeps bounding real blocking concurrency.
    TimedOut,
}

/// Run `parse` under a real concurrency bound: acquire one of `parse_limit`'s
/// permits with a NON-blocking `try_acquire` (so a flood returns [`ParseSlot::Busy`]
/// instead of queueing), spawn the work into a detached task that holds the permit
/// until `parse` actually returns, and wait for it for at most `timeout`.
///
/// The permit lives with the spawned task — NOT with the caller's `timeout` future
/// — so when the caller gives up ([`ParseSlot::TimedOut`]) the permit stays held
/// until the underlying (uncancelable `spawn_blocking`) parse completes. That is
/// what makes `MAX_CONCURRENT_PARSES` bound the number of in-flight blocking parses
/// even under repeated timeouts, rather than just the number of waiting callers.
async fn bounded_parse<Fut, T>(
    parse_limit: Arc<tokio::sync::Semaphore>,
    timeout: Duration,
    parse: Fut,
) -> ParseSlot<T>
where
    Fut: std::future::Future<Output = T> + Send + 'static,
    T: Send + 'static,
{
    // try_acquire (not acquire): don't queue behind busy parses — fail fast so the
    // caller can return metadata-only immediately instead of piling up tasks.
    let Ok(permit) = parse_limit.try_acquire_owned() else {
        return ParseSlot::Busy;
    };
    let handle = tokio::spawn(async move {
        // Hold the permit for the WHOLE parse; released only when `parse` returns.
        let _permit = permit;
        parse.await
    });
    match tokio::time::timeout(timeout, handle).await {
        Ok(Ok(value)) => ParseSlot::Ready(value),
        // Task panicked — treat as a failed parse (no value).
        Ok(Err(_join)) => ParseSlot::TimedOut,
        // Caller's wait elapsed; the detached task keeps running with its permit.
        Err(_) => ParseSlot::TimedOut,
    }
}

/// Snake_case wire form of an [`AgentType`] (e.g. `claude_code`) — matches the
/// `conversation.agent_type` column and the frontend's `ALL_AGENT_TYPES`.
fn agent_type_str(at: AgentType) -> String {
    serde_json::to_value(at)
        .ok()
        .and_then(|v| v.as_str().map(String::from))
        .unwrap_or_default()
}

/// Compact the most recent `max` turns into [`SessionMessages`]. Walks the turns
/// newest-first so the freshest context is kept under [`OVERALL_CHARS`], always
/// retaining at least the newest turn, then restores chronological order.
fn compact_turns(turns: &[MessageTurn], max: u32) -> SessionMessages {
    let total = turns.len();
    let max = max as usize;
    let mut items: Vec<SessionMessageItem> = Vec::new();
    let mut budget = OVERALL_CHARS;

    for turn in turns.iter().rev() {
        if items.len() >= max {
            break;
        }
        let item = compact_turn(turn);
        // Charge BOTH the text and the (bounded) tool names against the budget so
        // a turn can't smuggle an oversized payload through `tools`.
        let cost = item.text.chars().count()
            + item
                .tools
                .iter()
                .map(|t| t.chars().count())
                .sum::<usize>();
        // Always keep the newest turn; stop once the budget can't fit the next.
        if !items.is_empty() && cost > budget {
            break;
        }
        budget = budget.saturating_sub(cost);
        items.push(item);
    }
    items.reverse();

    let included = items.len();
    SessionMessages {
        total: total as u32,
        included: included as u32,
        truncated: included < total,
        items,
    }
}

/// One turn → role + truncated text (Text/Thinking blocks, and a failed
/// turn's account of the failure) + tool names.
fn compact_turn(turn: &MessageTurn) -> SessionMessageItem {
    let role = match turn.role {
        TurnRole::User => "user",
        TurnRole::Assistant => "assistant",
        TurnRole::System => "system",
    }
    .to_string();

    let mut parts: Vec<&str> = Vec::new();
    let mut tools: Vec<String> = Vec::new();
    for block in &turn.blocks {
        match block {
            ContentBlock::Text { text }
            | ContentBlock::Thinking { text }
            | ContentBlock::TurnError { message: text } => {
                let t = text.trim();
                if !t.is_empty() {
                    parts.push(t);
                }
            }
            // Bound the count per turn and each name's length; dedup on the
            // truncated form so the collected `tools` can't exceed
            // MAX_TOOLS_PER_TURN * MAX_TOOL_NAME_CHARS. Once the cap is hit the
            // guard fails and further ToolUse blocks fall through to `_`.
            ContentBlock::ToolUse { tool_name, .. }
                if tools.len() < MAX_TOOLS_PER_TURN && !tool_name.is_empty() =>
            {
                let name = truncate_chars(tool_name, MAX_TOOL_NAME_CHARS);
                if !tools.contains(&name) {
                    tools.push(name);
                }
            }
            // Tool results, images, and image-generation carry no useful plain
            // text for a compact preview — skipped to keep the payload lean.
            _ => {}
        }
    }
    let text = truncate_chars(&parts.join("\n"), PER_TURN_CHARS);
    SessionMessageItem { role, text, tools }
}

/// Truncate to at most `cap` characters, appending an ellipsis marker when cut.
fn truncate_chars(s: &str, cap: usize) -> String {
    if s.chars().count() <= cap {
        return s.to_string();
    }
    let mut out: String = s.chars().take(cap).collect();
    out.push('…');
    out
}

// ===========================================================================
// Settings persistence — `session_info.enabled` (default ON). Mirrors
// `crate::commands::question`.
// ===========================================================================

pub const KEY_SESSION_INFO_ENABLED: &str = "session_info.enabled";

/// On by default (`enabled: true`). The `get_session_info` tool is read-only and
/// only fires when the user references a session, so — like `ask_user_question` —
/// it ships enabled; a user who never wants agents reading session data can turn
/// it off in settings.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SessionInfoSettings {
    pub enabled: bool,
}

impl Default for SessionInfoSettings {
    fn default() -> Self {
        Self { enabled: true }
    }
}

impl SessionInfoSettings {
    fn into_runtime_config(self) -> SessionInfoConfig {
        SessionInfoConfig {
            enabled: self.enabled,
        }
    }
}

/// Read the persisted key from `app_metadata`, falling back to the default
/// (enabled) for a missing or malformed value. Never errors hard.
pub async fn load_session_info_settings(conn: &DatabaseConnection) -> SessionInfoSettings {
    let mut settings = SessionInfoSettings::default();
    if let Ok(Some(raw)) = app_metadata_service::get_value(conn, KEY_SESSION_INFO_ENABLED).await {
        if let Ok(v) = raw.parse::<bool>() {
            settings.enabled = v;
        }
    }
    settings
}

/// Pull settings from the DB and push the resulting `SessionInfoConfig` onto the
/// shared runtime handle. Idempotent — safe on startup or after any save.
pub async fn apply_persisted_session_info_config(
    conn: &DatabaseConnection,
    config: &SessionInfoRuntimeConfig,
) {
    let settings = load_session_info_settings(conn).await;
    config.set(settings.into_runtime_config()).await;
}

/// Persist + apply + broadcast. Shared by the Tauri command and the HTTP handler
/// so the write + re-apply + notify chain lives in one place.
pub async fn set_session_info_settings_core(
    conn: &DatabaseConnection,
    config: &SessionInfoRuntimeConfig,
    emitter: &EventEmitter,
    desired: SessionInfoSettings,
) -> Result<SessionInfoSettings, AppCommandError> {
    app_metadata_service::upsert_value(
        conn,
        KEY_SESSION_INFO_ENABLED,
        &desired.enabled.to_string(),
    )
    .await
    .map_err(AppCommandError::from)?;
    config.set(desired.clone().into_runtime_config()).await;
    emit_event(emitter, SESSION_INFO_SETTINGS_CHANGED_EVENT, &desired);
    Ok(desired)
}

// -------- Tauri commands -----------------------------------------------------

#[cfg_attr(feature = "tauri-runtime", tauri::command)]
pub async fn get_session_info_settings(
    #[cfg(feature = "tauri-runtime")] db: tauri::State<'_, crate::db::AppDatabase>,
) -> Result<SessionInfoSettings, AppCommandError> {
    #[cfg(feature = "tauri-runtime")]
    {
        Ok(load_session_info_settings(&db.conn).await)
    }
    #[cfg(not(feature = "tauri-runtime"))]
    {
        Err(AppCommandError::configuration_invalid("tauri-only command"))
    }
}

#[cfg_attr(feature = "tauri-runtime", tauri::command)]
pub async fn set_session_info_settings(
    #[cfg(feature = "tauri-runtime")] app: tauri::AppHandle,
    #[cfg(feature = "tauri-runtime")] db: tauri::State<'_, crate::db::AppDatabase>,
    #[cfg(feature = "tauri-runtime")] config: tauri::State<'_, SessionInfoRuntimeConfig>,
    settings: SessionInfoSettings,
) -> Result<SessionInfoSettings, AppCommandError> {
    #[cfg(feature = "tauri-runtime")]
    {
        let emitter = EventEmitter::Tauri(app);
        set_session_info_settings_core(&db.conn, &config, &emitter, settings).await
    }
    #[cfg(not(feature = "tauri-runtime"))]
    {
        let _ = settings;
        Err(AppCommandError::configuration_invalid("tauri-only command"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::acp::connection::ConnectionCommand;
    use chrono::Utc;
    use sea_orm::{ActiveModelTrait, Set};
    use crate::models::message::ContentBlock;

    fn turn(role: TurnRole, blocks: Vec<ContentBlock>) -> MessageTurn {
        MessageTurn {
            id: "t".into(),
            native_turn_id: None,
            role,
            blocks,
            timestamp: Utc::now(),
            usage: None,
            duration_ms: None,
            model: None,
            completed_at: None,
        agent_message_id: None,
        }
    }

    #[test]
    fn compact_turn_joins_text_and_collects_tools() {
        let item = compact_turn(&turn(
            TurnRole::Assistant,
            vec![
                ContentBlock::Text { text: "hello".into() },
                ContentBlock::ToolUse {
                    tool_use_id: None,
                    tool_name: "Read".into(),
                    input_preview: None,
                    status: None,
                    meta: None,
                },
                ContentBlock::ToolUse {
                    tool_use_id: None,
                    tool_name: "Read".into(),
                    input_preview: None,
                    status: None,
                    meta: None,
                },
                ContentBlock::Thinking { text: "hmm".into() },
            ],
        ));
        assert_eq!(item.role, "assistant");
        assert_eq!(item.text, "hello\nhmm");
        assert_eq!(item.tools, vec!["Read".to_string()]); // deduped
    }

    /// A failed turn reads as its system line, so an agent looking at the
    /// session sees that the turn failed, and why.
    #[test]
    fn compact_turn_keeps_a_failed_turn_s_account() {
        let item = compact_turn(&turn(
            TurnRole::System,
            vec![ContentBlock::TurnError {
                message: "API Error: 503".into(),
            }],
        ));
        assert_eq!(item.role, "system");
        assert_eq!(item.text, "API Error: 503");
    }

    #[test]
    fn compact_turns_keeps_most_recent_within_max() {
        let turns: Vec<MessageTurn> = (0..5)
            .map(|i| {
                turn(
                    TurnRole::User,
                    vec![ContentBlock::Text {
                        text: format!("msg{i}"),
                    }],
                )
            })
            .collect();
        let out = compact_turns(&turns, 2);
        assert_eq!(out.total, 5);
        assert_eq!(out.included, 2);
        assert!(out.truncated);
        // Chronological order preserved, newest two kept.
        assert_eq!(out.items[0].text, "msg3");
        assert_eq!(out.items[1].text, "msg4");
    }

    #[test]
    fn compact_turn_bounds_tool_names() {
        // A turn with thousands of distinct, long tool names must collapse to at
        // most MAX_TOOLS_PER_TURN names, each ≤ MAX_TOOL_NAME_CHARS.
        let blocks: Vec<ContentBlock> = (0..5000)
            .map(|i| ContentBlock::ToolUse {
                tool_use_id: None,
                tool_name: format!("{}_{i}", "x".repeat(500)),
                input_preview: None,
                status: None,
                meta: None,
            })
            .collect();
        let item = compact_turn(&turn(TurnRole::Assistant, blocks));
        assert!(item.tools.len() <= MAX_TOOLS_PER_TURN);
        for name in &item.tools {
            assert!(name.chars().count() <= MAX_TOOL_NAME_CHARS + 1); // +1 for the ellipsis
        }
    }

    #[test]
    fn compact_turns_charges_tools_against_budget() {
        // Many turns, each carrying the max tool payload, must still be bounded:
        // the budget accounts for tool-name chars, so `included` stays small and
        // `truncated` is set rather than returning every turn.
        let one_turn = || {
            let blocks: Vec<ContentBlock> = (0..MAX_TOOLS_PER_TURN)
                .map(|i| ContentBlock::ToolUse {
                    tool_use_id: None,
                    tool_name: format!("{}_{i}", "t".repeat(MAX_TOOL_NAME_CHARS)),
                    input_preview: None,
                    status: None,
                    meta: None,
                })
                .collect();
            turn(TurnRole::Assistant, blocks)
        };
        let turns: Vec<MessageTurn> = (0..500).map(|_| one_turn()).collect();
        let out = compact_turns(&turns, 500);
        // Total payload chars (text + tools) stays within ~one turn of the budget.
        let total_chars: usize = out
            .items
            .iter()
            .map(|i| {
                i.text.chars().count()
                    + i.tools.iter().map(|t| t.chars().count()).sum::<usize>()
            })
            .sum();
        assert!(total_chars <= OVERALL_CHARS + PER_TURN_CHARS);
        assert!(out.truncated);
        assert!((out.included as usize) < out.total as usize);
    }

    #[test]
    fn compact_turns_not_truncated_when_all_fit() {
        let turns = vec![turn(
            TurnRole::User,
            vec![ContentBlock::Text { text: "only".into() }],
        )];
        let out = compact_turns(&turns, 20);
        assert_eq!(out.total, 1);
        assert_eq!(out.included, 1);
        assert!(!out.truncated);
    }

    #[test]
    fn truncate_chars_marks_when_cut() {
        assert_eq!(truncate_chars("abc", 5), "abc");
        assert_eq!(truncate_chars("abcdef", 3), "abc…");
    }

    #[test]
    fn agent_type_str_is_snake_case() {
        assert_eq!(agent_type_str(AgentType::ClaudeCode), "claude_code");
        assert_eq!(agent_type_str(AgentType::OpenClaw), "open_claw");
    }

    #[tokio::test]
    async fn bounded_parse_returns_value_within_timeout() {
        let sem = Arc::new(tokio::sync::Semaphore::new(1));
        let out = bounded_parse(sem.clone(), Duration::from_secs(5), async { 7 }).await;
        assert!(matches!(out, ParseSlot::Ready(7)));
        // The permit is released once the work completes.
        assert_eq!(sem.available_permits(), 1);
    }

    #[tokio::test]
    async fn bounded_parse_reports_busy_when_no_slot_free() {
        // A semaphore with zero permits → no slot → Busy, work never starts.
        let sem = Arc::new(tokio::sync::Semaphore::new(0));
        let out: ParseSlot<i32> =
            bounded_parse(sem, Duration::from_secs(5), async { 1 }).await;
        assert!(matches!(out, ParseSlot::Busy));
    }

    /// Regression for the review finding: a timed-out parse must keep occupying its
    /// slot until the underlying (uncancelable) work actually finishes — NOT be
    /// released the moment the caller gives up waiting.
    #[tokio::test]
    async fn bounded_parse_holds_permit_past_caller_timeout() {
        let sem = Arc::new(tokio::sync::Semaphore::new(1));
        let (release_tx, release_rx) = tokio::sync::oneshot::channel::<()>();
        // A "parse" that blocks until we signal it, modeling a slow spawn_blocking.
        let parse = async move {
            let _ = release_rx.await;
            99
        };
        // Caller waits only briefly while the work is still blocked → TimedOut.
        let out = bounded_parse(sem.clone(), Duration::from_millis(50), parse).await;
        assert!(matches!(out, ParseSlot::TimedOut));
        // The permit is STILL held by the detached worker (work hasn't finished).
        assert_eq!(sem.available_permits(), 0);
        assert!(sem.try_acquire().is_err());

        // Now let the work complete; the worker drops the permit.
        release_tx.send(()).unwrap();
        // Await the permit returning (generous bound; no fixed sleep) to prove it
        // is released only after the underlying work finishes.
        let _permit = tokio::time::timeout(Duration::from_secs(2), sem.acquire())
            .await
            .expect("permit must be released once the parse completes")
            .unwrap();
    }

    #[tokio::test]
    async fn settings_default_on_and_round_trip() {
        let db = crate::db::test_helpers::fresh_in_memory_db().await;
        assert!(load_session_info_settings(&db.conn).await.enabled);

        let config = SessionInfoRuntimeConfig::new();
        set_session_info_settings_core(
            &db.conn,
            &config,
            &EventEmitter::Noop,
            SessionInfoSettings { enabled: false },
        )
        .await
        .unwrap();
        assert!(!load_session_info_settings(&db.conn).await.enabled);
        assert!(!config.is_enabled().await);
    }

    #[tokio::test]
    async fn resolve_unknown_id_is_not_found() {
        let db = crate::db::test_helpers::fresh_in_memory_db().await;
        let lookup = DbSessionInfoLookup::new(Arc::new(AppDatabase {
            conn: db.conn.clone(),
        }));
        let info = lookup.resolve(999_999, 10).await;
        assert!(!info.found);
        assert_eq!(info.session_id, 999_999);
    }

    #[tokio::test]
    async fn send_message_without_production_wiring_fails_closed() {
        let db = crate::db::test_helpers::fresh_in_memory_db().await;
        let lookup = DbSessionInfoLookup::new(Arc::new(AppDatabase {
            conn: db.conn.clone(),
        }));
        let outcome = lookup
            .send_message(42, "parent", "hello".into())
            .await;
        assert!(!outcome.accepted);
        assert_eq!(outcome.error_code.as_deref(), Some("session_messaging_unavailable"));
    }

    #[test]
    fn mcp_forwarded_message_includes_known_source_link() {
        let body = "keep this body\nexactly as written";
        let marked = format_mcp_forwarded_message(Some(214), body.into());

        assert!(marked.contains("Forwarded from another session through MCP"));
        assert!(marked.contains("codeg://session/214"));
        assert!(marked.ends_with(body));
        assert!(marked.contains(MCP_FORWARD_SEPARATOR));
    }

    #[test]
    fn mcp_forwarded_message_marks_unknown_source() {
        let body = "original body with codeg://session/999 in it";
        let marked = format_mcp_forwarded_message(None, body.into());

        assert!(marked.contains("Forwarded from another session through MCP"));
        assert!(marked.contains("Source session ID unavailable"));
        assert!(marked.ends_with(body));
        assert!(marked.contains(MCP_FORWARD_SEPARATOR));
    }

    #[tokio::test]
    async fn send_message_delivers_to_original_live_conversation() {
        let db = crate::db::test_helpers::fresh_in_memory_db().await;
        let workspace = "/tmp/session-message-success";
        let folder_id = crate::db::test_helpers::seed_folder(&db, workspace).await;
        let row = conversation_service::create(
            &db.conn,
            folder_id,
            AgentType::ClaudeCode,
            None,
            None,
        )
        .await
        .unwrap();
        let mut active: crate::db::entities::conversation::ActiveModel = row.clone().into();
        active.external_id = Set(Some("agent-session-1".into()));
        active.update(&db.conn).await.unwrap();

        let manager = ConnectionManager::new();
        let mut commands = manager
            .insert_test_connection_live(
                "target-connection",
                AgentType::ClaudeCode,
                Some(PathBuf::from(workspace)),
                EventEmitter::Noop,
            )
            .await;
        let _parent_commands = manager
            .insert_test_connection_live(
                "parent-connection",
                AgentType::ClaudeCode,
                Some(PathBuf::from("/tmp/session-message-parent")),
                EventEmitter::Noop,
            )
            .await;
        let parent_state = manager.get_state("parent-connection").await.unwrap();
        parent_state.write().await.conversation_id = Some(214);
        let state = manager.get_state("target-connection").await.unwrap();
        {
            let mut state = state.write().await;
            state.external_id = Some("agent-session-1".into());
            state.conversation_id = Some(row.id);
            state.folder_id = Some(folder_id);
        }

        let lookup = DbSessionInfoLookup::new_with_messaging(
            Arc::new(AppDatabase {
                conn: db.conn.clone(),
            }),
            Arc::new(manager.clone_ref()),
            PathBuf::from("/tmp"),
        );
        let original_message = "continue the fix\nwith exact spacing";
        let outcome = lookup
            .send_message(row.id, "parent-connection", original_message.into())
            .await;
        assert!(outcome.accepted, "message should be accepted: {outcome:?}");
        assert_eq!(outcome.session_id, row.id);

        match commands.recv().await.unwrap() {
            ConnectionCommand::Prompt { blocks, .. } => {
                assert_eq!(
                    blocks,
                    vec![crate::acp::types::PromptInputBlock::Text {
                        text: format_mcp_forwarded_message(Some(214), original_message.into())
                    }]
                );
            }
            _ => panic!("expected prompt command"),
        }
    }

    #[tokio::test]
    async fn send_message_rejects_busy_target_before_delivery() {
        let db = crate::db::test_helpers::fresh_in_memory_db().await;
        let workspace = "/tmp/session-message-busy";
        let folder_id = crate::db::test_helpers::seed_folder(&db, workspace).await;
        let row = conversation_service::create(
            &db.conn,
            folder_id,
            AgentType::ClaudeCode,
            None,
            None,
        )
        .await
        .unwrap();
        let mut active: crate::db::entities::conversation::ActiveModel = row.clone().into();
        active.external_id = Set(Some("agent-session-busy".into()));
        active.update(&db.conn).await.unwrap();

        let manager = ConnectionManager::new();
        let _commands = manager
            .insert_test_connection_live(
                "busy-connection",
                AgentType::ClaudeCode,
                Some(PathBuf::from(workspace)),
                EventEmitter::Noop,
            )
            .await;
        let state = manager.get_state("busy-connection").await.unwrap();
        {
            let mut state = state.write().await;
            state.external_id = Some("agent-session-busy".into());
            state.conversation_id = Some(row.id);
            state.folder_id = Some(folder_id);
            state.turn_in_flight = true;
        }

        let lookup = DbSessionInfoLookup::new_with_messaging(
            Arc::new(AppDatabase {
                conn: db.conn.clone(),
            }),
            Arc::new(manager.clone_ref()),
            PathBuf::from("/tmp"),
        );
        let outcome = lookup.send_message(row.id, "parent", "second turn".into()).await;
        assert!(!outcome.accepted);
        assert_eq!(outcome.error_code.as_deref(), Some("target_busy"));
    }

    #[tokio::test]
    async fn resume_fallback_session_new_is_rejected_by_identity_check() {
        let manager = ConnectionManager::new();
        manager
            .insert_test_connection(
                "fallback-connection",
                AgentType::ClaudeCode,
                Some(PathBuf::from("/tmp/fallback")),
                EventEmitter::Noop,
            )
            .await;
        let state = manager.get_state("fallback-connection").await.unwrap();
        state.write().await.external_id = Some("session/new-id".into());

        let result = DbSessionInfoLookup::validate_connection(
            &manager,
            "fallback-connection",
            214,
            AgentType::ClaudeCode,
            "persisted-session-id",
            "/tmp/fallback",
        )
        .await;
        let outcome = result.expect_err("session/new identity must never be accepted");
        assert_eq!(outcome.error_code.as_deref(), Some("identity_mismatch"));
    }
}
