use std::collections::HashMap;
use std::future::Future;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use sea_orm::DatabaseConnection;
use tokio::sync::{mpsc, Mutex, OwnedRwLockReadGuard, RwLock};

use super::error::ChatChannelError;
use super::session_bridge::SessionBridge;
use super::traits::ChatChannelBackend;
use super::types::*;
use crate::acp::manager::ConnectionManager;
use crate::db::service::{chat_channel_service, thread_binding_service};
use crate::web::event_bridge::{EventEmitter, WebEventBroadcaster};

struct ActiveChannel {
    id: i32,
    name: String,
    channel_type: ChannelType,
    generation: u64,
    backend: Arc<dyn ChatChannelBackend>,
}

/// Inner state shared across clones.
struct Inner {
    channels: Mutex<HashMap<i32, ActiveChannel>>,
    generations: Mutex<HashMap<i32, u64>>,
    command_gates: Mutex<HashMap<i32, Arc<RwLock<()>>>>,
    lifecycle: Mutex<()>,
    command_tx: mpsc::Sender<IncomingCommand>,
    command_rx: Mutex<Option<mpsc::Receiver<IncomingCommand>>>,
    broadcaster: Mutex<Option<Arc<WebEventBroadcaster>>>,
}

pub struct ChatChannelManager {
    inner: Arc<Inner>,
}

impl Default for ChatChannelManager {
    fn default() -> Self {
        Self::new()
    }
}

impl ChatChannelManager {
    pub fn new() -> Self {
        let (command_tx, command_rx) = mpsc::channel(256);
        Self {
            inner: Arc::new(Inner {
                channels: Mutex::new(HashMap::new()),
                generations: Mutex::new(HashMap::new()),
                command_gates: Mutex::new(HashMap::new()),
                lifecycle: Mutex::new(()),
                command_tx,
                command_rx: Mutex::new(Some(command_rx)),
                broadcaster: Mutex::new(None),
            }),
        }
    }

    /// Shallow clone sharing the same state (like ConnectionManager::clone_ref).
    pub fn clone_ref(&self) -> Self {
        Self {
            inner: self.inner.clone(),
        }
    }

    pub fn command_sender(&self) -> mpsc::Sender<IncomingCommand> {
        self.inner.command_tx.clone()
    }

    /// Take the command receiver (can only be called once, at startup).
    pub async fn take_command_receiver(&self) -> Option<mpsc::Receiver<IncomingCommand>> {
        self.inner.command_rx.lock().await.take()
    }

    /// Emit a status change event to the frontend via broadcaster.
    async fn emit_status_event(&self, channel_id: i32, status: &str) {
        if let Some(broadcaster) = self.inner.broadcaster.lock().await.as_ref() {
            broadcaster.send(
                "chat-channel://status",
                &serde_json::json!({
                    "channel_id": channel_id,
                    "status": status,
                }),
            );
        }
    }

    pub async fn add_channel(
        &self,
        id: i32,
        name: String,
        channel_type: ChannelType,
        backend: Box<dyn ChatChannelBackend>,
    ) -> Result<(), ChatChannelError> {
        let gate = self.command_gate(id).await;
        let _commands = gate.write().await;
        let _lifecycle = self.inner.lifecycle.lock().await;
        self.add_channel_locked(id, name, channel_type, backend)
            .await
    }

    pub async fn add_channel_with_factory<F, Fut, E>(&self, id: i32, factory: F) -> Result<(), E>
    where
        F: FnOnce() -> Fut,
        Fut: Future<Output = Result<(i32, String, ChannelType, Box<dyn ChatChannelBackend>), E>>,
        E: From<ChatChannelError>,
    {
        let gate = self.command_gate(id).await;
        let _commands = gate.write().await;
        let _lifecycle = self.inner.lifecycle.lock().await;
        let (actual_id, name, channel_type, backend) = factory().await?;
        if actual_id != id {
            return Err(E::from(ChatChannelError::ConfigurationInvalid(
                "channel factory returned a different channel ID".to_string(),
            )));
        }
        self.add_channel_locked(id, name, channel_type, backend)
            .await
            .map_err(E::from)
    }

