use crate::app_error::AppCommandError;
use crate::chat_channel::backends::weixin::{WeixinQrcodeInfo, WeixinQrcodeStatusPublic};
use crate::chat_channel::manager::ChatChannelManager;
use crate::chat_channel::types::ChannelType;
use crate::chat_channel::webhook::WebhookConfig;
use crate::db::service::{chat_channel_message_log_service, chat_channel_service};
use crate::db::AppDatabase;
use crate::models::chat_channel::{ChannelStatusInfo, ChatChannelInfo, ChatChannelMessageLogInfo};

// ---------------------------------------------------------------------------
// Shared core functions (used by both Tauri commands and web handlers)
// ---------------------------------------------------------------------------

pub async fn list_chat_channels_core(
    db: &AppDatabase,
) -> Result<Vec<ChatChannelInfo>, AppCommandError> {
    let rows = chat_channel_service::list_all(&db.conn)
        .await
        .map_err(AppCommandError::from)?;
    Ok(rows.into_iter().map(ChatChannelInfo::from).collect())
}

pub async fn create_chat_channel_core(
    db: &AppDatabase,
    name: String,
    channel_type: String,
    config_json: String,
    enabled: bool,
    daily_report_enabled: bool,
    daily_report_time: Option<String>,
) -> Result<ChatChannelInfo, AppCommandError> {
    // Validate channel_type
    let _: ChannelType = serde_json::from_value(serde_json::Value::String(channel_type.clone()))
        .map_err(|_| {
            AppCommandError::invalid_input(format!("Invalid channel type: {channel_type}"))
        })?;

    let model = chat_channel_service::create(
        &db.conn,
        name,
        channel_type,
        config_json,
        enabled,
        daily_report_enabled,
        daily_report_time,
    )
    .await
    .map_err(AppCommandError::from)?;
    Ok(ChatChannelInfo::from(model))
}

#[allow(clippy::too_many_arguments)]
pub async fn update_chat_channel_core(
    db: &AppDatabase,
    id: i32,
    name: Option<String>,
    enabled: Option<bool>,
    config_json: Option<String>,
    event_filter_json: Option<Option<String>>,
    daily_report_enabled: Option<bool>,
    daily_report_time: Option<Option<String>>,
) -> Result<ChatChannelInfo, AppCommandError> {
    update_chat_channel_core_inner(
        db,
        None,
        id,
        name,
        enabled,
        config_json,
        event_filter_json,
        daily_report_enabled,
        daily_report_time,
        #[cfg(test)]
        None,
    )
    .await
}

#[allow(clippy::too_many_arguments)]
pub async fn update_chat_channel_core_with_manager(
    db: &AppDatabase,
    manager: &ChatChannelManager,
    id: i32,
    name: Option<String>,
    enabled: Option<bool>,
    config_json: Option<String>,
    event_filter_json: Option<Option<String>>,
    daily_report_enabled: Option<bool>,
    daily_report_time: Option<Option<String>>,
) -> Result<ChatChannelInfo, AppCommandError> {
    update_chat_channel_core_inner(
        db,
        Some(manager),
        id,
        name,
        enabled,
        config_json,
        event_filter_json,
        daily_report_enabled,
        daily_report_time,
        #[cfg(test)]
        None,
    )
    .await
}

#[cfg(test)]
struct UpdateTestPause {
    entered: tokio::sync::oneshot::Sender<()>,
    resume: tokio::sync::oneshot::Receiver<()>,
}

#[allow(clippy::too_many_arguments)]
async fn update_chat_channel_core_inner(
    db: &AppDatabase,
    manager: Option<&ChatChannelManager>,
    id: i32,
    name: Option<String>,
    enabled: Option<bool>,
    config_json: Option<String>,
    event_filter_json: Option<Option<String>>,
    daily_report_enabled: Option<bool>,
    daily_report_time: Option<Option<String>>,
    #[cfg(test)] pause: Option<UpdateTestPause>,
) -> Result<ChatChannelInfo, AppCommandError> {
    if let Some(manager) = manager {
        let current = chat_channel_service::get_by_id(&db.conn, id)
            .await
            .map_err(AppCommandError::from)?;
        if current.is_none() && config_json.is_some() {
            return Err(AppCommandError::not_found(format!(
                "Chat channel {id} not found"
            )));
        }
        #[cfg(test)]
        if let Some(pause) = pause {
            let _ = pause.entered.send(());
            let _ = pause.resume.await;
        }
        if current.as_ref().is_some_and(|channel| {
            channel.channel_type == "qqbot" || channel.channel_type == "qq_bot"
        }) {
            let next_config = config_json.clone();
            let model = manager
                .update_with_lifecycle(
                    id,
                    || async {
                        let current = chat_channel_service::get_by_id(&db.conn, id)
                            .await
                            .map_err(AppCommandError::from)?
                            .ok_or_else(|| {
                                AppCommandError::not_found(format!("Chat channel {id} not found"))
                            })?;
                        if let Some(next_config) = &next_config {
                            chat_channel_service::qqbot_config_requires_runtime_reset(
                                &current.config_json,
                                next_config,
                            )
                            .map_err(AppCommandError::from)
                        } else {
                            Ok(false)
                        }
                    },
                    || async {
                        chat_channel_service::update(
                            &db.conn,
                            id,
                            name,
                            enabled,
                            config_json,
                            event_filter_json,
                            daily_report_enabled,
                            daily_report_time,
                        )
                        .await
                        .map_err(AppCommandError::from)
                    },
                )
                .await?;
            return Ok(ChatChannelInfo::from(model));
        }
    }

    let model = chat_channel_service::update(
        &db.conn,
        id,
        name,
        enabled,
        config_json,
        event_filter_json,
        daily_report_enabled,
        daily_report_time,
    )
    .await
    .map_err(AppCommandError::from)?;
    Ok(ChatChannelInfo::from(model))
}

