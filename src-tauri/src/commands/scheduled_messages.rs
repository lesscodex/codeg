use std::path::Path;

use chrono::Utc;
use sea_orm::TransactionTrait;
use serde::{Deserialize, Serialize};

use crate::app_error::AppCommandError;
use crate::commands::conversations::{
    create_chat_conversation_core, create_conversation_core, emit_conversation_upsert,
};
use crate::db::entities::conversation::ConversationStatus;
use crate::db::entities::scheduled_message::Status;
use crate::db::error::DbError;
use crate::db::service::{
    conversation_selector_preference_service::{self, ConversationSelectorPreferences},
    conversation_service, scheduled_message_service as store,
};
use crate::db::AppDatabase;
use crate::models::{AgentType, FolderDetail};
use crate::scheduled_messages::{live_runtime_started_at, CHANGED_EVENT};
use crate::web::event_bridge::{emit_event, EventEmitter};

#[derive(Deserialize)]
#[serde(
    tag = "kind",
    rename_all = "snake_case",
    rename_all_fields = "camelCase"
)]
pub enum Target {
    Existing {
        conversation_id: i32,
    },
    Project {
        folder_id: i32,
        agent_type: AgentType,
        title: String,
    },
    Chat {
        agent_type: AgentType,
        title: String,
        existing_dir: Option<String>,
    },
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ScheduleRequest {
    pub target: Target,
    pub draft: store::Prompt,
    pub scheduled_at: i64,
    pub initial_connection_id: Option<String>,
    pub selector_preferences: Option<ConversationSelectorPreferences>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ScheduleResult {
    pub conversation_id: i32,
    pub folder: Option<FolderDetail>,
}

pub async fn schedule_message_core(
    db: &AppDatabase,
    emitter: &EventEmitter,
    data_dir: &Path,
    request: ScheduleRequest,
) -> Result<ScheduleResult, AppCommandError> {
    store::validate_time(request.scheduled_at, Utc::now().timestamp_millis())?;
    if request.draft.blocks.is_empty() {
        return Err(DbError::Validation("scheduled prompt must contain content".into()).into());
    }
    if let Target::Existing { conversation_id } = request.target {
        conversation_service::get_by_id(&db.conn, conversation_id).await?;
        store::create(
            &db.conn,
            conversation_id,
            request.draft,
            request.scheduled_at,
            false,
            None,
        )
        .await?;
        emit_event(emitter, CHANGED_EVENT, ());
        return Ok(ScheduleResult {
            conversation_id,
            folder: None,
        });
    }

    let txn = db.conn.begin().await.map_err(DbError::from)?;
    let (conversation_id, folder) = match request.target {
        Target::Existing { .. } => unreachable!(),
        Target::Project {
            folder_id,
            agent_type,
            title,
        } => (
            create_conversation_core(&txn, folder_id, agent_type, Some(title)).await?,
            None,
        ),
        Target::Chat {
            agent_type,
            title,
            existing_dir,
        } => {
            let result = create_chat_conversation_core(
                &txn,
                data_dir,
                agent_type,
                Some(title),
                existing_dir.as_deref(),
            )
            .await?;
            (result.conversation_id, Some(result.folder))
        }
    };
    conversation_service::update_status(&txn, conversation_id, ConversationStatus::Completed)
        .await?;
    store::create(
        &txn,
        conversation_id,
        request.draft,
        request.scheduled_at,
        true,
        request.initial_connection_id,
    )
    .await?;
    if let Some(preferences) = request.selector_preferences {
        conversation_selector_preference_service::upsert(
            &txn,
            conversation_id,
            preferences.mode_id,
            &preferences.config_values,
        )
        .await?;
    }
    txn.commit().await.map_err(DbError::from)?;
    emit_conversation_upsert(emitter, &db.conn, conversation_id).await;
    emit_event(emitter, CHANGED_EVENT, ());
    Ok(ScheduleResult {
        conversation_id,
        folder,
    })
}

pub async fn list_scheduled_messages_core(
    db: &AppDatabase,
    conversation_id: i32,
) -> Result<Vec<store::ScheduledMessage>, AppCommandError> {
    Ok(store::list(&db.conn, conversation_id).await?)
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LegacyMessage {
    id: String,
    scope_id: String,
    draft: serde_json::Value,
    scheduled_at: i64,
    status: String,
}

pub async fn import_legacy_scheduled_message_core(
    db: &AppDatabase,
    emitter: &EventEmitter,
    data_dir: &Path,
    message: LegacyMessage,
) -> Result<bool, AppCommandError> {
    let Some(conversation_id) = message
        .scope_id
        .strip_prefix("conversation:")
        .filter(|id| id.bytes().all(|b| b.is_ascii_digit()))
        .and_then(|id| id.parse::<i32>().ok())
        .filter(|id| *id > 0)
    else {
        return Ok(false);
    };
    if message.id.is_empty()
        || chrono::DateTime::from_timestamp_millis(message.scheduled_at).is_none()
    {
        return Ok(false);
    }
    let Ok(draft) = serde_json::from_value::<store::Prompt>(message.draft) else {
        return Ok(false);
    };
    if draft.blocks.is_empty() {
        return Ok(false);
    }
    let status = match message.status.as_str() {
        "missed" => Status::Missed,
        "failed" => Status::Failed,
        "scheduled" | "waiting" => {
            let started_at = live_runtime_started_at(data_dir).map_err(DbError::from)?;
            if message.scheduled_at <= started_at {
                Status::Missed
            } else if message.status == "waiting" {
                Status::Waiting
            } else {
                Status::Scheduled
            }
        }
        _ => return Ok(false),
    };
    match store::import_legacy(
        &db.conn,
        &message.id,
        conversation_id,
        draft,
        message.scheduled_at,
        status,
    )
    .await?
    {
        store::ImportResult::Created => {
            emit_event(emitter, CHANGED_EVENT, ());
            Ok(true)
        }
        store::ImportResult::AlreadyImported => Ok(true),
        store::ImportResult::Unresolved => Ok(false),
    }
}

pub async fn reschedule_message_core(
    db: &AppDatabase,
    emitter: &EventEmitter,
    id: String,
    scheduled_at: i64,
) -> Result<(), AppCommandError> {
    store::reschedule(&db.conn, id, scheduled_at, Utc::now().timestamp_millis()).await?;
    emit_event(emitter, CHANGED_EVENT, ());
    Ok(())
}

pub async fn cancel_scheduled_message_core(
    db: &AppDatabase,
    emitter: &EventEmitter,
    id: String,
) -> Result<(), AppCommandError> {
    store::cancel(&db.conn, id).await?;
    emit_event(emitter, CHANGED_EVENT, ());
    Ok(())
}

#[cfg(feature = "tauri-runtime")]
#[tauri::command]
pub async fn schedule_message(
    app: tauri::AppHandle,
    db: tauri::State<'_, AppDatabase>,
    request: ScheduleRequest,
) -> Result<ScheduleResult, AppCommandError> {
    let data_dir = scheduled_data_dir(&app)?;
    schedule_message_core(&db, &EventEmitter::from_app(&app), &data_dir, request).await
}

#[cfg(feature = "tauri-runtime")]
fn scheduled_data_dir(app: &tauri::AppHandle) -> Result<std::path::PathBuf, DbError> {
    use tauri::Manager;
    app.path()
        .app_data_dir()
        .map(|path| crate::paths::resolve_effective_data_dir(&path))
        .map_err(|err| DbError::Validation(err.to_string()))
}

#[cfg(feature = "tauri-runtime")]
#[tauri::command]
pub async fn import_legacy_scheduled_message(
    app: tauri::AppHandle,
    db: tauri::State<'_, AppDatabase>,
    message: LegacyMessage,
) -> Result<bool, AppCommandError> {
    use sea_orm::{EntityTrait, PaginatorTrait};

    // Desktop local and remote windows shared one storage origin in v1.
    // A bare conversation ID cannot identify the database in that profile.
    if crate::db::entities::remote_workspace_connection::Entity::find()
        .count(&db.conn)
        .await
        .map_err(DbError::from)?
        != 0
    {
        return Ok(false);
    }
    let data_dir = scheduled_data_dir(&app)?;
    import_legacy_scheduled_message_core(&db, &EventEmitter::from_app(&app), &data_dir, message)
        .await
}

#[cfg(feature = "tauri-runtime")]
#[tauri::command]
pub async fn list_scheduled_messages(
    db: tauri::State<'_, AppDatabase>,
    conversation_id: i32,
) -> Result<Vec<store::ScheduledMessage>, AppCommandError> {
    list_scheduled_messages_core(&db, conversation_id).await
}

#[cfg(feature = "tauri-runtime")]
#[tauri::command]
pub async fn reschedule_message(
    app: tauri::AppHandle,
    db: tauri::State<'_, AppDatabase>,
    id: String,
    scheduled_at: i64,
) -> Result<(), AppCommandError> {
    reschedule_message_core(&db, &EventEmitter::from_app(&app), id, scheduled_at).await
}

#[cfg(feature = "tauri-runtime")]
#[tauri::command]
pub async fn cancel_scheduled_message(
    app: tauri::AppHandle,
    db: tauri::State<'_, AppDatabase>,
    id: String,
) -> Result<(), AppCommandError> {
    cancel_scheduled_message_core(&db, &EventEmitter::from_app(&app), id).await
}