    async fn add_channel_locked(
        &self,
        id: i32,
        name: String,
        channel_type: ChannelType,
        backend: Box<dyn ChatChannelBackend>,
    ) -> Result<(), ChatChannelError> {
        let backend: Arc<dyn ChatChannelBackend> = Arc::from(backend);

        // Stop existing channel if present (prevents task leak on duplicate connect)
        let old = self.inner.channels.lock().await.remove(&id);
        if let Some(existing) = old {
            let _ = existing.backend.stop().await;
        }

        let generation = self.next_generation(id).await;
        backend.set_runtime_generation(generation);

        let command_tx = self.inner.command_tx.clone();
        backend.start(command_tx).await?;

        let status_backend = backend.clone();
        let channel = ActiveChannel {
            id,
            name,
            channel_type,
            generation,
            backend,
        };

        self.inner.channels.lock().await.insert(id, channel);
        let status = if channel_type == ChannelType::QqBot {
            "connecting"
        } else {
            "connected"
        };
        self.emit_status_event(id, status).await;
        if channel_type == ChannelType::QqBot {
            drop(self.spawn_status_watcher(id, generation, status_backend));
        }
        Ok(())
    }

    fn spawn_status_watcher(
        &self,
        channel_id: i32,
        generation: u64,
        backend: Arc<dyn ChatChannelBackend>,
    ) -> tokio::task::JoinHandle<()> {
        let manager = self.clone_ref();
        tokio::spawn(async move {
            let mut previous = ChannelConnectionStatus::Connecting;
            loop {
                tokio::time::sleep(Duration::from_millis(250)).await;
                let status = backend.status().await;
                let _lifecycle = manager.inner.lifecycle.lock().await;
                let current = manager.inner.channels.lock().await;
                if !current
                    .get(&channel_id)
                    .is_some_and(|channel| channel.generation == generation)
                {
                    break;
                }
                drop(current);
                if status != previous {
                    previous = status;
                    manager
                        .emit_status_event(channel_id, channel_status_name(status))
                        .await;
                }
            }
        })
    }

    pub async fn remove_channel(&self, id: i32) -> Result<(), ChatChannelError> {
        let gate = self.command_gate(id).await;
        let _commands = gate.write().await;
        let _lifecycle = self.inner.lifecycle.lock().await;
        let removed = self.inner.channels.lock().await.remove(&id);
        self.next_generation(id).await;
        if let Some(channel) = removed {
            channel.backend.stop().await?;
            self.emit_status_event(id, "disconnected").await;
        }
        Ok(())
    }

    pub async fn stop_all(&self) {
        loop {
            let mut ids: Vec<i32> = self.inner.channels.lock().await.keys().copied().collect();
            ids.sort_unstable();
            let mut guards = Vec::with_capacity(ids.len());
            for id in &ids {
                guards.push(self.command_gate(*id).await.write_owned().await);
            }
            let _lifecycle = self.inner.lifecycle.lock().await;
            let mut channels = self.inner.channels.lock().await;
            if channels.keys().any(|id| ids.binary_search(id).is_err()) {
                continue;
            }
            let drained: Vec<ActiveChannel> = channels.drain().map(|(_, ch)| ch).collect();
            drop(channels);
            for channel in drained {
                self.next_generation(channel.id).await;
                let _ = channel.backend.stop().await;
            }
            break;
        }
    }

    /// Immediately stop and remove a backend whose QQBot configuration has
    /// changed. The generation is advanced even when no backend is active so
    /// commands already queued by an older runtime cannot be accepted later.
    pub async fn invalidate_channel(&self, id: i32) -> Result<(), ChatChannelError> {
        let gate = self.command_gate(id).await;
        let _commands = gate.write().await;
        let _lifecycle = self.inner.lifecycle.lock().await;
        self.invalidate_channel_locked(id).await
    }

    pub async fn update_with_lifecycle<C, Check, F, Fut, T, E>(
        &self,
        id: i32,
        check: C,
        update: F,
    ) -> Result<T, E>
    where
        C: FnOnce() -> Check,
        Check: Future<Output = Result<bool, E>>,
        F: FnOnce() -> Fut,
        Fut: Future<Output = Result<T, E>>,
        E: From<ChatChannelError>,
    {
        let gate = self.command_gate(id).await;
        let _commands = gate.write().await;
        let _lifecycle = self.inner.lifecycle.lock().await;
        if check().await? {
            self.invalidate_channel_locked(id).await.map_err(E::from)?;
        }
        update().await
    }