pub async fn delete_chat_channel_core(
    db: &AppDatabase,
    manager: &ChatChannelManager,
    id: i32,
) -> Result<(), AppCommandError> {
    // Disconnect running backend before deleting from DB (prevents orphaned task)
    let _ = manager.remove_channel(id).await;
    chat_channel_service::delete(&db.conn, id)
        .await
        .map_err(AppCommandError::from)?;
    let _ = crate::keyring_store::delete_channel_token(id);
    Ok(())
}

pub async fn connect_chat_channel_core(
    db: &AppDatabase,
    manager: &ChatChannelManager,
    id: i32,
) -> Result<(), AppCommandError> {
    manager
        .add_channel_with_factory(id, || async move {
            let model = chat_channel_service::get_by_id(&db.conn, id)
                .await
                .map_err(AppCommandError::from)?
                .ok_or_else(|| {
                    AppCommandError::not_found(format!("Chat channel {id} not found"))
                })?;

            let channel_type: ChannelType =
                serde_json::from_value(serde_json::Value::String(model.channel_type.clone()))
                    .map_err(|_| {
                        AppCommandError::configuration_invalid(format!(
                            "Invalid channel type: {}",
                            model.channel_type
                        ))
                    })?;

            let config: serde_json::Value =
                serde_json::from_str(&model.config_json).map_err(|e| {
                    AppCommandError::configuration_invalid("Invalid config JSON")
                        .with_detail(e.to_string())
                })?;

            let token = crate::keyring_store::get_channel_token(id).ok_or_else(|| {
                tracing::info!("[connect_chat_channel] channel {id}: Token not set in keyring");
                AppCommandError::configuration_missing("Token not set")
            })?;

            tracing::info!(
                "[connect_chat_channel] channel {id}: creating {channel_type} backend, config={}",
                model.config_json
            );

            let backend =
                crate::chat_channel::backends::create_backend(id, channel_type, &config, token)
                    .map_err(AppCommandError::from)?;
            Ok((id, model.name, channel_type, backend))
        })
        .await
        .map_err(|error: AppCommandError| {
            tracing::error!("[connect_chat_channel] channel {id}: add_channel failed: {error}");
            error
        })?;

    tracing::info!("[connect_chat_channel] channel {id}: connected successfully");
    Ok(())
}

pub async fn test_chat_channel_core(db: &AppDatabase, id: i32) -> Result<(), AppCommandError> {
    let model = chat_channel_service::get_by_id(&db.conn, id)
        .await
        .map_err(AppCommandError::from)?
        .ok_or_else(|| AppCommandError::not_found(format!("Chat channel {id} not found")))?;

    let channel_type: ChannelType = serde_json::from_value(serde_json::Value::String(
        model.channel_type.clone(),
    ))
    .map_err(|_| {
        AppCommandError::configuration_invalid(format!(
            "Invalid channel type: {}",
            model.channel_type
        ))
    })?;

    let config: serde_json::Value = serde_json::from_str(&model.config_json).map_err(|e| {
        AppCommandError::configuration_invalid("Invalid config JSON").with_detail(e.to_string())
    })?;

    let token = crate::keyring_store::get_channel_token(id)
        .ok_or_else(|| AppCommandError::configuration_missing("Token not set"))?;

    let backend = crate::chat_channel::backends::create_backend(id, channel_type, &config, token)
        .map_err(AppCommandError::from)?;

    backend
        .test_connection()
        .await
        .map_err(AppCommandError::from)?;

    Ok(())
}

pub fn save_chat_channel_token_core(channel_id: i32, token: &str) -> Result<(), AppCommandError> {
    crate::keyring_store::set_channel_token(channel_id, token)
        .map_err(|e| AppCommandError::io_error("Failed to save token").with_detail(e))
}

pub fn get_chat_channel_has_token_core(channel_id: i32) -> Result<bool, AppCommandError> {
    Ok(crate::keyring_store::get_channel_token(channel_id).is_some())
}

pub fn delete_chat_channel_token_core(channel_id: i32) -> Result<(), AppCommandError> {
    crate::keyring_store::delete_channel_token(channel_id)
        .map_err(|e| AppCommandError::io_error("Failed to delete token").with_detail(e))
}

pub async fn disconnect_chat_channel_core(
    manager: &ChatChannelManager,
    id: i32,
) -> Result<(), AppCommandError> {
    manager
        .remove_channel(id)
        .await
        .map_err(AppCommandError::from)?;
    Ok(())
}

