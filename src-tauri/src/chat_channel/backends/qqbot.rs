use std::collections::{HashSet, VecDeque};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use async_trait::async_trait;
use futures_util::{SinkExt, StreamExt};
use reqwest::StatusCode;
use serde::Deserialize;
use tokio::sync::{mpsc, Mutex, RwLock};
use tokio_tungstenite::{connect_async, tungstenite::Message};

use crate::chat_channel::error::ChatChannelError;
use crate::chat_channel::traits::ChatChannelBackend;
use crate::chat_channel::types::*;

const QQBOT_TOKEN_URL: &str = "https://api.bot.qq.com/app/getAppAccessToken";
const QQBOT_GATEWAY_URL: &str = "https://api.sgroup.qq.com/gateway";
const QQBOT_API_BASE_URL: &str = "https://api.sgroup.qq.com";
const QQBOT_C2C_INTENT: u64 = 1 << 25;
const TOKEN_REFRESH_MARGIN_SECS: u64 = 300;
const MAX_SEEN_MESSAGE_IDS: usize = 2048;
const QQBOT_RATE_LIMIT_RETRY_SECS: u64 = 60;

#[derive(Clone)]
struct QqBotEndpoints {
    token_url: String,
    gateway_url: String,
    api_base_url: String,
}

impl Default for QqBotEndpoints {
    fn default() -> Self {
        Self {
            token_url: QQBOT_TOKEN_URL.to_string(),
            gateway_url: QQBOT_GATEWAY_URL.to_string(),
            api_base_url: QQBOT_API_BASE_URL.to_string(),
        }
    }
}

#[derive(Clone)]
struct AccessToken {
    value: String,
    expires_at: Instant,
}

#[derive(Deserialize)]
struct AccessTokenResponse {
    access_token: Option<String>,
    expires_in: Option<AccessTokenExpiresIn>,
    code: Option<i64>,
    message: Option<String>,
}

#[derive(Deserialize)]
#[serde(untagged)]
enum AccessTokenExpiresIn {
    Number(u64),
    String(String),
}

impl AccessTokenExpiresIn {
    fn into_seconds(self) -> Result<u64, ChatChannelError> {
        match self {
            Self::Number(value) => Ok(value),
            Self::String(value) => value.trim().parse::<u64>().map_err(|_| {
                ChatChannelError::AuthenticationFailed(
                    "QQBot access-token response had an invalid expires_in".to_string(),
                )
            }),
        }
    }
}

#[derive(Deserialize)]
struct GatewayResponse {
    url: Option<String>,
}