    async fn invalidate_channel_locked(&self, id: i32) -> Result<(), ChatChannelError> {
        let removed = self.inner.channels.lock().await.remove(&id);
        self.next_generation(id).await;
        if let Some(channel) = removed {
            channel.backend.stop().await?;
            self.emit_status_event(id, "disconnected").await;
        }
        Ok(())
    }

    async fn next_generation(&self, id: i32) -> u64 {
        let mut generations = self.inner.generations.lock().await;
        let generation = generations.entry(id).or_insert(0);
        *generation = generation.saturating_add(1);
        *generation
    }

    async fn command_gate(&self, channel_id: i32) -> Arc<RwLock<()>> {
        self.inner
            .command_gates
            .lock()
            .await
            .entry(channel_id)
            .or_insert_with(|| Arc::new(RwLock::new(())))
            .clone()
    }

    /// The read guard admits this generation until all command effects finish.
    /// Lifecycle operations acquire the channel's write guard before lifecycle.
    pub(super) async fn qqbot_execution_lease(
        &self,
        channel_id: i32,
        generation: u64,
    ) -> Option<OwnedRwLockReadGuard<()>> {
        let lease = self.command_gate(channel_id).await.read_owned().await;
        let _lifecycle = self.inner.lifecycle.lock().await;
        let channels = self.inner.channels.lock().await;
        if channels.get(&channel_id).is_some_and(|channel| {
            channel.channel_type == ChannelType::QqBot && channel.generation == generation
        }) {
            Some(lease)
        } else {
            None
        }
    }

    pub async fn is_runtime_current(&self, channel_id: i32, generation: u64) -> bool {
        let _lifecycle = self.inner.lifecycle.lock().await;
        self.inner
            .channels
            .lock()
            .await
            .get(&channel_id)
            .is_some_and(|channel| channel.generation == generation)
    }

    #[cfg(test)]
    pub async fn runtime_generation(&self, channel_id: i32) -> Option<u64> {
        self.inner
            .channels
            .lock()
            .await
            .get(&channel_id)
            .map(|channel| channel.generation)
    }

    /// Keep the runtime identity check, durable binding, and confirmation
    /// within the same lifecycle as channel replacement and config updates.
    pub async fn bind_qqbot_owner_for_runtime(
        &self,
        db: &DatabaseConnection,
        channel_id: i32,
        owner_openid: &str,
        identity: &ChannelRuntimeIdentity,
    ) -> Result<(), ChatChannelError> {
        let _lifecycle = self.inner.lifecycle.lock().await;
        let backend = {
            let channels = self.inner.channels.lock().await;
            let channel = channels
                .get(&channel_id)
                .ok_or(ChatChannelError::NotFound(channel_id))?;
            if channel.generation != identity.generation
                || channel.channel_type != ChannelType::QqBot
            {
                return Err(ChatChannelError::ConnectionFailed(
                    "QQBot runtime was replaced before owner binding".to_string(),
                ));
            }
            channel.backend.clone()
        };
        chat_channel_service::bind_qqbot_owner_for_runtime(
            db,
            channel_id,
            owner_openid,
            &identity.app_id,
            identity.owner_openid.as_deref(),
        )
        .await
        .map_err(|error| ChatChannelError::Other(error.to_string()))?;
        backend.confirm_owner_binding(owner_openid).await
    }

    pub async fn send_to_channel(
        &self,
        channel_id: i32,
        message: &RichMessage,
    ) -> Result<SentMessageId, ChatChannelError> {
        let backend = {
            let channels = self.inner.channels.lock().await;
            channels
                .get(&channel_id)
                .ok_or(ChatChannelError::NotFound(channel_id))?
                .backend
                .clone()
        };
        backend.send_rich_message(message).await
    }