pub async fn get_chat_channel_status_core(
    manager: &ChatChannelManager,
) -> Result<Vec<ChannelStatusInfo>, AppCommandError> {
    Ok(manager.get_status().await)
}

pub async fn list_chat_channel_messages_core(
    db: &AppDatabase,
    channel_id: i32,
    limit: Option<u64>,
    offset: Option<u64>,
) -> Result<Vec<ChatChannelMessageLogInfo>, AppCommandError> {
    let limit = limit.unwrap_or(50);
    let offset = offset.unwrap_or(0);
    let rows =
        chat_channel_message_log_service::list_by_channel(&db.conn, channel_id, limit, offset)
            .await
            .map_err(AppCommandError::from)?;
    Ok(rows
        .into_iter()
        .map(ChatChannelMessageLogInfo::from)
        .collect())
}

const COMMAND_PREFIX_KEY: &str = "chat_command_prefix";
const DEFAULT_COMMAND_PREFIX: &str = "/";

pub async fn get_chat_command_prefix_core(db: &AppDatabase) -> Result<String, AppCommandError> {
    let val = crate::db::service::app_metadata_service::get_value(&db.conn, COMMAND_PREFIX_KEY)
        .await
        .map_err(AppCommandError::from)?;
    Ok(val.unwrap_or_else(|| DEFAULT_COMMAND_PREFIX.to_string()))
}

pub async fn set_chat_command_prefix_core(
    db: &AppDatabase,
    prefix: String,
) -> Result<(), AppCommandError> {
    let trimmed = prefix.trim();
    if trimmed.is_empty() || trimmed.len() > 3 || trimmed.chars().any(|c| c.is_alphanumeric()) {
        return Err(AppCommandError::invalid_input(
            "Prefix must be 1-3 non-alphanumeric characters",
        ));
    }
    crate::db::service::app_metadata_service::upsert_value(&db.conn, COMMAND_PREFIX_KEY, trimmed)
        .await
        .map_err(AppCommandError::from)?;
    Ok(())
}

const MESSAGE_LANGUAGE_KEY: &str = "chat_message_language";

pub async fn get_chat_message_language_core(db: &AppDatabase) -> Result<String, AppCommandError> {
    let val = crate::db::service::app_metadata_service::get_value(&db.conn, MESSAGE_LANGUAGE_KEY)
        .await
        .map_err(AppCommandError::from)?;
    Ok(val.unwrap_or_else(|| "en".to_string()))
}

pub async fn set_chat_message_language_core(
    db: &AppDatabase,
    language: String,
) -> Result<(), AppCommandError> {
    // Validate language code
    let valid = [
        "en", "zh-cn", "zh-tw", "ja", "ko", "es", "de", "fr", "pt", "ar",
    ];
    let lang_lower = language.to_lowercase();
    if !valid.contains(&lang_lower.as_str()) {
        return Err(AppCommandError::invalid_input(format!(
            "Unsupported language: {language}. Supported: {}",
            valid.join(", ")
        )));
    }
    crate::db::service::app_metadata_service::upsert_value(
        &db.conn,
        MESSAGE_LANGUAGE_KEY,
        &lang_lower,
    )
    .await
    .map_err(AppCommandError::from)?;
    crate::chat_channel::event_subscriber::bump_event_config_epoch();
    Ok(())
}

const EVENT_FILTER_KEY: &str = "chat_event_filter";

pub async fn get_chat_event_filter_core(
    db: &AppDatabase,
) -> Result<Option<Vec<String>>, AppCommandError> {
    let val = crate::db::service::app_metadata_service::get_value(&db.conn, EVENT_FILTER_KEY)
        .await
        .map_err(AppCommandError::from)?;
    match val {
        Some(json) => {
            // Parse as Option<Vec<String>> to correctly handle stored "null"
            let filter: Option<Vec<String>> = serde_json::from_str(&json)
                .map_err(|e| AppCommandError::invalid_input(e.to_string()))?;
            Ok(filter)
        }
        None => Ok(None),
    }
}

pub async fn set_chat_event_filter_core(
    db: &AppDatabase,
    filter: Option<Vec<String>>,
) -> Result<(), AppCommandError> {
    match filter {
        Some(arr) => {
            let json = serde_json::to_string(&arr)
                .map_err(|e| AppCommandError::invalid_input(e.to_string()))?;
            crate::db::service::app_metadata_service::upsert_value(
                &db.conn,
                EVENT_FILTER_KEY,
                &json,
            )
            .await
            .map_err(AppCommandError::from)?;
        }
        None => {
            // null is the DEFAULT event set: every event EXCEPT the opt-in ones
            // that export prompt text (see `event_subscriber::DEFAULT_OFF_EVENTS`,
            // e.g. `user_prompt_sent`). Persist the sentinel "null".
            crate::db::service::app_metadata_service::upsert_value(
                &db.conn,
                EVENT_FILTER_KEY,
                "null",
            )
            .await
            .map_err(AppCommandError::from)?;
        }
    }
    crate::chat_channel::event_subscriber::bump_event_config_epoch();
    Ok(())
}