#[derive(Default)]
struct SeenMessageIds {
    set: HashSet<String>,
    order: VecDeque<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct GatewaySession {
    session_id: Option<String>,
    sequence: Option<i64>,
    resumable: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum GatewayClose {
    Shutdown,
    Reconnect { retry_after: Option<Duration> },
    Fatal { code: u16 },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct GatewayCloseAction {
    retry_after: Option<Duration>,
    clear_session: bool,
    refresh_token: bool,
    fatal: bool,
}

fn classify_gateway_close(code: Option<u16>) -> GatewayCloseAction {
    match code {
        Some(4914 | 4915) => GatewayCloseAction {
            retry_after: None,
            clear_session: false,
            refresh_token: false,
            fatal: true,
        },
        Some(4006 | 4007 | 4009) | Some(4900..=4913) => GatewayCloseAction {
            retry_after: None,
            clear_session: true,
            refresh_token: true,
            fatal: false,
        },
        Some(4004) => GatewayCloseAction {
            retry_after: None,
            clear_session: false,
            refresh_token: true,
            fatal: false,
        },
        Some(4008) => GatewayCloseAction {
            retry_after: Some(Duration::from_secs(QQBOT_RATE_LIMIT_RETRY_SECS)),
            clear_session: false,
            refresh_token: false,
            fatal: false,
        },
        _ => GatewayCloseAction {
            retry_after: None,
            clear_session: false,
            refresh_token: false,
            fatal: false,
        },
    }
}

fn retry_delay(retry_count: u32, retry_after: Option<Duration>) -> Duration {
    retry_after.unwrap_or_else(|| Duration::from_secs(2u64.saturating_pow(retry_count.min(5))))
}

impl GatewaySession {
    fn clear(&mut self) {
        self.session_id = None;
        self.sequence = None;
        self.resumable = false;
    }

    fn can_resume(&self) -> bool {
        self.resumable && self.session_id.is_some() && self.sequence.is_some()
    }

    fn mark_ready(&mut self, session_id: &str) {
        self.session_id = Some(session_id.to_string());
        self.resumable = true;
    }
}

fn gateway_handshake(access_token: &str, session: &GatewaySession) -> serde_json::Value {
    if session.can_resume() {
        serde_json::json!({
            "op": 6,
            "d": {
                "token": format!("QQBot {access_token}"),
                "session_id": session.session_id,
                "seq": session.sequence,
            }
        })
    } else {
        serde_json::json!({
            "op": 2,
            "d": {
                "token": format!("QQBot {access_token}"),
                "intents": QQBOT_C2C_INTENT,
                "shard": [0, 1],
                "properties": {
                    "$os": "codeg",
                    "$browser": "codeg",
                    "$device": "codeg"
                }
            }
        })
    }
}

impl SeenMessageIds {
    fn already_seen(&mut self, id: &str) -> bool {
        if id.is_empty() {
            return false;
        }
        if self.set.contains(id) {
            return true;
        }
        let id = id.to_string();
        self.set.insert(id.clone());
        self.order.push_back(id);
        while self.order.len() > MAX_SEEN_MESSAGE_IDS {
            if let Some(old) = self.order.pop_front() {
                self.set.remove(&old);
            }
        }
        false
    }
}

/// Official QQBot self-use C2C backend.
///
/// The keyring token passed by the channel manager is the AppSecret. The
/// bot-scoped OpenID is learned from an inbound C2C event but is not
/// authorized for outbound messages until the command dispatcher persists it
/// in the channel config and confirms it back to this backend.
#[derive(Clone)]
pub struct QqBotBackend {
    channel_id: i32,
    app_id: String,
    app_secret: String,
    client: reqwest::Client,
    endpoints: QqBotEndpoints,
    token_cache: Arc<RwLock<Option<AccessToken>>>,
    owner_openid: Arc<Mutex<Option<String>>>,
    status: Arc<Mutex<ChannelConnectionStatus>>,
    shutdown_tx: Arc<Mutex<Option<tokio::sync::watch::Sender<bool>>>>,
    runtime_generation: Arc<AtomicU64>,
    runtime_managed: Arc<AtomicBool>,
    gateway_ready: Arc<AtomicBool>,
}

impl QqBotBackend {
    pub fn new(
        channel_id: i32,
        app_id: String,
        app_secret: String,
        owner_openid: Option<String>,
    ) -> Self {
        Self::new_with_endpoints(
            channel_id,
            app_id,
            app_secret,
            owner_openid,
            QqBotEndpoints::default(),
        )
    }

    fn new_with_endpoints(
        channel_id: i32,
        app_id: String,
        app_secret: String,
        owner_openid: Option<String>,
        endpoints: QqBotEndpoints,
    ) -> Self {
        Self {
            channel_id,
            app_id: app_id.trim().to_string(),
            app_secret,
            client: reqwest::Client::builder()
                .connect_timeout(Duration::from_secs(10))
                .timeout(Duration::from_secs(30))
                .build()
                .unwrap_or_default(),
            endpoints,
            token_cache: Arc::new(RwLock::new(None)),
            owner_openid: Arc::new(Mutex::new(normalize_owner(owner_openid))),
            status: Arc::new(Mutex::new(ChannelConnectionStatus::Disconnected)),
            shutdown_tx: Arc::new(Mutex::new(None)),
            runtime_generation: Arc::new(AtomicU64::new(0)),
            runtime_managed: Arc::new(AtomicBool::new(false)),
            gateway_ready: Arc::new(AtomicBool::new(false)),
        }
    }

    async fn access_token(&self) -> Result<String, ChatChannelError> {
        {
            let cache = self.token_cache.read().await;
            if let Some(cached) = cache.as_ref() {
                if cached.expires_at > Instant::now() {
                    return Ok(cached.value.clone());
                }
            }
        }

        let response = self
            .client
            .post(&self.endpoints.token_url)
            .json(&serde_json::json!({
                "appId": self.app_id,
                "clientSecret": self.app_secret,
            }))
            .send()
            .await
            .map_err(|e| {
                ChatChannelError::AuthenticationFailed(redact_secrets(
                    e.to_string(),
                    [&self.app_secret],
                ))
            })?;

        let status = response.status();
        let body = response.text().await.map_err(|e| {
            ChatChannelError::AuthenticationFailed(redact_secrets(
                e.to_string(),
                [&self.app_secret],
            ))
        })?;
        if !status.is_success() {
            return Err(ChatChannelError::AuthenticationFailed(
                format_platform_error(
                    status,
                    &body,
                    "QQBot access-token request",
                    [&self.app_secret],
                ),
            ));
        }

        let parsed: AccessTokenResponse = serde_json::from_str(&body).map_err(|e| {
            ChatChannelError::AuthenticationFailed(format!(
                "QQBot access-token response was invalid: {}",
                redact_secrets(e.to_string(), [&self.app_secret])
            ))
        })?;
        if let Some(code) = parsed.code.filter(|code| *code != 0) {
            let message = redact_secrets(
                parsed
                    .message
                    .as_deref()
                    .unwrap_or("unknown error")
                    .to_string(),
                [&self.app_secret],
            );
            return Err(ChatChannelError::AuthenticationFailed(format!(
                "QQBot access-token request failed: code {code}: {message}"
            )));
        }
        let value = parsed
            .access_token
            .filter(|token| !token.trim().is_empty())
            .ok_or_else(|| {
                ChatChannelError::AuthenticationFailed(
                    "QQBot access-token response did not include access_token".to_string(),
                )
            })?;
        let expires_in = match parsed.expires_in {
            Some(value) => value.into_seconds()?,
            None => 7200,
        };
        let expires_at = Instant::now()
            + Duration::from_secs(expires_in.saturating_sub(TOKEN_REFRESH_MARGIN_SECS).max(1));
        *self.token_cache.write().await = Some(AccessToken {
            value: value.clone(),
            expires_at,
        });
        Ok(value)
    }

    async fn invalidate_access_token(&self) {
        *self.token_cache.write().await = None;
    }

    async fn gateway(&self, access_token: &str) -> Result<String, ChatChannelError> {
        let response = self
            .client
            .get(&self.endpoints.gateway_url)
            .header(
                reqwest::header::AUTHORIZATION,
                format!("QQBot {access_token}"),
            )
            .send()
            .await
            .map_err(|e| {
                ChatChannelError::ConnectionFailed(redact_secrets(
                    e.to_string(),
                    [&self.app_secret, access_token],
                ))
            })?;
        let status = response.status();
        let body = response.text().await.map_err(|e| {
            ChatChannelError::ConnectionFailed(redact_secrets(
                e.to_string(),
                [&self.app_secret, access_token],
            ))
        })?;
        if status == StatusCode::UNAUTHORIZED {
            self.invalidate_access_token().await;
        }
        if !status.is_success() {
            return Err(ChatChannelError::ConnectionFailed(format_platform_error(
                status,
                &body,
                "QQBot gateway request",
                [&self.app_secret, access_token],
            )));
        }
        let parsed: GatewayResponse = serde_json::from_str(&body).map_err(|e| {
            ChatChannelError::ConnectionFailed(format!(
                "QQBot gateway response was invalid: {}",
                redact_secrets(e.to_string(), [&self.app_secret, access_token])
            ))
        })?;
        parsed
            .url
            .filter(|url| url.starts_with("ws://") || url.starts_with("wss://"))
            .ok_or_else(|| {
                ChatChannelError::ConnectionFailed(
                    "QQBot gateway response did not include a WebSocket URL".to_string(),
                )
            })
    }

    async fn owner(&self) -> Option<String> {
        self.owner_openid.lock().await.clone()
    }

    /// Accept only the persisted owner once one exists. Before the first
    /// durable bind, queue text from a candidate sender but do not authorize
    /// it for outbound messages; the dispatcher confirms the DB write first.
    async fn accept_owner(&self, sender_id: &str) -> bool {
        let Some(sender_id) = normalize_owner(Some(sender_id.to_string())) else {
            return false;
        };
        let owner = self.owner_openid.lock().await;
        match owner.as_deref() {
            Some(current) => current == sender_id,
            None => true,
        }
    }

    async fn confirm_owner(&self, owner_openid: &str) -> Result<(), ChatChannelError> {
        let owner_openid = normalize_owner(Some(owner_openid.to_string())).ok_or_else(|| {
            ChatChannelError::ConfigurationInvalid("QQBot owner OpenID cannot be empty".to_string())
        })?;
        let mut owner = self.owner_openid.lock().await;
        // The database check happens before this method. Overwrite a stale
        // backend copy so a duplicate/previous connection cannot keep routing
        // messages to an obsolete OpenID.
        *owner = Some(owner_openid);
        Ok(())
    }

    async fn apply_gateway_close(
        &self,
        code: Option<u16>,
        session: &mut GatewaySession,
        ready: &mut bool,
    ) -> GatewayClose {
        let action = classify_gateway_close(code);
        *ready = false;
        self.gateway_ready.store(false, Ordering::Release);
        if action.fatal {
            return GatewayClose::Fatal {
                code: code.unwrap_or_default(),
            };
        }
        if action.clear_session {
            session.clear();
        }
        if action.refresh_token {
            self.invalidate_access_token().await;
        }
        *self.status.lock().await = ChannelConnectionStatus::Connecting;
        GatewayClose::Reconnect {
            retry_after: action.retry_after,
        }
    }

    async fn ensure_gateway_ready_for_send(&self) -> Result<(), ChatChannelError> {
        if !self.runtime_managed.load(Ordering::Acquire) {
            return Ok(());
        }
        if !self.gateway_ready.load(Ordering::Acquire)
            || *self.status.lock().await != ChannelConnectionStatus::Connected
        {
            return Err(ChatChannelError::SendFailed(
                "QQBot gateway is reconnecting; message was not sent".to_string(),
            ));
        }
        Ok(())
    }

    async fn send_to_owner(&self, text: &str) -> Result<SentMessageId, ChatChannelError> {
        let owner = self.owner().await.ok_or_else(|| {
            ChatChannelError::ConfigurationInvalid(
                "QQBot owner is not bound; send a private message to the bot first".to_string(),
            )
        })?;
        self.send_c2c(&owner, text).await
    }

    async fn send_c2c(
        &self,
        owner_openid: &str,
        text: &str,
    ) -> Result<SentMessageId, ChatChannelError> {
        self.ensure_gateway_ready_for_send().await?;
        let first_token = self.access_token().await?;
        match self
            .send_c2c_with_token(&first_token, owner_openid, text)
            .await
        {
            Ok(id) => Ok(id),
            Err(error) if is_retryable_auth_error(&error) => {
                // A gateway or HTTP send can invalidate a token before its
                // advertised expiry. Refresh once, then report the real error.
                self.invalidate_access_token().await;
                let refreshed = self.access_token().await?;
                self.send_c2c_with_token(&refreshed, owner_openid, text)
                    .await
            }
            Err(error) => Err(error),
        }
    }

    async fn send_c2c_with_token(
        &self,
        access_token: &str,
        owner_openid: &str,
        text: &str,
    ) -> Result<SentMessageId, ChatChannelError> {
        let url = format!(
            "{}/v2/users/{}/messages",
            self.endpoints.api_base_url.trim_end_matches('/'),
            urlencoding::encode(owner_openid)
        );
        // Omitting msg_id is intentional: notifications are ordinary
        // proactive C2C messages and must not reuse an expired inbound id.
        let body = serde_json::json!({
            "content": text,
            "msg_type": 0,
        });
        let response = self
            .client
            .post(url)
            .header(
                reqwest::header::AUTHORIZATION,
                format!("QQBot {access_token}"),
            )
            .json(&body)
            .send()
            .await
            .map_err(|e| {
                ChatChannelError::SendFailed(redact_secrets(
                    e.to_string(),
                    [&self.app_secret, access_token],
                ))
            })?;
        let status = response.status();
        let response_body = response.text().await.map_err(|e| {
            ChatChannelError::SendFailed(redact_secrets(
                e.to_string(),
                [&self.app_secret, access_token],
            ))
        })?;
        if status == StatusCode::UNAUTHORIZED {
            self.invalidate_access_token().await;
        }
        if !status.is_success() {
            return Err(classify_send_failure(
                status,
                &response_body,
                [&self.app_secret, access_token],
            ));
        }

        parse_send_response(status, &response_body, [&self.app_secret, access_token])
    }

    async fn run_receiver(
        &self,
        command_tx: mpsc::Sender<IncomingCommand>,
        mut shutdown_rx: tokio::sync::watch::Receiver<bool>,
    ) {
        let mut retry_count = 0u32;
        let mut seen_ids = SeenMessageIds::default();
        let mut session = GatewaySession::default();

        loop {
            if *shutdown_rx.borrow() {
                break;
            }

            let access_token = match self.access_token().await {
                Ok(token) => token,
                Err(error) => {
                    tracing::error!("[QQBot] access-token refresh failed: {error}");
                    *self.status.lock().await = ChannelConnectionStatus::Error;
                    if wait_for_retry(&mut shutdown_rx, retry_count, None).await {
                        break;
                    }
                    retry_count = retry_count.saturating_add(1);
                    continue;
                }
            };
            let gateway = match self.gateway(&access_token).await {
                Ok(url) => url,
                Err(error) => {
                    tracing::error!("[QQBot] gateway lookup failed: {error}");
                    *self.status.lock().await = ChannelConnectionStatus::Error;
                    if wait_for_retry(&mut shutdown_rx, retry_count, None).await {
                        break;
                    }
                    retry_count = retry_count.saturating_add(1);
                    continue;
                }
            };

            let ws = match connect_async(&gateway).await {
                Ok((stream, _)) => stream,
                Err(error) => {
                    tracing::error!("[QQBot] WebSocket connect failed: {error}");
                    *self.status.lock().await = ChannelConnectionStatus::Error;
                    if wait_for_retry(&mut shutdown_rx, retry_count, None).await {
                        break;
                    }
                    retry_count = retry_count.saturating_add(1);
                    continue;
                }
            };

            self.gateway_ready.store(false, Ordering::Release);
            *self.status.lock().await = ChannelConnectionStatus::Connecting;
            tracing::info!(
                "[QQBot] WebSocket transport connected for channel {}",
                self.channel_id
            );

            let result = self
                .run_websocket(
                    ws,
                    &access_token,
                    &command_tx,
                    &mut seen_ids,
                    &mut session,
                    &mut shutdown_rx,
                    &mut retry_count,
                )
                .await;
            let retry_after = match &result {
                Ok(GatewayClose::Reconnect { retry_after, .. }) => *retry_after,
                _ => None,
            };
            match result {
                Ok(GatewayClose::Shutdown) => break,
                Ok(GatewayClose::Reconnect { .. }) => {}
                Ok(GatewayClose::Fatal { code }) => {
                    tracing::error!("[QQBot] fatal gateway close code {code}; stopping retries");
                    *self.status.lock().await = ChannelConnectionStatus::Error;
                    break;
                }
                Err(error) => {
                    tracing::warn!("[QQBot] WebSocket closed: {error}");
                    self.gateway_ready.store(false, Ordering::Release);
                    *self.status.lock().await = ChannelConnectionStatus::Error;
                    if matches!(error, ChatChannelError::AuthenticationFailed(_)) {
                        session.clear();
                        self.invalidate_access_token().await;
                    }
                }
            }
            if *shutdown_rx.borrow() {
                break;
            }
            *self.status.lock().await = ChannelConnectionStatus::Connecting;
            if wait_for_retry(&mut shutdown_rx, retry_count, retry_after).await {
                break;
            }
            retry_count = retry_count.saturating_add(1);
        }

        self.gateway_ready.store(false, Ordering::Release);
        if *self.status.lock().await != ChannelConnectionStatus::Error {
            *self.status.lock().await = ChannelConnectionStatus::Disconnected;
        }
    }

    #[allow(clippy::too_many_arguments)]
    async fn run_websocket<S>(
        &self,
        stream: tokio_tungstenite::WebSocketStream<S>,
        access_token: &str,
        command_tx: &mpsc::Sender<IncomingCommand>,
        seen_ids: &mut SeenMessageIds,
        session: &mut GatewaySession,
        shutdown_rx: &mut tokio::sync::watch::Receiver<bool>,
        retry_count: &mut u32,
    ) -> Result<GatewayClose, ChatChannelError>
    where
        S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
    {
        let (mut write, mut read) = stream.split();
        let mut heartbeat = tokio::time::interval(Duration::from_secs(3600));
        heartbeat.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        let mut handshake_sent = false;
        let mut ready = false;

        loop {
            tokio::select! {
                _ = heartbeat.tick() => {
                    if handshake_sent {
                        write.send(Message::Text(serde_json::json!({
                            "op": 1,
                            "d": session.sequence,
                        }).to_string().into())).await.map_err(|e| ChatChannelError::ConnectionFailed(e.to_string()))?;
                    }
                }
                _ = shutdown_rx.changed() => {
                    let _ = write.close().await;
                    return Ok(GatewayClose::Shutdown);
                }
                message = read.next() => {
                    let Some(message) = message else {
                        return Err(ChatChannelError::ConnectionFailed("QQBot WebSocket ended".into()));
                    };
                    match message {
                        Ok(Message::Text(text)) => {
                            if let Some(close) = self.handle_gateway_payload(text.as_bytes(), access_token, &mut write, command_tx, seen_ids, session, &mut heartbeat, &mut handshake_sent, &mut ready, retry_count).await? {
                                return Ok(close);
                            }
                        }
                        Ok(Message::Binary(bytes)) => {
                            if let Some(close) = self.handle_gateway_payload(&bytes, access_token, &mut write, command_tx, seen_ids, session, &mut heartbeat, &mut handshake_sent, &mut ready, retry_count).await? {
                                return Ok(close);
                            }
                        }
                        Ok(Message::Ping(data)) => {
                            write.send(Message::Pong(data)).await.map_err(|e| ChatChannelError::ConnectionFailed(e.to_string()))?;
                        }
                        Ok(Message::Close(frame)) => {
                            let code = frame.as_ref().map(|frame| u16::from(&frame.code));
                            return Ok(self.apply_gateway_close(code, session, &mut ready).await);
                        }
                        Ok(Message::Pong(_)) => {}
                        Ok(Message::Frame(_)) => {}
                        Err(error) => return Err(ChatChannelError::ConnectionFailed(error.to_string())),
                    }
                }
            }
        }
    }

    #[allow(clippy::too_many_arguments)]
    async fn handle_gateway_payload<W>(
        &self,
        bytes: &[u8],
        access_token: &str,
        write: &mut W,
        command_tx: &mpsc::Sender<IncomingCommand>,
        seen_ids: &mut SeenMessageIds,
        session: &mut GatewaySession,
        heartbeat: &mut tokio::time::Interval,
        handshake_sent: &mut bool,
        ready: &mut bool,
        retry_count: &mut u32,
    ) -> Result<Option<GatewayClose>, ChatChannelError>
    where
        W: futures_util::Sink<Message> + Unpin,
        W::Error: std::fmt::Display,
    {
        let payload: serde_json::Value = serde_json::from_slice(bytes).map_err(|e| {
            ChatChannelError::ConnectionFailed(format!("QQBot gateway payload was invalid: {e}"))
        })?;
        if let Some(seq_value) = payload.get("s").and_then(serde_json::Value::as_i64) {
            session.sequence = Some(seq_value);
        }

        match payload.get("op").and_then(serde_json::Value::as_i64) {
            Some(10) => {
                let interval = payload
                    .pointer("/d/heartbeat_interval")
                    .and_then(serde_json::Value::as_u64)
                    .unwrap_or(45_000);
                let handshake = gateway_handshake(access_token, session);
                write
                    .send(Message::Text(handshake.to_string().into()))
                    .await
                    .map_err(|e| ChatChannelError::ConnectionFailed(e.to_string()))?;
                *heartbeat = tokio::time::interval(Duration::from_millis(interval));
                heartbeat.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
                *handshake_sent = true;
            }
            Some(0) => match payload.get("t").and_then(serde_json::Value::as_str) {
                Some("READY") => {
                    if let Some(session_id) = payload
                        .pointer("/d/session_id")
                        .and_then(serde_json::Value::as_str)
                    {
                        session.mark_ready(session_id);
                        *ready = true;
                        *retry_count = 0;
                        self.gateway_ready.store(true, Ordering::Release);
                        *self.status.lock().await = ChannelConnectionStatus::Connected;
                    }
                }
                Some("RESUMED") => {
                    *ready = true;
                    *retry_count = 0;
                    session.resumable = session.session_id.is_some();
                    self.gateway_ready.store(true, Ordering::Release);
                    *self.status.lock().await = ChannelConnectionStatus::Connected;
                }
                Some("C2C_MESSAGE_CREATE") if *ready => {
                    handle_qqbot_event(
                        payload.get("d").unwrap_or(&serde_json::Value::Null),
                        self,
                        command_tx,
                        seen_ids,
                    )
                    .await;
                }
                _ => {}
            },
            Some(1) => {
                write
                    .send(Message::Text(
                        serde_json::json!({ "op": 1, "d": session.sequence })
                            .to_string()
                            .into(),
                    ))
                    .await
                    .map_err(|e| ChatChannelError::ConnectionFailed(e.to_string()))?;
            }
            Some(7) => {
                return Ok(Some(self.apply_gateway_close(None, session, ready).await));
            }
            Some(9) => {
                let resumable = payload
                    .pointer("/d")
                    .and_then(serde_json::Value::as_bool)
                    .unwrap_or(false);
                if !resumable {
                    session.clear();
                }
                return Ok(Some(self.apply_gateway_close(None, session, ready).await));
            }
            _ => {}
        }
        Ok(None)
    }
}

#[async_trait]
impl ChatChannelBackend for QqBotBackend {
    fn channel_type(&self) -> ChannelType {
        ChannelType::QqBot
    }

    fn set_runtime_generation(&self, generation: u64) {
        self.runtime_generation.store(generation, Ordering::Release);
    }

    async fn start(
        &self,
        command_tx: mpsc::Sender<IncomingCommand>,
    ) -> Result<(), ChatChannelError> {
        self.runtime_managed.store(false, Ordering::Release);
        self.gateway_ready.store(false, Ordering::Release);
        *self.status.lock().await = ChannelConnectionStatus::Connecting;
        let access_token = match self.access_token().await {
            Ok(token) => token,
            Err(error) => {
                *self.status.lock().await = ChannelConnectionStatus::Error;
                return Err(error);
            }
        };
        if let Err(error) = self.gateway(&access_token).await {
            *self.status.lock().await = ChannelConnectionStatus::Error;
            return Err(error);
        }

        let (shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);
        *self.shutdown_tx.lock().await = Some(shutdown_tx);
        self.runtime_managed.store(true, Ordering::Release);
        let backend = self.clone();
        tokio::spawn(async move {
            backend.run_receiver(command_tx, shutdown_rx).await;
        });
        Ok(())
    }

    async fn stop(&self) -> Result<(), ChatChannelError> {
        self.runtime_managed.store(false, Ordering::Release);
        self.gateway_ready.store(false, Ordering::Release);
        if let Some(tx) = self.shutdown_tx.lock().await.take() {
            let _ = tx.send(true);
        }
        *self.status.lock().await = ChannelConnectionStatus::Disconnected;
        Ok(())
    }

    async fn status(&self) -> ChannelConnectionStatus {
        *self.status.lock().await
    }

    async fn send_message(&self, text: &str) -> Result<SentMessageId, ChatChannelError> {
        self.send_to_owner(text).await
    }

    async fn send_rich_message(
        &self,
        message: &RichMessage,
    ) -> Result<SentMessageId, ChatChannelError> {
        self.send_to_owner(&message.to_plain_text()).await
    }

    async fn send_rich_message_to(
        &self,
        message: &RichMessage,
        target: &ChannelMessageTarget,
    ) -> Result<SentMessageId, ChatChannelError> {
        let owner = self.owner().await.ok_or_else(|| {
            ChatChannelError::ConfigurationInvalid(
                "QQBot owner is not bound; send a private message to the bot first".to_string(),
            )
        })?;
        let target_owner = target.chat_id.as_deref().unwrap_or_default();
        if target.channel_id != self.channel_id
            || target.thread_kind.as_deref() != Some(QQBOT_C2C_THREAD_KIND)
            || target_owner != owner
        {
            return Err(ChatChannelError::SendFailed(
                "QQBot recipient rejected: target is not the bound owner".to_string(),
            ));
        }
        self.send_c2c(&owner, &message.to_plain_text()).await
    }

    async fn test_connection(&self) -> Result<(), ChatChannelError> {
        let access_token = self.access_token().await?;
        self.gateway(&access_token).await.map(|_| ())
    }

    async fn confirm_owner_binding(&self, owner_openid: &str) -> Result<(), ChatChannelError> {
        self.confirm_owner(owner_openid).await
    }
}

async fn wait_for_retry(
    shutdown_rx: &mut tokio::sync::watch::Receiver<bool>,
    retry_count: u32,
    retry_after: Option<Duration>,
) -> bool {
    let delay = retry_delay(retry_count, retry_after);
    tokio::select! {
        _ = tokio::time::sleep(delay) => false,
        _ = shutdown_rx.changed() => true,
    }
}

async fn handle_qqbot_event(
    event: &serde_json::Value,
    backend: &QqBotBackend,
    command_tx: &mpsc::Sender<IncomingCommand>,
    seen_ids: &mut SeenMessageIds,
) {
    // C2C text events use msg_type 0. Ignore any future rich/media event
    // shapes rather than treating their metadata as a text command.
    if event
        .get("msg_type")
        .or_else(|| event.get("message_type"))
        .and_then(serde_json::Value::as_i64)
        .is_some_and(|message_type| message_type != 0)
    {
        return;
    }
    let Some(sender_id) = event
        .pointer("/author/user_openid")
        .and_then(serde_json::Value::as_str)
        .map(str::trim)
        .filter(|id| !id.is_empty())
    else {
        return;
    };
    let Some(text) = event
        .get("content")
        .and_then(serde_json::Value::as_str)
        .map(str::trim)
        .filter(|text| !text.is_empty())
    else {
        return;
    };

    if !backend.accept_owner(sender_id).await {
        tracing::debug!("[QQBot] ignored C2C message from a non-owner OpenID");
        return;
    }
    let message_id = event
        .get("id")
        .and_then(serde_json::Value::as_str)
        .unwrap_or_default();
    if seen_ids.already_seen(message_id) {
        return;
    }

    let runtime_identity = Some(ChannelRuntimeIdentity {
        generation: backend.runtime_generation.load(Ordering::Acquire),
        app_id: backend.app_id.clone(),
        owner_openid: backend.owner().await,
    });
    let command = IncomingCommand {
        channel_id: backend.channel_id,
        sender_id: sender_id.to_string(),
        command_text: text.to_string(),
        callback_data: None,
        target: ChannelMessageTarget::qqbot_c2c(backend.channel_id, sender_id),
        metadata: event.clone(),
        owner_binding: true,
        runtime_identity,
    };
    if let Err(error) = command_tx.send(command).await {
        tracing::error!("[QQBot] failed to dispatch C2C command: {error}");
    }
}

fn normalize_owner(owner: Option<String>) -> Option<String> {
    owner
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
}

fn redact_secrets<const N: usize>(mut message: String, secrets: [&str; N]) -> String {
    for secret in secrets {
        if !secret.is_empty() {
            message = message.replace(secret, "***");
        }
    }
    message
}

fn format_platform_error<const N: usize>(
    status: StatusCode,
    body: &str,
    operation: &str,
    secrets: [&str; N],
) -> String {
    let detail = platform_detail(body);
    let detail = redact_secrets(detail, secrets);
    if detail.is_empty() {
        format!("{operation} failed (HTTP {status})")
    } else {
        format!("{operation} failed (HTTP {status}): {detail}")
    }
}

fn parse_send_response<const N: usize>(
    status: StatusCode,
    body: &str,
    secrets: [&str; N],
) -> Result<SentMessageId, ChatChannelError> {
    let parsed: serde_json::Value = serde_json::from_str(body)
        .map_err(|e| ChatChannelError::SendFailed(format!("QQBot response was invalid: {e}")))?;
    if qq_business_error(&parsed).is_some() {
        return Err(classify_send_failure(status, body, secrets));
    }
    let message_id = parsed
        .get("id")
        .or_else(|| parsed.get("message_id"))
        .and_then(json_value_to_string)
        .map(|id| id.trim().to_string())
        .filter(|id| !id.is_empty())
        .ok_or_else(|| {
            ChatChannelError::SendFailed("QQBot response did not include a message id".to_string())
        })?;
    Ok(SentMessageId(message_id))
}

fn classify_send_failure<const N: usize>(
    status: StatusCode,
    body: &str,
    secrets: [&str; N],
) -> ChatChannelError {
    let detail = redact_secrets(platform_detail(body), secrets);
    let suffix = if detail.is_empty() {
        String::new()
    } else {
        format!(": {detail}")
    };
    if let Ok(parsed) = serde_json::from_str::<serde_json::Value>(body) {
        if let Some((code, _)) = qq_business_error(&parsed) {
            let code_suffix = format!(" (err_code {code})");
            if code == 40034105 {
                return ChatChannelError::SendFailed(format!(
                    "QQBot proactive permission denied{code_suffix}{suffix}"
                ));
            }
            if code == 40034100 {
                return ChatChannelError::SendFailed(format!(
                    "QQBot rate limited{code_suffix}{suffix}"
                ));
            }
            if is_auth_business_code(code) {
                return ChatChannelError::AuthenticationFailed(format!(
                    "QQBot authentication failed{code_suffix}{suffix}"
                ));
            }
            if is_recipient_business_code(code) {
                return ChatChannelError::SendFailed(format!(
                    "QQBot recipient/friend rejected{code_suffix}{suffix}"
                ));
            }
            return ChatChannelError::SendFailed(format!(
                "QQBot API business error{code_suffix}{suffix}"
            ));
        }
    }
    match status {
        StatusCode::UNAUTHORIZED => ChatChannelError::AuthenticationFailed(format!(
            "QQBot authentication failed (HTTP 401){suffix}"
        )),
        StatusCode::FORBIDDEN => ChatChannelError::SendFailed(format!(
            "QQBot platform permission denied (HTTP 403){suffix}"
        )),
        StatusCode::BAD_REQUEST | StatusCode::NOT_FOUND | StatusCode::UNPROCESSABLE_ENTITY => {
            ChatChannelError::SendFailed(format!(
                "QQBot recipient rejected (HTTP {status}){suffix}"
            ))
        }
        StatusCode::TOO_MANY_REQUESTS => {
            ChatChannelError::SendFailed(format!("QQBot rate limited (HTTP 429){suffix}"))
        }
        _ => ChatChannelError::SendFailed(format!("QQBot API error (HTTP {status}){suffix}")),
    }
}

fn is_auth_business_code(code: i64) -> bool {
    code == 401
        || code == 40100
        || (11240..=11243).contains(&code)
        || (40_100_000..=40_199_999).contains(&code)
}

fn is_recipient_business_code(code: i64) -> bool {
    matches!(code, 40054004 | 40054013)
}

fn is_retryable_auth_error(error: &ChatChannelError) -> bool {
    let ChatChannelError::AuthenticationFailed(message) = error else {
        return false;
    };
    message.contains("HTTP 401")
        || message.contains("err_code 401")
        || message.contains("err_code 40100")
        || message.contains("err_code 11241")
        || message.contains("err_code 11242")
        || message.contains("err_code 11243")
}

fn json_value_to_string(value: &serde_json::Value) -> Option<String> {
    value
        .as_str()
        .map(str::to_string)
        .or_else(|| value.as_i64().map(|number| number.to_string()))
}

fn qq_business_error(value: &serde_json::Value) -> Option<(i64, String)> {
    let code = value
        .get("err_code")
        .or_else(|| value.get("code"))
        .and_then(|value| {
            value
                .as_i64()
                .or_else(|| value.as_str().and_then(|text| text.parse().ok()))
        })?;
    if code == 0 {
        return None;
    }
    Some((code, platform_detail(&value.to_string())))
}

fn platform_detail(body: &str) -> String {
    let parsed: serde_json::Value = serde_json::from_str(body).unwrap_or_default();
    let detail = parsed
        .get("message")
        .or_else(|| parsed.get("msg"))
        .or_else(|| parsed.get("err_msg"))
        .or_else(|| parsed.get("error_message"))
        .or_else(|| parsed.get("error_description"))
        .and_then(serde_json::Value::as_str)
        .unwrap_or_else(|| body.trim());
    detail.chars().take(512).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::{to_bytes, Body};
    use axum::extract::State;
    use axum::http::Request;
    use axum::response::IntoResponse;
    use axum::routing::any;
    use axum::{Json, Router};
    use futures_util::Sink;
    use std::sync::Arc;
    use std::task::{Context, Poll};
    use tokio::sync::mpsc;

    fn backend(owner_openid: Option<&str>) -> QqBotBackend {
        QqBotBackend::new(
            7,
            "app-id".into(),
            "app-secret".into(),
            owner_openid.map(str::to_string),
        )
    }

    #[derive(Default)]
    struct TestSink;

    impl Sink<Message> for TestSink {
        type Error = std::convert::Infallible;

        fn poll_ready(
            self: std::pin::Pin<&mut Self>,
            _cx: &mut Context<'_>,
        ) -> Poll<Result<(), Self::Error>> {
            Poll::Ready(Ok(()))
        }

        fn start_send(self: std::pin::Pin<&mut Self>, _item: Message) -> Result<(), Self::Error> {
            Ok(())
        }

        fn poll_flush(
            self: std::pin::Pin<&mut Self>,
            _cx: &mut Context<'_>,
        ) -> Poll<Result<(), Self::Error>> {
            Poll::Ready(Ok(()))
        }

        fn poll_close(
            self: std::pin::Pin<&mut Self>,
            _cx: &mut Context<'_>,
        ) -> Poll<Result<(), Self::Error>> {
            Poll::Ready(Ok(()))
        }
    }

    #[derive(Clone, Default)]
    struct MockApiState {
        requests: Arc<Mutex<Vec<(String, String, serde_json::Value)>>>,
        token_responses: Arc<Mutex<Vec<serde_json::Value>>>,
        send_responses: Arc<Mutex<Vec<serde_json::Value>>>,
    }

    async fn mock_api(
        State(state): State<MockApiState>,
        request: Request<Body>,
    ) -> impl IntoResponse {
        let path = request.uri().path().to_string();
        let authorization = request
            .headers()
            .get("authorization")
            .and_then(|value| value.to_str().ok())
            .unwrap_or_default()
            .to_string();
        let body = to_bytes(request.into_body(), 1024 * 1024)
            .await
            .expect("mock request body");
        let json = serde_json::from_slice(&body).unwrap_or_default();
        state
            .requests
            .lock()
            .await
            .push((path.clone(), authorization, json));

        if path == "/token" {
            let response = {
                let mut responses = state.token_responses.lock().await;
                if responses.is_empty() {
                    serde_json::json!({
                        "access_token": "access-token",
                        "expires_in": 3600,
                    })
                } else {
                    responses.remove(0)
                }
            };
            Json(response).into_response()
        } else if path == "/gateway" {
            Json(serde_json::json!({ "url": "wss://gateway.invalid" })).into_response()
        } else {
            let response = {
                let mut responses = state.send_responses.lock().await;
                if responses.is_empty() {
                    serde_json::json!({ "id": "sent-message-id" })
                } else {
                    responses.remove(0)
                }
            };
            Json(response).into_response()
        }
    }

    async fn backend_with_mock_api(state: MockApiState) -> QqBotBackend {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("mock listener");
        let base_url = format!("http://{}", listener.local_addr().unwrap());
        let app = Router::new().fallback(any(mock_api)).with_state(state);
        tokio::spawn(async move {
            axum::serve(listener, app).await.expect("mock server");
        });
        QqBotBackend::new_with_endpoints(
            7,
            "app-id".into(),
            "app-secret".into(),
            Some("owner-openid".into()),
            QqBotEndpoints {
                token_url: format!("{base_url}/token"),
                gateway_url: format!("{base_url}/gateway"),
                api_base_url: base_url,
            },
        )
    }

    #[test]
    fn proactive_body_omits_inbound_message_id() {
        let body = serde_json::json!({
            "content": "turn complete",
            "msg_type": 0,
        });
        assert_eq!(body["msg_type"], 0);
        assert!(body.get("msg_id").is_none());
        assert!(body.get("is_wakeup").is_none());
    }

    #[test]
    fn send_failures_keep_platform_error_categories() {
        assert!(matches!(
            classify_send_failure::<0>(StatusCode::FORBIDDEN, "{\"message\":\"scope\"}", []),
            ChatChannelError::SendFailed(message) if message.contains("permission denied")
        ));
        assert!(matches!(
            classify_send_failure::<0>(StatusCode::NOT_FOUND, "recipient", []),
            ChatChannelError::SendFailed(message) if message.contains("recipient rejected")
        ));
        assert!(matches!(
            classify_send_failure::<0>(StatusCode::TOO_MANY_REQUESTS, "", []),
            ChatChannelError::SendFailed(message) if message.contains("rate limited")
        ));
        assert!(matches!(
            classify_send_failure::<0>(
                StatusCode::OK,
                "{\"err_code\":40034105,\"err_msg\":\"proactive denied\"}",
                []
            ),
            ChatChannelError::SendFailed(message)
                if message.contains("proactive permission denied")
                    && message.contains("40034105")
        ));
        assert!(matches!(
            classify_send_failure::<0>(
                StatusCode::OK,
                "{\"err_code\":40034100,\"err_msg\":\"too fast\"}",
                []
            ),
            ChatChannelError::SendFailed(message)
                if message.contains("rate limited") && message.contains("40034100")
        ));
        assert!(matches!(
            classify_send_failure::<0>(
                StatusCode::OK,
                "{\"err_code\":40054004,\"err_msg\":\"not a friend\"}",
                []
            ),
            ChatChannelError::SendFailed(message)
                if message.contains("recipient/friend rejected")
        ));
        assert!(matches!(
            classify_send_failure::<0>(
                StatusCode::OK,
                "{\"err_code\":40054013,\"err_msg\":\"recipient rejected\"}",
                []
            ),
            ChatChannelError::SendFailed(message)
                if message.contains("recipient/friend rejected")
        ));
    }

    #[test]
    fn nonzero_err_code_never_becomes_empty_success() {
        let result = parse_send_response::<0>(
            StatusCode::OK,
            "{\"err_code\":40034105,\"err_msg\":\"denied\"}",
            [],
        );
        assert!(
            matches!(result, Err(ChatChannelError::SendFailed(message)) if message.contains("permission denied"))
        );

        let result = parse_send_response::<0>(StatusCode::OK, "{}", []);
        assert!(
            matches!(result, Err(ChatChannelError::SendFailed(message)) if message.contains("message id"))
        );

        for body in [r#"{"id":""}"#, r#"{"message_id":"  "}"#] {
            let result = parse_send_response::<0>(StatusCode::OK, body, []);
            assert!(
                matches!(result, Err(ChatChannelError::SendFailed(message)) if message.contains("message id")),
                "empty message id must fail: {body}"
            );
        }
    }

    #[test]
    fn gateway_uses_identify_then_resume_and_falls_back_after_clear() {
        let mut fresh = GatewaySession::default();
        fresh.mark_ready("session-1");
        fresh.sequence = Some(42);
        assert!(fresh.can_resume());
        assert_eq!(fresh.session_id.as_deref(), Some("session-1"));

        let fresh = GatewaySession::default();
        assert_eq!(gateway_handshake("access-token", &fresh)["op"], 2);

        let resumable = GatewaySession {
            session_id: Some("session-1".into()),
            sequence: Some(42),
            resumable: true,
        };
        let resume = gateway_handshake("access-token", &resumable);
        assert_eq!(resume["op"], 6);
        assert_eq!(resume["d"]["session_id"], "session-1");
        assert_eq!(resume["d"]["seq"], 42);

        let mut invalid = resumable;
        invalid.clear();
        assert!(!invalid.can_resume());
        assert_eq!(gateway_handshake("access-token", &invalid)["op"], 2);
    }

    #[test]
    fn gateway_close_codes_select_session_and_token_actions() {
        let ordinary = classify_gateway_close(Some(1006));
        assert_eq!(ordinary, classify_gateway_close(Some(1000)));
        assert!(!ordinary.clear_session);
        assert!(!ordinary.refresh_token);

        let refresh = classify_gateway_close(Some(4004));
        assert!(refresh.refresh_token);
        assert!(!refresh.clear_session);

        for code in [4006, 4007, 4009, 4900, 4913] {
            let action = classify_gateway_close(Some(code));
            assert!(action.clear_session, "code {code}");
            assert!(action.refresh_token, "code {code}");
        }

        assert_eq!(
            classify_gateway_close(Some(4008)).retry_after,
            Some(Duration::from_secs(60))
        );
        assert!(classify_gateway_close(Some(4914)).fatal);
        assert!(classify_gateway_close(Some(4915)).fatal);
    }

    #[test]
    fn gateway_retry_delay_honors_rate_limit_close() {
        assert_eq!(
            retry_delay(0, Some(Duration::from_secs(60))),
            Duration::from_secs(60)
        );
        assert_eq!(retry_delay(3, None), Duration::from_secs(8));
    }

    #[tokio::test]
    async fn gateway_close_applies_token_and_session_invalidation() {
        let backend = backend(None);
        *backend.token_cache.write().await = Some(AccessToken {
            value: "cached-token".to_string(),
            expires_at: Instant::now() + Duration::from_secs(60),
        });
        let mut session = GatewaySession {
            session_id: Some("session-1".to_string()),
            sequence: Some(42),
            resumable: true,
        };
        let mut ready = true;
        let result = backend
            .apply_gateway_close(Some(4009), &mut session, &mut ready)
            .await;
        assert!(matches!(result, GatewayClose::Reconnect { .. }));
        assert!(!ready);
        assert!(!session.can_resume());
        assert!(backend.token_cache.read().await.is_none());

        *backend.token_cache.write().await = Some(AccessToken {
            value: "cached-token".to_string(),
            expires_at: Instant::now() + Duration::from_secs(60),
        });
        session = GatewaySession {
            session_id: Some("session-1".to_string()),
            sequence: Some(42),
            resumable: true,
        };
        ready = true;
        let result = backend
            .apply_gateway_close(Some(4004), &mut session, &mut ready)
            .await;
        assert!(matches!(result, GatewayClose::Reconnect { .. }));
        assert!(session.can_resume());
        assert!(backend.token_cache.read().await.is_none());
    }

    #[tokio::test]
    async fn fatal_gateway_close_does_not_schedule_reconnect() {
        let backend = backend(None);
        let mut session = GatewaySession {
            session_id: Some("session-1".to_string()),
            sequence: Some(42),
            resumable: true,
        };
        let mut ready = true;
        let result = backend
            .apply_gateway_close(Some(4914), &mut session, &mut ready)
            .await;
        assert_eq!(result, GatewayClose::Fatal { code: 4914 });
        assert!(session.can_resume());
    }

    #[tokio::test]
    async fn ready_and_resumed_are_the_connected_boundary() {
        let backend = backend(None);
        let (tx, _rx) = mpsc::channel(1);
        let mut seen = SeenMessageIds::default();
        let mut session = GatewaySession::default();
        let mut heartbeat = tokio::time::interval(Duration::from_secs(3600));
        let mut handshake_sent = false;
        let mut ready = false;
        let mut retry_count = 7;
        let mut sink = TestSink;

        backend
            .handle_gateway_payload(
                br#"{"op":0,"t":"READY","s":1,"d":{"session_id":"session-1"}}"#,
                "token",
                &mut sink,
                &tx,
                &mut seen,
                &mut session,
                &mut heartbeat,
                &mut handshake_sent,
                &mut ready,
                &mut retry_count,
            )
            .await
            .expect("READY payload");
        assert!(ready);
        assert_eq!(retry_count, 0);
        assert_eq!(backend.status().await, ChannelConnectionStatus::Connected);

        ready = false;
        backend.gateway_ready.store(false, Ordering::Release);
        backend
            .handle_gateway_payload(
                br#"{"op":0,"t":"RESUMED","s":2,"d":{}}"#,
                "token",
                &mut sink,
                &tx,
                &mut seen,
                &mut session,
                &mut heartbeat,
                &mut handshake_sent,
                &mut ready,
                &mut retry_count,
            )
            .await
            .expect("RESUMED payload");
        assert!(ready);
        assert!(backend.gateway_ready.load(Ordering::Acquire));
        assert_eq!(backend.status().await, ChannelConnectionStatus::Connected);
    }

    #[tokio::test]
    async fn op7_reconnects_with_session_and_op9_false_identifies() {
        let backend = backend(None);
        let (tx, _rx) = mpsc::channel(1);
        let mut seen = SeenMessageIds::default();
        let mut session = GatewaySession {
            session_id: Some("session-1".to_string()),
            sequence: Some(42),
            resumable: true,
        };
        let mut heartbeat = tokio::time::interval(Duration::from_secs(3600));
        let mut handshake_sent = true;
        let mut ready = true;
        let mut retry_count = 0;
        let mut sink = TestSink;

        let result = backend
            .handle_gateway_payload(
                br#"{"op":7}"#,
                "token",
                &mut sink,
                &tx,
                &mut seen,
                &mut session,
                &mut heartbeat,
                &mut handshake_sent,
                &mut ready,
                &mut retry_count,
            )
            .await
            .expect("op 7");
        assert!(matches!(result, Some(GatewayClose::Reconnect { .. })));
        assert!(session.can_resume());

        ready = true;
        let result = backend
            .handle_gateway_payload(
                br#"{"op":9,"d":false}"#,
                "token",
                &mut sink,
                &tx,
                &mut seen,
                &mut session,
                &mut heartbeat,
                &mut handshake_sent,
                &mut ready,
                &mut retry_count,
            )
            .await
            .expect("op 9");
        assert!(matches!(result, Some(GatewayClose::Reconnect { .. })));
        assert!(!session.can_resume());
    }

    #[tokio::test]
    async fn proactive_send_is_rejected_during_reconnect_window() {
        let backend = backend(Some("owner-openid"));
        backend.runtime_managed.store(true, Ordering::Release);
        *backend.status.lock().await = ChannelConnectionStatus::Connecting;
        let result = backend.send_message("do not fake-success").await;
        assert!(matches!(
            result,
            Err(ChatChannelError::SendFailed(message))
                if message.contains("gateway is reconnecting")
        ));
    }

    #[test]
    fn errors_redact_app_secret_and_access_token() {
        let message = format_platform_error::<2>(
            StatusCode::BAD_REQUEST,
            "{\"message\":\"app-secret access-token\"}",
            "send",
            ["app-secret", "access-token"],
        );
        assert!(!message.contains("app-secret"));
        assert!(!message.contains("access-token"));
        assert!(message.contains("***"));
    }

    #[tokio::test]
    async fn first_c2c_binds_owner_and_rejects_other_openids() {
        let backend = backend(None);
        let (tx, mut rx) = mpsc::channel(4);
        let mut seen = SeenMessageIds::default();
        let event = serde_json::json!({
            "id": "message-1",
            "content": "/help",
            "author": {"user_openid": "owner-openid"}
        });
        handle_qqbot_event(&event, &backend, &tx, &mut seen).await;
        let command = rx.recv().await.expect("owner command");
        assert!(command.owner_binding);
        assert_eq!(command.sender_id, "owner-openid");
        assert_eq!(command.target.chat_id.as_deref(), Some("owner-openid"));
        assert!(backend.owner().await.is_none());
        backend
            .confirm_owner("owner-openid")
            .await
            .expect("durable owner confirmation");

        let other = serde_json::json!({
            "id": "message-2",
            "content": "/status",
            "author": {"user_openid": "other-openid"}
        });
        handle_qqbot_event(&other, &backend, &tx, &mut seen).await;
        assert!(rx.try_recv().is_err());

        let media = serde_json::json!({
            "id": "message-media",
            "msg_type": 1,
            "content": "ignored",
            "author": {"user_openid": "owner-openid"}
        });
        handle_qqbot_event(&media, &backend, &tx, &mut seen).await;
        assert!(
            rx.try_recv().is_err(),
            "non-text C2C events must be ignored"
        );

        handle_qqbot_event(&event, &backend, &tx, &mut seen).await;
        assert!(
            rx.try_recv().is_err(),
            "duplicate message id must be ignored"
        );
    }

    #[tokio::test]
    async fn unconfirmed_owner_remains_durable_check_for_follow_up_messages() {
        let backend = backend(None);
        let (tx, mut rx) = mpsc::channel(4);
        let mut seen = SeenMessageIds::default();
        let event = |id: &str| {
            serde_json::json!({
                "id": id,
                "content": "/status",
                "author": {"user_openid": "owner-openid"}
            })
        };

        handle_qqbot_event(&event("message-1"), &backend, &tx, &mut seen).await;
        handle_qqbot_event(&event("message-2"), &backend, &tx, &mut seen).await;
        assert!(backend.owner().await.is_none());
        assert!(rx.recv().await.expect("first candidate").owner_binding);
        assert!(rx.recv().await.expect("follow-up candidate").owner_binding);
    }

    #[tokio::test]
    async fn mocked_api_sends_owner_scoped_proactive_text() {
        let state = MockApiState::default();
        let backend = backend_with_mock_api(state.clone()).await;
        backend.test_connection().await.expect("gateway lookup");
        let sent = backend.send_message("turn complete").await.expect("send");
        assert_eq!(sent.0, "sent-message-id");

        let requests = state.requests.lock().await;
        assert_eq!(requests.len(), 3);
        assert_eq!(requests[1].0, "/gateway");
        assert_eq!(requests[1].1, "QQBot access-token");
        let (path, auth, body) = &requests[2];
        assert!(path.ends_with("/v2/users/owner-openid/messages"));
        assert_eq!(auth, "QQBot access-token");
        assert_eq!(body["content"], "turn complete");
        assert_eq!(body["msg_type"], 0);
        assert!(body.get("msg_id").is_none());
        assert!(body.get("is_wakeup").is_none());
    }

    #[test]
    fn default_token_endpoint_matches_current_official_api() {
        assert_eq!(
            QqBotEndpoints::default().token_url,
            "https://api.bot.qq.com/app/getAppAccessToken"
        );
    }

    #[tokio::test]
    async fn token_response_accepts_official_string_expiry() {
        let state = MockApiState::default();
        state.token_responses.lock().await.push(serde_json::json!({
            "access_token": "access-token",
            "expires_in": "7200"
        }));
        let backend = backend_with_mock_api(state.clone()).await;

        backend.test_connection().await.expect("gateway lookup");

        let requests = state.requests.lock().await;
        assert_eq!(requests.len(), 2);
        assert_eq!(requests[0].0, "/token");
        assert_eq!(
            requests[0].2,
            serde_json::json!({
                "appId": "app-id",
                "clientSecret": "app-secret"
            })
        );
    }

    #[tokio::test]
    async fn token_business_error_preserves_code_and_redacts_secret() {
        let state = MockApiState::default();
        state.token_responses.lock().await.push(serde_json::json!({
            "code": 100016,
            "message": "invalid app-secret"
        }));
        let backend = backend_with_mock_api(state.clone()).await;

        let error = backend.test_connection().await.unwrap_err();

        assert!(matches!(
            error,
            ChatChannelError::AuthenticationFailed(message)
                if message.contains("100016")
                    && message.contains("***")
                    && !message.contains("app-secret")
        ));
        let requests = state.requests.lock().await;
        assert_eq!(
            requests
                .iter()
                .filter(|request| request.0 == "/token")
                .count(),
            1
        );
        assert!(requests.iter().all(|request| request.0 != "/gateway"));
    }

    #[tokio::test]
    async fn token_auth_business_error_refreshes_once_in_send_path() {
        let state = MockApiState::default();
        state.token_responses.lock().await.extend([
            serde_json::json!({"access_token":"token-1","expires_in":3600}),
            serde_json::json!({"access_token":"token-2","expires_in":3600}),
        ]);
        state.send_responses.lock().await.extend([
            serde_json::json!({"err_code":11242,"err_msg":"token validation failed"}),
            serde_json::json!({"id":"retried-message-id"}),
        ]);
        let backend = backend_with_mock_api(state.clone()).await;

        let sent = backend.send_message("retry me").await.expect("retry send");
        assert_eq!(sent.0, "retried-message-id");
        let requests = state.requests.lock().await;
        let token_requests = requests
            .iter()
            .filter(|request| request.0 == "/token")
            .count();
        let sends: Vec<_> = requests
            .iter()
            .filter(|request| request.0.ends_with("/messages"))
            .collect();
        assert_eq!(token_requests, 2);
        assert_eq!(sends.len(), 2);
        assert_eq!(sends[0].1, "QQBot token-1");
        assert_eq!(sends[1].1, "QQBot token-2");
    }

    #[tokio::test]
    async fn permanent_app_id_business_error_does_not_retry_send() {
        let state = MockApiState::default();
        state.token_responses.lock().await.push(serde_json::json!({
            "access_token": "token-1",
            "expires_in": 3600
        }));
        state.send_responses.lock().await.push(serde_json::json!({
            "err_code": 11240,
            "err_msg": "invalid app id or authorization header"
        }));
        let backend = backend_with_mock_api(state.clone()).await;

        let result = backend.send_message("do not loop").await;
        assert!(matches!(
            result,
            Err(ChatChannelError::AuthenticationFailed(message))
                if message.contains("11240")
        ));
        let requests = state.requests.lock().await;
        assert_eq!(
            requests
                .iter()
                .filter(|request| request.0 == "/token")
                .count(),
            1
        );
        assert_eq!(
            requests
                .iter()
                .filter(|request| request.0.ends_with("/messages"))
                .count(),
            1
        );
    }

    #[tokio::test]
    async fn target_routing_rejects_a_non_owner_openid() {
        let backend = backend(Some("owner-openid"));
        let target = ChannelMessageTarget::qqbot_c2c(7, "other-openid");
        let result = backend
            .send_rich_message_to(&RichMessage::info("status"), &target)
            .await;
        assert!(matches!(
            result,
            Err(ChatChannelError::SendFailed(message))
                if message.contains("target is not the bound owner")
        ));
    }
}