    pub async fn send_to_target(
        &self,
        target: &ChannelMessageTarget,
        message: &RichMessage,
    ) -> Result<SentMessageId, ChatChannelError> {
        let backend = {
            let channels = self.inner.channels.lock().await;
            channels
                .get(&target.channel_id)
                .ok_or(ChatChannelError::NotFound(target.channel_id))?
                .backend
                .clone()
        };
        backend.send_rich_message_to(message, target).await
    }

    pub async fn send_interactive_to_target(
        &self,
        target: &ChannelMessageTarget,
        message: &InteractiveMessage,
    ) -> Result<SentMessageId, ChatChannelError> {
        let backend = {
            let channels = self.inner.channels.lock().await;
            channels
                .get(&target.channel_id)
                .ok_or(ChatChannelError::NotFound(target.channel_id))?
                .backend
                .clone()
        };
        backend.send_interactive_message_to(message, target).await
    }

    pub async fn create_thread(
        &self,
        channel_id: i32,
        title: &str,
    ) -> Result<ChannelMessageTarget, ChatChannelError> {
        let backend = {
            let channels = self.inner.channels.lock().await;
            channels
                .get(&channel_id)
                .ok_or(ChatChannelError::NotFound(channel_id))?
                .backend
                .clone()
        };
        backend.create_thread(title).await
    }

    pub async fn edit_thread_title(
        &self,
        target: &ChannelMessageTarget,
        title: &str,
    ) -> Result<(), ChatChannelError> {
        let backend = {
            let channels = self.inner.channels.lock().await;
            channels
                .get(&target.channel_id)
                .ok_or(ChatChannelError::NotFound(target.channel_id))?
                .backend
                .clone()
        };
        backend.edit_thread_title(target, title).await
    }

    pub async fn sync_conversation_title(
        &self,
        db: &DatabaseConnection,
        conversation_id: i32,
        title: &str,
    ) {
        let title = title.trim();
        if title.is_empty() {
            return;
        }
        let bindings = match thread_binding_service::list_by_conversation(db, conversation_id).await
        {
            Ok(bindings) => bindings,
            Err(e) => {
                tracing::warn!(
                    "[ChatChannel] failed to load thread bindings for conversation {conversation_id}: {e}"
                );
                return;
            }
        };
        let topic_title = topic_title_for_conversation(conversation_id, title);
        for binding in bindings {
            if binding.thread_kind != TELEGRAM_FORUM_THREAD_KIND || !binding.title_sync_enabled {
                continue;
            }
            let target = ChannelMessageTarget {
                channel_id: binding.channel_id,
                chat_id: Some(binding.chat_id.clone()),
                thread_key: Some(binding.thread_key.clone()),
                thread_kind: Some(binding.thread_kind.clone()),
                provider_payload: binding
                    .provider_payload_json
                    .as_deref()
                    .and_then(|json| serde_json::from_str(json).ok()),
            };
            match self.edit_thread_title(&target, &topic_title).await {
                Ok(()) => {
                    let _ = thread_binding_service::update_display_title(
                        db,
                        binding.id,
                        topic_title.clone(),
                    )
                    .await;
                }
                Err(e) => {
                    tracing::warn!(
                        "[ChatChannel] failed to sync Telegram topic title for conversation {conversation_id}: {e}"
                    );
                }
            }
        }
    }

    pub async fn send_to_all(&self, message: &RichMessage) {
        let backends: Vec<Arc<dyn ChatChannelBackend>> = {
            let channels = self.inner.channels.lock().await;
            channels.values().map(|ch| ch.backend.clone()).collect()
        };
        for backend in backends {
            let _ = backend.send_rich_message(message).await;
        }
    }

    pub async fn get_status(&self) -> Vec<crate::models::ChannelStatusInfo> {
        let entries: Vec<(i32, String, String, Arc<dyn ChatChannelBackend>)> = {
            let channels = self.inner.channels.lock().await;
            channels
                .values()
                .map(|ch| {
                    (
                        ch.id,
                        ch.name.clone(),
                        ch.channel_type.to_string(),
                        ch.backend.clone(),
                    )
                })
                .collect()
        };
        let mut result = Vec::with_capacity(entries.len());
        for (id, name, ct, backend) in entries {
            let status = backend.status().await;
            result.push(crate::models::ChannelStatusInfo {
                channel_id: id,
                name,
                channel_type: ct,
                status: serde_json::to_value(status)
                    .ok()
                    .and_then(|v| v.as_str().map(String::from))
                    .unwrap_or_else(|| "unknown".to_string()),
            });
        }
        result
    }