const EVENT_WEBHOOKS_KEY: &str = "chat_event_webhooks";

pub async fn get_chat_event_webhooks_core(
    db: &AppDatabase,
) -> Result<Vec<WebhookConfig>, AppCommandError> {
    let val = crate::db::service::app_metadata_service::get_value(&db.conn, EVENT_WEBHOOKS_KEY)
        .await
        .map_err(AppCommandError::from)?;
    match val {
        Some(json) => {
            let hooks: Vec<WebhookConfig> = serde_json::from_str(&json)
                .map_err(|e| AppCommandError::invalid_input(e.to_string()))?;
            Ok(hooks)
        }
        None => Ok(Vec::new()),
    }
}

pub async fn set_chat_event_webhooks_core(
    db: &AppDatabase,
    webhooks: Vec<WebhookConfig>,
) -> Result<(), AppCommandError> {
    // Trim, drop empty-URL rows, require http(s), dedup by URL (order-preserving,
    // first `enabled` wins). Store the trimmed original (not reqwest's normalized
    // form) so the user's input round-trips unchanged in the UI.
    let mut cleaned: Vec<WebhookConfig> = Vec::new();
    for w in webhooks {
        let url = w.url.trim();
        if url.is_empty() {
            continue;
        }
        let parsed = reqwest::Url::parse(url)
            .map_err(|_| AppCommandError::invalid_input(format!("Invalid webhook URL: {url}")))?;
        if !matches!(parsed.scheme(), "http" | "https") {
            return Err(AppCommandError::invalid_input(format!(
                "Webhook URL must use http or https: {url}"
            )));
        }
        if !cleaned.iter().any(|c| c.url == url) {
            cleaned.push(WebhookConfig {
                url: url.to_string(),
                enabled: w.enabled,
            });
        }
    }
    let json = serde_json::to_string(&cleaned)
        .map_err(|e| AppCommandError::invalid_input(e.to_string()))?;
    crate::db::service::app_metadata_service::upsert_value(&db.conn, EVENT_WEBHOOKS_KEY, &json)
        .await
        .map_err(AppCommandError::from)?;
    crate::chat_channel::event_subscriber::bump_event_config_epoch();
    Ok(())
}

/// Whether delegation sub-sessions stay quiet on the global event feed (no
/// `turn_complete` / `error` push to IM channels or webhooks). Off by default.
pub async fn get_chat_event_top_level_only_core(db: &AppDatabase) -> Result<bool, AppCommandError> {
    let val = crate::db::service::app_metadata_service::get_value(
        &db.conn,
        crate::chat_channel::event_subscriber::EVENT_TOP_LEVEL_ONLY_KEY,
    )
    .await
    .map_err(AppCommandError::from)?;
    Ok(val.as_deref() == Some("true"))
}

pub async fn set_chat_event_top_level_only_core(
    db: &AppDatabase,
    enabled: bool,
) -> Result<(), AppCommandError> {
    crate::db::service::app_metadata_service::upsert_value(
        &db.conn,
        crate::chat_channel::event_subscriber::EVENT_TOP_LEVEL_ONLY_KEY,
        if enabled { "true" } else { "false" },
    )
    .await
    .map_err(AppCommandError::from)?;
    crate::chat_channel::event_subscriber::bump_event_config_epoch();
    Ok(())
}

// ---------------------------------------------------------------------------
// WeChat QR code auth
// ---------------------------------------------------------------------------

pub async fn weixin_get_qrcode_core() -> Result<WeixinQrcodeInfo, AppCommandError> {
    crate::chat_channel::backends::weixin::weixin_get_qrcode()
        .await
        .map_err(AppCommandError::from)
}

pub async fn weixin_check_qrcode_core(
    db: &AppDatabase,
    channel_id: i32,
    qrcode: &str,
) -> Result<WeixinQrcodeStatusPublic, AppCommandError> {
    let result = crate::chat_channel::backends::weixin::weixin_check_qrcode(qrcode)
        .await
        .map_err(AppCommandError::from)?;

    // On confirmed: save token + update config with base_url
    if result.status == "confirmed" {
        tracing::error!(
            "[Weixin] QR confirmed for channel {channel_id}, bot_token={}, base_url={}",
            result
                .bot_token
                .as_deref()
                .map(|t| {
                    // Char-boundary-safe prefix: `&t[..8]` panics if a multibyte
                    // char straddles byte 8.
                    let end = t.char_indices().nth(8).map_or(t.len(), |(i, _)| i);
                    &t[..end]
                })
                .unwrap_or("None"),
            result.base_url.as_deref().unwrap_or("None"),
        );
        if let Some(ref token) = result.bot_token {
            save_chat_channel_token_core(channel_id, token)?;
            tracing::info!("[Weixin] Token saved for channel {channel_id}");
        } else {
            tracing::warn!(
                "[Weixin] WARNING: No bot_token in confirmed response for channel {channel_id}"
            );
        }
        if let Some(ref base_url) = result.base_url {
            let config_json = serde_json::json!({ "base_url": base_url }).to_string();
            update_chat_channel_core(
                db,
                channel_id,
                None,
                None,
                Some(config_json),
                None,
                None,
                None,
            )
            .await?;
            tracing::info!("[Weixin] Config updated with base_url for channel {channel_id}");
        }
    }

    // Return only the status — never expose bot_token to the frontend
    Ok(WeixinQrcodeStatusPublic {
        status: result.status,
    })
}

