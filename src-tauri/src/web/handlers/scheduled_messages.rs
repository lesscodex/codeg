use std::sync::Arc;

use axum::{extract::Extension, Json};
use serde::Deserialize;

use crate::app_error::AppCommandError;
use crate::app_state::AppState;
use crate::commands::scheduled_messages as core;
use crate::db::service::scheduled_message_service::ScheduledMessage;

#[derive(Deserialize)]
pub struct ScheduleParams {
    request: core::ScheduleRequest,
}

pub async fn schedule_message(
    Extension(state): Extension<Arc<AppState>>,
    Json(params): Json<ScheduleParams>,
) -> Result<Json<core::ScheduleResult>, AppCommandError> {
    Ok(Json(
        core::schedule_message_core(&state.db, &state.emitter, &state.data_dir, params.request)
            .await?,
    ))
}

#[derive(Deserialize)]
pub struct LegacyImportParams {
    message: core::LegacyMessage,
}

pub async fn import_legacy_scheduled_message(
    Extension(state): Extension<Arc<AppState>>,
    Json(params): Json<LegacyImportParams>,
) -> Result<Json<bool>, AppCommandError> {
    Ok(Json(
        core::import_legacy_scheduled_message_core(
            &state.db,
            &state.emitter,
            &state.data_dir,
            params.message,
        )
        .await?,
    ))
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ListParams {
    conversation_id: i32,
}

pub async fn list_scheduled_messages(
    Extension(state): Extension<Arc<AppState>>,
    Json(params): Json<ListParams>,
) -> Result<Json<Vec<ScheduledMessage>>, AppCommandError> {
    Ok(Json(
        core::list_scheduled_messages_core(&state.db, params.conversation_id).await?,
    ))
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RescheduleParams {
    id: String,
    scheduled_at: i64,
}

pub async fn reschedule_message(
    Extension(state): Extension<Arc<AppState>>,
    Json(params): Json<RescheduleParams>,
) -> Result<Json<()>, AppCommandError> {
    Ok(Json(
        core::reschedule_message_core(&state.db, &state.emitter, params.id, params.scheduled_at)
            .await?,
    ))
}

#[derive(Deserialize)]
pub struct CancelParams {
    id: String,
}

pub async fn cancel_scheduled_message(
    Extension(state): Extension<Arc<AppState>>,
    Json(params): Json<CancelParams>,
) -> Result<Json<()>, AppCommandError> {
    Ok(Json(
        core::cancel_scheduled_message_core(&state.db, &state.emitter, params.id).await?,
    ))
}