    pub async fn test_channel(&self, id: i32) -> Result<(), ChatChannelError> {
        let backend = {
            let channels = self.inner.channels.lock().await;
            channels
                .get(&id)
                .ok_or(ChatChannelError::NotFound(id))?
                .backend
                .clone()
        };
        backend.test_connection().await
    }

    pub async fn is_connected(&self, id: i32) -> bool {
        let backend = {
            let channels = self.inner.channels.lock().await;
            channels.get(&id).map(|ch| ch.backend.clone())
        };
        if let Some(b) = backend {
            b.status().await == ChannelConnectionStatus::Connected
        } else {
            false
        }
    }

    /// Start background tasks (event subscriber + command dispatcher) and
    /// auto-connect all enabled channels from DB.
    ///
    /// `broadcaster` continues to back the `*Status* / *Inbound*` JSON
    /// events the ChatChannel itself emits (still consumed by the WS
    /// firehose). `bus` carries typed `Arc<EventEnvelope>` to the two
    /// ACP-event-driven subscribers (`event_subscriber`,
    /// `session_event_subscriber`). Phase 5 split: ACP-shaped data goes
    /// through the typed bus; chat-channel-shaped data stays on the JSON
    /// broadcaster.
    pub async fn start_background(
        &self,
        broadcaster: Arc<WebEventBroadcaster>,
        bus: Arc<crate::acp::InternalEventBus>,
        db_conn: DatabaseConnection,
        data_dir: PathBuf,
        conn_mgr: ConnectionManager,
        emitter: EventEmitter,
    ) {
        // Store broadcaster for status event emission
        *self.inner.broadcaster.lock().await = Some(broadcaster.clone());

        let db_conn2 = db_conn.clone();

        // Create shared session bridge
        let bridge = Arc::new(Mutex::new(SessionBridge::new()));

        // Spawn event subscriber
        let manager_for_events = self.clone_ref();
        super::event_subscriber::spawn_event_subscriber(
            bus.clone(),
            manager_for_events,
            db_conn.clone(),
            bridge.clone(),
            conn_mgr.clone_ref(),
        );

        // Spawn session event subscriber (ACP event routing to channels)
        let manager_for_session_events = self.clone_ref();
        super::session_event_subscriber::spawn_session_event_subscriber(
            bus,
            bridge.clone(),
            manager_for_session_events,
            conn_mgr.clone_ref(),
            db_conn.clone(),
            // Cloned rather than moved: the command dispatcher below still
            // takes the original. The subscriber needs one that outlives any
            // individual connection — see the fn's own doc.
            emitter.clone(),
        );

        // Spawn command dispatcher
        if let Some(command_rx) = self.take_command_receiver().await {
            tracing::info!("[ChatChannel] command dispatcher started");
            let manager_for_cmds = self.clone_ref();
            super::command_dispatcher::spawn_command_dispatcher(
                command_rx,
                manager_for_cmds,
                db_conn.clone(),
                data_dir,
                conn_mgr,
                emitter,
                bridge,
            );
        } else {
            tracing::warn!("[ChatChannel] WARNING: command_rx already taken, dispatcher NOT started");
        }

        // Spawn daily report scheduler
        let manager_for_scheduler = self.clone_ref();
        super::scheduler::spawn_daily_report_scheduler(manager_for_scheduler, db_conn.clone());

        // Auto-connect enabled channels
        self.auto_connect_channels(&db_conn2).await;
    }