// ---------------------------------------------------------------------------
// Tauri commands (use tauri::State for injection)
// ---------------------------------------------------------------------------

#[cfg(feature = "tauri-runtime")]
#[tauri::command]
pub async fn list_chat_channels(
    db: tauri::State<'_, AppDatabase>,
) -> Result<Vec<ChatChannelInfo>, AppCommandError> {
    list_chat_channels_core(&db).await
}

#[cfg(feature = "tauri-runtime")]
#[tauri::command]
pub async fn create_chat_channel(
    db: tauri::State<'_, AppDatabase>,
    name: String,
    channel_type: String,
    config_json: String,
    enabled: bool,
    daily_report_enabled: bool,
    daily_report_time: Option<String>,
) -> Result<ChatChannelInfo, AppCommandError> {
    create_chat_channel_core(
        &db,
        name,
        channel_type,
        config_json,
        enabled,
        daily_report_enabled,
        daily_report_time,
    )
    .await
}

#[allow(clippy::too_many_arguments)]
#[cfg(feature = "tauri-runtime")]
#[tauri::command]
pub async fn update_chat_channel(
    db: tauri::State<'_, AppDatabase>,
    manager: tauri::State<'_, ChatChannelManager>,
    id: i32,
    name: Option<String>,
    enabled: Option<bool>,
    config_json: Option<String>,
    event_filter_json: Option<Option<String>>,
    daily_report_enabled: Option<bool>,
    daily_report_time: Option<Option<String>>,
) -> Result<ChatChannelInfo, AppCommandError> {
    update_chat_channel_core_with_manager(
        &db,
        &manager,
        id,
        name,
        enabled,
        config_json,
        event_filter_json,
        daily_report_enabled,
        daily_report_time,
    )
    .await
}

#[cfg(feature = "tauri-runtime")]
#[tauri::command]
pub async fn delete_chat_channel(
    db: tauri::State<'_, AppDatabase>,
    manager: tauri::State<'_, ChatChannelManager>,
    id: i32,
) -> Result<(), AppCommandError> {
    delete_chat_channel_core(&db, &manager, id).await
}

#[cfg(feature = "tauri-runtime")]
#[tauri::command]
pub async fn save_chat_channel_token(
    channel_id: i32,
    token: String,
) -> Result<(), AppCommandError> {
    save_chat_channel_token_core(channel_id, &token)
}

#[cfg(feature = "tauri-runtime")]
#[tauri::command]
pub async fn get_chat_channel_has_token(channel_id: i32) -> Result<bool, AppCommandError> {
    get_chat_channel_has_token_core(channel_id)
}

#[cfg(feature = "tauri-runtime")]
#[tauri::command]
pub async fn delete_chat_channel_token(channel_id: i32) -> Result<(), AppCommandError> {
    delete_chat_channel_token_core(channel_id)
}

#[cfg(feature = "tauri-runtime")]
#[tauri::command]
pub async fn connect_chat_channel(
    db: tauri::State<'_, AppDatabase>,
    manager: tauri::State<'_, ChatChannelManager>,
    id: i32,
) -> Result<(), AppCommandError> {
    connect_chat_channel_core(&db, &manager, id).await
}

#[cfg(feature = "tauri-runtime")]
#[tauri::command]
pub async fn disconnect_chat_channel(
    manager: tauri::State<'_, ChatChannelManager>,
    id: i32,
) -> Result<(), AppCommandError> {
    disconnect_chat_channel_core(&manager, id).await
}

#[cfg(feature = "tauri-runtime")]
#[tauri::command]
pub async fn test_chat_channel(
    db: tauri::State<'_, AppDatabase>,
    id: i32,
) -> Result<(), AppCommandError> {
    test_chat_channel_core(&db, id).await
}

#[cfg(feature = "tauri-runtime")]
#[tauri::command]
pub async fn get_chat_channel_status(
    manager: tauri::State<'_, ChatChannelManager>,
) -> Result<Vec<ChannelStatusInfo>, AppCommandError> {
    get_chat_channel_status_core(&manager).await
}

#[cfg(feature = "tauri-runtime")]
#[tauri::command]
pub async fn list_chat_channel_messages(
    db: tauri::State<'_, AppDatabase>,
    channel_id: i32,
    limit: Option<u64>,
    offset: Option<u64>,
) -> Result<Vec<ChatChannelMessageLogInfo>, AppCommandError> {
    list_chat_channel_messages_core(&db, channel_id, limit, offset).await
}

#[cfg(feature = "tauri-runtime")]
#[tauri::command]
pub async fn get_chat_command_prefix(
    db: tauri::State<'_, AppDatabase>,
) -> Result<String, AppCommandError> {
    get_chat_command_prefix_core(&db).await
}