    async fn auto_connect_channels(&self, db_conn: &DatabaseConnection) {
        let channels = match crate::db::service::chat_channel_service::list_enabled(db_conn).await {
            Ok(c) => c,
            Err(e) => {
                tracing::error!("[ChatChannel] failed to load enabled channels: {e}");
                return;
            }
        };

        for ch in channels {
            let channel_type: ChannelType =
                match serde_json::from_value(serde_json::Value::String(ch.channel_type.clone())) {
                    Ok(t) => t,
                    Err(_) => {
                        tracing::warn!(
                            "[ChatChannel] unknown channel type '{}' for '{}' (id={}), skipping",
                            ch.channel_type, ch.name, ch.id
                        );
                        continue;
                    }
                };

            if channel_type == ChannelType::QqBot {
                let result = self
                    .add_channel_with_factory(ch.id, || async {
                        let current = chat_channel_service::get_by_id(db_conn, ch.id)
                            .await
                            .map_err(|error| ChatChannelError::Other(error.to_string()))?
                            .ok_or(ChatChannelError::NotFound(ch.id))?;
                        if !current.enabled
                            || (current.channel_type != "qqbot" && current.channel_type != "qq_bot")
                        {
                            return Err(ChatChannelError::ConfigurationInvalid(
                                "QQBot channel is no longer enabled".to_string(),
                            ));
                        }
                        let config =
                            serde_json::from_str(&current.config_json).map_err(|error| {
                                ChatChannelError::ConfigurationInvalid(error.to_string())
                            })?;
                        let token =
                            crate::keyring_store::get_channel_token(ch.id).ok_or_else(|| {
                                ChatChannelError::ConfigurationInvalid("Token not set".to_string())
                            })?;
                        let backend = super::backends::create_backend(
                            ch.id,
                            ChannelType::QqBot,
                            &config,
                            token,
                        )?;
                        Ok((ch.id, current.name, ChannelType::QqBot, backend))
                    })
                    .await;
                if let Err(error) = result {
                    tracing::error!(
                        "[ChatChannel] failed to auto-connect QQBot {}: {error}",
                        ch.id
                    );
                }
                continue;
            }

            let config: serde_json::Value = match serde_json::from_str(&ch.config_json) {
                Ok(v) => v,
                Err(e) => {
                    tracing::warn!(
                        "[ChatChannel] invalid config for '{}' (id={}): {e}, skipping",
                        ch.name, ch.id
                    );
                    continue;
                }
            };

            let token = match crate::keyring_store::get_channel_token(ch.id) {
                Some(t) => t,
                None => {
                    tracing::warn!(
                        "[ChatChannel] no token found for '{}' (id={}), skipping auto-connect",
                        ch.name, ch.id
                    );
                    continue;
                }
            };

            let backend = match super::backends::create_backend(ch.id, channel_type, &config, token)
            {
                Ok(b) => b,
                Err(e) => {
                    tracing::error!(
                        "[ChatChannel] failed to create backend for '{}' (id={}): {e}",
                        ch.name, ch.id
                    );
                    continue;
                }
            };

            if let Err(e) = self
                .add_channel(ch.id, ch.name.clone(), channel_type, backend)
                .await
            {
                tracing::error!(
                    "[ChatChannel] failed to auto-connect '{}' (id={}): {e}",
                    ch.name, ch.id
                );
            } else {
                tracing::info!("[ChatChannel] auto-connected '{}' (id={})", ch.name, ch.id);
            }
        }
    }
}

fn topic_title_for_conversation(conversation_id: i32, title: &str) -> String {
    format!("#{conversation_id} {title}").chars().take(128).collect()
}

fn channel_status_name(status: ChannelConnectionStatus) -> &'static str {
    match status {
        ChannelConnectionStatus::Connected => "connected",
        ChannelConnectionStatus::Connecting => "connecting",
        ChannelConnectionStatus::Disconnected => "disconnected",
        ChannelConnectionStatus::Error => "error",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

    struct TestBackend {
        stopped: Arc<AtomicBool>,
        status: ChannelConnectionStatus,
        pause_status: Option<Arc<StatusPause>>,
    }

    struct StatusPause {
        calls: AtomicUsize,
        entered: tokio::sync::Notify,
        resume: tokio::sync::Notify,
    }

    #[async_trait]
    impl ChatChannelBackend for TestBackend {
        fn channel_type(&self) -> ChannelType {
            ChannelType::QqBot
        }

        async fn start(
            &self,
            _command_tx: mpsc::Sender<IncomingCommand>,
        ) -> Result<(), ChatChannelError> {
            Ok(())
        }

        async fn stop(&self) -> Result<(), ChatChannelError> {
            self.stopped.store(true, Ordering::Release);
            Ok(())
        }

        async fn status(&self) -> ChannelConnectionStatus {
            if let Some(pause) = &self.pause_status {
                if pause.calls.fetch_add(1, Ordering::Relaxed) == 0 {
                    return ChannelConnectionStatus::Connecting;
                }
                pause.entered.notify_one();
                pause.resume.notified().await;
            }
            self.status
        }

        async fn send_message(&self, _text: &str) -> Result<SentMessageId, ChatChannelError> {
            Ok(SentMessageId("test".to_string()))
        }

        async fn send_rich_message(
            &self,
            _message: &RichMessage,
        ) -> Result<SentMessageId, ChatChannelError> {
            Ok(SentMessageId("test".to_string()))
        }

        async fn test_connection(&self) -> Result<(), ChatChannelError> {
            Ok(())
        }
    }

    #[tokio::test]
    async fn invalidating_connected_channel_advances_runtime_generation() {
        let manager = ChatChannelManager::new();
        let first_stopped = Arc::new(AtomicBool::new(false));
        manager
            .add_channel(
                7,
                "QQBot".to_string(),
                ChannelType::QqBot,
                Box::new(TestBackend {
                    stopped: first_stopped.clone(),
                    status: ChannelConnectionStatus::Connected,
                    pause_status: None,
                }),
            )
            .await
            .expect("add first backend");
        let first_generation = manager.runtime_generation(7).await.expect("generation");

        manager.invalidate_channel(7).await.expect("invalidate");
        assert!(first_stopped.load(Ordering::Acquire));
        assert!(!manager.is_runtime_current(7, first_generation).await);

        manager
            .add_channel(
                7,
                "QQBot".to_string(),
                ChannelType::QqBot,
                Box::new(TestBackend {
                    stopped: Arc::new(AtomicBool::new(false)),
                    status: ChannelConnectionStatus::Connected,
                    pause_status: None,
                }),
            )
            .await
            .expect("add replacement backend");
        let replacement_generation = manager.runtime_generation(7).await.expect("generation");
        assert!(replacement_generation > first_generation);
        assert!(manager.is_runtime_current(7, replacement_generation).await);
        assert!(!manager.is_runtime_current(7, first_generation).await);
    }

    #[tokio::test]
    async fn qqbot_admitted_command_delays_only_its_channel_reset() {
        let manager = ChatChannelManager::new();
        let stopped = Arc::new(AtomicBool::new(false));
        manager
            .add_channel(
                7,
                "QQBot".to_string(),
                ChannelType::QqBot,
                Box::new(TestBackend {
                    stopped: stopped.clone(),
                    status: ChannelConnectionStatus::Connected,
                    pause_status: None,
                }),
            )
            .await
            .unwrap();
        let generation = manager.runtime_generation(7).await.unwrap();
        let lease = manager.qqbot_execution_lease(7, generation).await.unwrap();
        let gate = manager.command_gate(7).await;
        let reset_manager = manager.clone_ref();
        let reset = tokio::spawn(async move {
            reset_manager
                .update_with_lifecycle(
                    7,
                    || async { Ok::<_, ChatChannelError>(true) },
                    || async { Ok::<_, ChatChannelError>(()) },
                )
                .await
        });

        tokio::time::timeout(Duration::from_secs(2), async {
            while gate.try_read().is_ok() {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("reset waiting for admitted command");
        assert!(!stopped.load(Ordering::Acquire));
        tokio::time::timeout(
            Duration::from_secs(2),
            manager.add_channel(
                8,
                "Other QQBot".to_string(),
                ChannelType::QqBot,
                Box::new(TestBackend {
                    stopped: Arc::new(AtomicBool::new(false)),
                    status: ChannelConnectionStatus::Connected,
                    pause_status: None,
                }),
            ),
        )
        .await
        .expect("other channel must not wait for reset")
        .unwrap();

        drop(lease);
        reset.await.unwrap().expect("reset runtime");
        assert!(stopped.load(Ordering::Acquire));
        assert!(manager.qqbot_execution_lease(7, generation).await.is_none());
        assert!(manager.runtime_generation(8).await.is_some());
    }

    #[tokio::test]
    async fn qqbot_stop_all_waits_for_admitted_commands() {
        let manager = ChatChannelManager::new();
        let stopped = Arc::new(AtomicBool::new(false));
        manager
            .add_channel(
                7,
                "QQBot".to_string(),
                ChannelType::QqBot,
                Box::new(TestBackend {
                    stopped: stopped.clone(),
                    status: ChannelConnectionStatus::Connected,
                    pause_status: None,
                }),
            )
            .await
            .unwrap();
        let generation = manager.runtime_generation(7).await.unwrap();
        let lease = manager.qqbot_execution_lease(7, generation).await.unwrap();
        let gate = manager.command_gate(7).await;
        let stop_manager = manager.clone_ref();
        let stop = tokio::spawn(async move { stop_manager.stop_all().await });
        tokio::time::timeout(Duration::from_secs(2), async {
            while gate.try_read().is_ok() {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("shutdown waiting for admitted command");
        assert!(!stopped.load(Ordering::Acquire));

        drop(lease);
        stop.await.expect("stop all");
        assert!(stopped.load(Ordering::Acquire));
        assert!(manager.qqbot_execution_lease(7, generation).await.is_none());
    }

    #[tokio::test(start_paused = true)]
    async fn qqbot_watcher_emits_connected_when_backend_is_already_ready() {
        let manager = ChatChannelManager::new();
        let broadcaster = Arc::new(WebEventBroadcaster::new());
        let mut events = broadcaster.subscribe();
        *manager.inner.broadcaster.lock().await = Some(broadcaster);
        manager
            .add_channel(
                7,
                "QQBot".to_string(),
                ChannelType::QqBot,
                Box::new(TestBackend {
                    stopped: Arc::new(AtomicBool::new(false)),
                    status: ChannelConnectionStatus::Connected,
                    pause_status: None,
                }),
            )
            .await
            .expect("connect QQBot");
        assert_eq!(events.recv().await.unwrap().payload["status"], "connecting");

        tokio::task::yield_now().await;
        tokio::time::advance(Duration::from_millis(250)).await;
        let event = tokio::time::timeout(Duration::from_secs(1), events.recv())
            .await
            .expect("connected event timeout")
            .expect("connected event");
        assert_eq!(event.payload["channel_id"], 7);
        assert_eq!(event.payload["status"], "connected");
    }

    #[tokio::test(start_paused = true)]
    async fn qqbot_watcher_drops_status_sample_from_replaced_runtime() {
        let manager = ChatChannelManager::new();
        let broadcaster = Arc::new(WebEventBroadcaster::new());
        let mut events = broadcaster.subscribe();
        *manager.inner.broadcaster.lock().await = Some(broadcaster);
        manager
            .add_channel(
                7,
                "QQBot".to_string(),
                ChannelType::QqBot,
                Box::new(TestBackend {
                    stopped: Arc::new(AtomicBool::new(false)),
                    status: ChannelConnectionStatus::Connecting,
                    pause_status: None,
                }),
            )
            .await
            .expect("connect QQBot");
        assert_eq!(events.recv().await.unwrap().payload["status"], "connecting");
        let generation = manager.runtime_generation(7).await.unwrap();
        let pause = Arc::new(StatusPause {
            calls: AtomicUsize::new(0),
            entered: tokio::sync::Notify::new(),
            resume: tokio::sync::Notify::new(),
        });
        let watcher = manager.spawn_status_watcher(
            7,
            generation,
            Arc::new(TestBackend {
                stopped: Arc::new(AtomicBool::new(false)),
                status: ChannelConnectionStatus::Connected,
                pause_status: Some(pause.clone()),
            }),
        );

        tokio::task::yield_now().await;
        tokio::time::advance(Duration::from_millis(250)).await;
        tokio::task::yield_now().await;
        tokio::time::advance(Duration::from_millis(250)).await;
        pause.entered.notified().await;
        manager
            .invalidate_channel(7)
            .await
            .expect("invalidate runtime");
        assert_eq!(
            events.recv().await.unwrap().payload["status"],
            "disconnected"
        );
        pause.resume.notify_one();
        watcher.await.expect("watcher task");
        assert!(events.try_recv().is_err());
    }
}