#[cfg(feature = "tauri-runtime")]
#[tauri::command]
pub async fn set_chat_command_prefix(
    db: tauri::State<'_, AppDatabase>,
    prefix: String,
) -> Result<(), AppCommandError> {
    set_chat_command_prefix_core(&db, prefix).await
}

#[cfg(feature = "tauri-runtime")]
#[tauri::command]
pub async fn get_chat_event_filter(
    db: tauri::State<'_, AppDatabase>,
) -> Result<Option<Vec<String>>, AppCommandError> {
    get_chat_event_filter_core(&db).await
}

#[cfg(feature = "tauri-runtime")]
#[tauri::command]
pub async fn set_chat_event_filter(
    db: tauri::State<'_, AppDatabase>,
    filter: Option<Vec<String>>,
) -> Result<(), AppCommandError> {
    set_chat_event_filter_core(&db, filter).await
}

#[cfg(feature = "tauri-runtime")]
#[tauri::command]
pub async fn get_chat_event_webhooks(
    db: tauri::State<'_, AppDatabase>,
) -> Result<Vec<WebhookConfig>, AppCommandError> {
    get_chat_event_webhooks_core(&db).await
}

#[cfg(feature = "tauri-runtime")]
#[tauri::command]
pub async fn set_chat_event_webhooks(
    db: tauri::State<'_, AppDatabase>,
    webhooks: Vec<WebhookConfig>,
) -> Result<(), AppCommandError> {
    set_chat_event_webhooks_core(&db, webhooks).await
}

#[cfg(feature = "tauri-runtime")]
#[tauri::command]
pub async fn get_chat_event_top_level_only(
    db: tauri::State<'_, AppDatabase>,
) -> Result<bool, AppCommandError> {
    get_chat_event_top_level_only_core(&db).await
}

#[cfg(feature = "tauri-runtime")]
#[tauri::command]
pub async fn set_chat_event_top_level_only(
    db: tauri::State<'_, AppDatabase>,
    enabled: bool,
) -> Result<(), AppCommandError> {
    set_chat_event_top_level_only_core(&db, enabled).await
}

#[cfg(feature = "tauri-runtime")]
#[tauri::command]
pub async fn get_chat_message_language(
    db: tauri::State<'_, AppDatabase>,
) -> Result<String, AppCommandError> {
    get_chat_message_language_core(&db).await
}

#[cfg(feature = "tauri-runtime")]
#[tauri::command]
pub async fn set_chat_message_language(
    db: tauri::State<'_, AppDatabase>,
    language: String,
) -> Result<(), AppCommandError> {
    set_chat_message_language_core(&db, language).await
}

#[cfg(feature = "tauri-runtime")]
#[tauri::command]
pub async fn weixin_get_qrcode() -> Result<WeixinQrcodeInfo, AppCommandError> {
    weixin_get_qrcode_core().await
}

#[cfg(feature = "tauri-runtime")]
#[tauri::command]
pub async fn weixin_check_qrcode(
    db: tauri::State<'_, AppDatabase>,
    channel_id: i32,
    qrcode: String,
) -> Result<WeixinQrcodeStatusPublic, AppCommandError> {
    weixin_check_qrcode_core(&db, channel_id, &qrcode).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::chat_channel::error::ChatChannelError;
    use crate::chat_channel::traits::ChatChannelBackend;
    use crate::chat_channel::types::{
        ChannelConnectionStatus, IncomingCommand, RichMessage, SentMessageId,
    };
    use crate::db::test_helpers::fresh_in_memory_db;
    use async_trait::async_trait;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::Arc;
    use tokio::sync::{mpsc, Mutex};

    struct QqBotTestBackend {
        app_id: String,
        stopped: Arc<AtomicBool>,
        connected_app_id: Arc<Mutex<Option<String>>>,
    }

    #[async_trait]
    impl ChatChannelBackend for QqBotTestBackend {
        fn channel_type(&self) -> ChannelType {
            ChannelType::QqBot
        }

        async fn start(&self, _: mpsc::Sender<IncomingCommand>) -> Result<(), ChatChannelError> {
            *self.connected_app_id.lock().await = Some(self.app_id.clone());
            Ok(())
        }

        async fn stop(&self) -> Result<(), ChatChannelError> {
            self.stopped.store(true, Ordering::Release);
            Ok(())
        }

        async fn status(&self) -> ChannelConnectionStatus {
            ChannelConnectionStatus::Connecting
        }

        async fn send_message(&self, _: &str) -> Result<SentMessageId, ChatChannelError> {
            Ok(SentMessageId("test".to_string()))
        }

        async fn send_rich_message(
            &self,
            _: &RichMessage,
        ) -> Result<SentMessageId, ChatChannelError> {
            Ok(SentMessageId("test".to_string()))
        }

        async fn test_connection(&self) -> Result<(), ChatChannelError> {
            Ok(())
        }
    }

    async fn connect_test_qqbot(
        db: &AppDatabase,
        manager: &ChatChannelManager,
        id: i32,
        stopped: Arc<AtomicBool>,
        connected_app_id: Arc<Mutex<Option<String>>>,
    ) {
        manager
            .add_channel_with_factory(id, || async {
                let row = chat_channel_service::get_by_id(&db.conn, id)
                    .await
                    .unwrap()
                    .unwrap();
                let config: serde_json::Value = serde_json::from_str(&row.config_json).unwrap();
                let backend: Box<dyn ChatChannelBackend> = Box::new(QqBotTestBackend {
                    app_id: config["app_id"].as_str().unwrap().to_string(),
                    stopped,
                    connected_app_id,
                });
                Ok::<_, ChatChannelError>((id, row.name, ChannelType::QqBot, backend))
            })
            .await
            .expect("connect test QQBot");
    }

    #[tokio::test]
    async fn qqbot_concurrent_updates_cannot_leave_runtime_on_stale_app_id() {
        let db = fresh_in_memory_db().await;
        let row = chat_channel_service::create(
            &db.conn,
            "QQBot".to_string(),
            "qqbot".to_string(),
            r#"{"app_id":"app-a"}"#.to_string(),
            true,
            false,
            None,
        )
        .await
        .unwrap();
        let manager = ChatChannelManager::new();
        let (entered_tx, entered_rx) = tokio::sync::oneshot::channel();
        let (resume_tx, resume_rx) = tokio::sync::oneshot::channel();
        let update_db = AppDatabase {
            conn: db.conn.clone(),
        };
        let update_manager = manager.clone_ref();
        let id = row.id;
        let update_a = tokio::spawn(async move {
            update_chat_channel_core_inner(
                &update_db,
                Some(&update_manager),
                id,
                None,
                None,
                Some(r#"{"app_id":"app-a"}"#.to_string()),
                None,
                None,
                None,
                Some(UpdateTestPause {
                    entered: entered_tx,
                    resume: resume_rx,
                }),
            )
            .await
        });

        entered_rx.await.expect("update A read initial config");
        update_chat_channel_core_with_manager(
            &db,
            &manager,
            id,
            None,
            None,
            Some(r#"{"app_id":"app-b"}"#.to_string()),
            None,
            None,
            None,
        )
        .await
        .expect("update B");
        let stopped_b = Arc::new(AtomicBool::new(false));
        let connected_app_id = Arc::new(Mutex::new(None));
        connect_test_qqbot(
            &db,
            &manager,
            id,
            stopped_b.clone(),
            connected_app_id.clone(),
        )
        .await;
        let generation_b = manager.runtime_generation(id).await.unwrap();
        assert_eq!(connected_app_id.lock().await.as_deref(), Some("app-b"));

        update_chat_channel_core_with_manager(
            &db,
            &manager,
            id,
            None,
            None,
            Some(r#"{"app_id":" app-b "}"#.to_string()),
            None,
            None,
            None,
        )
        .await
        .expect("same normalized App ID");
        assert!(!stopped_b.load(Ordering::Acquire));
        assert_eq!(manager.runtime_generation(id).await, Some(generation_b));

        resume_tx.send(()).expect("resume update A");
        update_a.await.unwrap().expect("update A");
        assert!(stopped_b.load(Ordering::Acquire));
        assert!(!manager.is_runtime_current(id, generation_b).await);
        assert_eq!(manager.runtime_generation(id).await, None);
        let persisted = chat_channel_service::get_by_id(&db.conn, id)
            .await
            .unwrap()
            .unwrap();
        let config: serde_json::Value = serde_json::from_str(&persisted.config_json).unwrap();
        assert_eq!(config["app_id"], "app-a");

        connect_test_qqbot(
            &db,
            &manager,
            id,
            Arc::new(AtomicBool::new(false)),
            connected_app_id.clone(),
        )
        .await;
        assert_eq!(
            connected_app_id.lock().await.as_deref(),
            config["app_id"].as_str()
        );
    }

    #[tokio::test]
    async fn list_chat_channels_is_empty_on_fresh_db() {
        let db = fresh_in_memory_db().await;
        let channels = list_chat_channels_core(&db).await.expect("list");
        assert!(channels.is_empty());
    }

    #[tokio::test]
    async fn create_then_list_chat_channel_roundtrip() {
        let db = fresh_in_memory_db().await;
        let created = create_chat_channel_core(
            &db,
            "test-channel".to_string(),
            "telegram".to_string(),
            "{}".to_string(),
            true,
            false,
            None,
        )
        .await
        .expect("create");
        assert_eq!(created.name, "test-channel");
        assert_eq!(created.channel_type, "telegram");

        let channels = list_chat_channels_core(&db).await.expect("list");
        assert_eq!(channels.len(), 1);
        assert_eq!(channels[0].id, created.id);
    }

    #[tokio::test]
    async fn create_chat_channel_rejects_invalid_type() {
        let db = fresh_in_memory_db().await;
        let result = create_chat_channel_core(
            &db,
            "x".to_string(),
            "not-a-real-channel-kind".to_string(),
            "{}".to_string(),
            true,
            false,
            None,
        )
        .await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn chat_command_prefix_default_and_roundtrip() {
        let db = fresh_in_memory_db().await;
        let default_prefix = get_chat_command_prefix_core(&db)
            .await
            .expect("get default");
        assert_eq!(default_prefix, DEFAULT_COMMAND_PREFIX);

        set_chat_command_prefix_core(&db, "$".to_string())
            .await
            .expect("set");
        let updated = get_chat_command_prefix_core(&db)
            .await
            .expect("get after set");
        assert_eq!(updated, "$");
    }

    #[tokio::test]
    async fn chat_command_prefix_rejects_alphanumeric() {
        let db = fresh_in_memory_db().await;
        assert!(set_chat_command_prefix_core(&db, "a".to_string())
            .await
            .is_err());
        assert!(set_chat_command_prefix_core(&db, "".to_string())
            .await
            .is_err());
        assert!(set_chat_command_prefix_core(&db, "$$$$".to_string())
            .await
            .is_err());
    }

    #[tokio::test]
    async fn chat_message_language_default_is_en() {
        let db = fresh_in_memory_db().await;
        let lang = get_chat_message_language_core(&db).await.expect("get");
        assert_eq!(lang, "en");
    }

    #[tokio::test]
    async fn chat_message_language_validates_supported_codes() {
        let db = fresh_in_memory_db().await;
        assert!(set_chat_message_language_core(&db, "zh-CN".to_string())
            .await
            .is_ok());
        let stored = get_chat_message_language_core(&db).await.expect("get");
        // Implementation lowercases before storing.
        assert_eq!(stored, "zh-cn");

        assert!(set_chat_message_language_core(&db, "klingon".to_string())
            .await
            .is_err());
    }

    #[tokio::test]
    async fn chat_event_filter_none_by_default() {
        let db = fresh_in_memory_db().await;
        let filter = get_chat_event_filter_core(&db).await.expect("get");
        assert!(filter.is_none());
    }

    #[tokio::test]
    async fn chat_event_filter_roundtrip_some_and_none() {
        let db = fresh_in_memory_db().await;
        set_chat_event_filter_core(
            &db,
            Some(vec!["session_started".into(), "turn_complete".into()]),
        )
        .await
        .expect("set some");
        let got = get_chat_event_filter_core(&db).await.expect("get some");
        assert_eq!(
            got.as_deref(),
            Some(["session_started".to_string(), "turn_complete".to_string()].as_slice())
        );

        set_chat_event_filter_core(&db, None)
            .await
            .expect("set none");
        let got_none = get_chat_event_filter_core(&db).await.expect("get none");
        assert!(got_none.is_none());
    }

    #[tokio::test]
    async fn chat_event_top_level_only_defaults_off_and_roundtrips() {
        let db = fresh_in_memory_db().await;
        assert!(!get_chat_event_top_level_only_core(&db).await.expect("get"));

        set_chat_event_top_level_only_core(&db, true)
            .await
            .expect("set on");
        assert!(get_chat_event_top_level_only_core(&db)
            .await
            .expect("get on"));

        set_chat_event_top_level_only_core(&db, false)
            .await
            .expect("set off");
        assert!(!get_chat_event_top_level_only_core(&db)
            .await
            .expect("get off"));
    }

    fn hook(url: &str, enabled: bool) -> WebhookConfig {
        WebhookConfig {
            url: url.to_string(),
            enabled,
        }
    }

    #[tokio::test]
    async fn chat_event_webhooks_empty_by_default() {
        let db = fresh_in_memory_db().await;
        let hooks = get_chat_event_webhooks_core(&db).await.expect("get");
        assert!(hooks.is_empty());
    }

    #[tokio::test]
    async fn chat_event_webhooks_roundtrip_preserves_enabled() {
        let db = fresh_in_memory_db().await;
        set_chat_event_webhooks_core(
            &db,
            vec![
                hook("https://example.com/hook", true),
                hook("http://localhost:9000/in", false),
            ],
        )
        .await
        .expect("set");
        let got = get_chat_event_webhooks_core(&db).await.expect("get");
        assert_eq!(
            got,
            vec![
                hook("https://example.com/hook", true),
                hook("http://localhost:9000/in", false),
            ]
        );
    }

    #[tokio::test]
    async fn chat_event_webhooks_trims_drops_empty_and_dedups() {
        let db = fresh_in_memory_db().await;
        set_chat_event_webhooks_core(
            &db,
            vec![
                hook("  https://a.test/h  ", true),
                hook("", true),
                hook("   ", false),
                hook("https://a.test/h", false), // duplicate of the trimmed first; first wins
                hook("https://b.test/h", true),
            ],
        )
        .await
        .expect("set");
        let got = get_chat_event_webhooks_core(&db).await.expect("get");
        assert_eq!(
            got,
            vec![
                hook("https://a.test/h", true),
                hook("https://b.test/h", true)
            ]
        );
    }

    #[tokio::test]
    async fn chat_event_webhooks_rejects_non_http_scheme() {
        let db = fresh_in_memory_db().await;
        assert!(
            set_chat_event_webhooks_core(&db, vec![hook("ftp://x.test/h", true)])
                .await
                .is_err()
        );
    }

    #[tokio::test]
    async fn chat_event_webhooks_rejects_unparseable_url() {
        let db = fresh_in_memory_db().await;
        assert!(
            set_chat_event_webhooks_core(&db, vec![hook("not a url", true)])
                .await
                .is_err()
        );
    }
}
