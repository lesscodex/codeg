use sea_orm::sea_query::Expr;
use sea_orm::{
    ActiveModelTrait, ColumnTrait, Condition, ConnectionTrait, DatabaseConnection, DbBackend,
    EntityTrait, QueryFilter, QueryOrder, QuerySelect, Set, Statement, TransactionTrait,
};
use serde::{Deserialize, Serialize};

use crate::acp::types::PromptInputBlock;
use crate::db::entities::conversation;
use crate::db::entities::scheduled_message::{ActiveModel, Column, Entity, Model, Status};
use crate::db::error::DbError;

#[derive(Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Prompt {
    pub blocks: Vec<PromptInputBlock>,
    pub display_text: String,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ScheduledMessage {
    pub id: String,
    pub conversation_id: i32,
    pub scheduled_at: i64,
    pub status: Status,
    pub draft: Prompt,
    pub error: Option<String>,
}

impl TryFrom<Model> for ScheduledMessage {
    type Error = DbError;

    fn try_from(row: Model) -> Result<Self, DbError> {
        let draft = decode_prompt(&row)?;
        Ok(Self {
            id: row.id,
            conversation_id: row.conversation_id,
            scheduled_at: row.scheduled_at,
            status: row.status,
            draft,
            error: row.error,
        })
    }
}

pub fn decode_prompt(row: &Model) -> Result<Prompt, DbError> {
    serde_json::from_str(&row.prompt).map_err(|err| DbError::Validation(err.to_string()))
}

pub fn validate_time(scheduled_at: i64, now: i64) -> Result<(), DbError> {
    if scheduled_at <= now || chrono::DateTime::from_timestamp_millis(scheduled_at).is_none() {
        return Err(DbError::Validation(
            "scheduled time must be in the future".into(),
        ));
    }
    Ok(())
}

pub async fn create(
    db: &impl ConnectionTrait,
    conversation_id: i32,
    prompt: Prompt,
    scheduled_at: i64,
    first_message: bool,
    initial_connection_id: Option<String>,
) -> Result<(), DbError> {
    ActiveModel {
        first_message: Set(first_message),
        initial_connection_id: Set(initial_connection_id),
        ..new_row(
            uuid::Uuid::new_v4().to_string(),
            conversation_id,
            prompt,
            scheduled_at,
            Status::Scheduled,
        )?
    }
    .insert(db)
    .await?;
    Ok(())
}

fn new_row(
    id: String,
    conversation_id: i32,
    prompt: Prompt,
    scheduled_at: i64,
    status: Status,
) -> Result<ActiveModel, DbError> {
    let prompt =
        serde_json::to_string(&prompt).map_err(|err| DbError::Validation(err.to_string()))?;
    Ok(ActiveModel {
        id: Set(id),
        conversation_id: Set(conversation_id),
        prompt: Set(prompt),
        scheduled_at: Set(scheduled_at),
        retry_at: Set(None),
        status: Set(status),
        first_message: Set(false),
        initial_connection_id: Set(None),
        error: Set(None),
    })
}

pub(crate) enum ImportResult {
    Created,
    AlreadyImported,
    Unresolved,
}

pub(crate) async fn import_legacy(
    db: &DatabaseConnection,
    legacy_id: &str,
    conversation_id: i32,
    prompt: Prompt,
    scheduled_at: i64,
    status: Status,
) -> Result<ImportResult, DbError> {
    let id = format!("legacy-v1:{legacy_id}");
    let txn = db.begin().await?;
    // This receipt outlives the queue row, including after delivery or cancel.
    // Claim it first so concurrent SQLite writers serialize before any reads.
    let claim = txn
        .execute(Statement::from_sql_and_values(
            DbBackend::Sqlite,
            "INSERT INTO scheduled_message_legacy_import (id) VALUES (?) ON CONFLICT(id) DO NOTHING",
            [id.clone().into()],
        ))
        .await?;
    if claim.rows_affected() == 0 {
        txn.commit().await?;
        return Ok(ImportResult::AlreadyImported);
    }

    let exists = conversation::Entity::find_by_id(conversation_id)
        .filter(conversation::Column::DeletedAt.is_null())
        .one(&txn)
        .await?
        .is_some();
    if !exists {
        txn.rollback().await?;
        return Ok(ImportResult::Unresolved);
    }

    new_row(id, conversation_id, prompt, scheduled_at, status)?
        .insert(&txn)
        .await?;
    txn.commit().await?;
    Ok(ImportResult::Created)
}

pub async fn list(
    db: &DatabaseConnection,
    conversation_id: i32,
) -> Result<Vec<ScheduledMessage>, DbError> {
    Entity::find()
        .filter(Column::ConversationId.eq(conversation_id))
        .order_by_asc(Column::ScheduledAt)
        .order_by_asc(Column::Id)
        .all(db)
        .await?
        .into_iter()
        .map(ScheduledMessage::try_from)
        .collect()
}

pub(crate) async fn revisions(
    db: &DatabaseConnection,
) -> Result<Vec<(String, i32, i64, Status)>, DbError> {
    Entity::find()
        .select_only()
        .columns([
            Column::Id,
            Column::ConversationId,
            Column::ScheduledAt,
            Column::Status,
        ])
        .order_by_asc(Column::Id)
        .into_tuple()
        .all(db)
        .await
        .map_err(Into::into)
}

pub async fn reschedule(
    db: &DatabaseConnection,
    id: String,
    scheduled_at: i64,
    now: i64,
) -> Result<(), DbError> {
    validate_time(scheduled_at, now)?;
    let changed = Entity::update_many()
        .col_expr(Column::ScheduledAt, Expr::value(scheduled_at))
        .col_expr(Column::Status, Expr::value(Status::Scheduled))
        .col_expr(Column::RetryAt, Expr::value(None::<i64>))
        .col_expr(Column::Error, Expr::value(None::<String>))
        .filter(Column::Id.eq(id))
        .filter(Column::Status.ne(Status::Dispatching))
        .exec(db)
        .await?;
    require_editable(changed.rows_affected)
}

pub async fn cancel(db: &DatabaseConnection, id: String) -> Result<(), DbError> {
    let changed = Entity::delete_many()
        .filter(Column::Id.eq(id))
        .filter(Column::Status.ne(Status::Dispatching))
        .exec(db)
        .await?;
    require_editable(changed.rows_affected)
}

fn require_editable(rows: u64) -> Result<(), DbError> {
    if rows == 0 {
        return Err(DbError::Conflict(
            "scheduled message no longer available for editing".into(),
        ));
    }
    Ok(())
}

fn due_condition(now: i64) -> Condition {
    Condition::all()
        .add(Column::Status.is_in([Status::Scheduled, Status::Waiting]))
        .add(Column::ScheduledAt.lte(now))
        .add(
            Condition::any()
                .add(Column::RetryAt.is_null())
                .add(Column::RetryAt.lte(now)),
        )
}

pub(crate) async fn due(db: &DatabaseConnection, now: i64) -> Result<Vec<Model>, DbError> {
    Ok(Entity::find()
        .filter(due_condition(now))
        .order_by_asc(Column::ScheduledAt)
        .order_by_asc(Column::Id)
        .all(db)
        .await?)
}

pub(crate) async fn claim(db: &DatabaseConnection, row: &Model, now: i64) -> Result<bool, DbError> {
    let changed = Entity::update_many()
        .col_expr(Column::Status, Expr::value(Status::Dispatching))
        .filter(Column::Id.eq(&row.id))
        .filter(Column::ScheduledAt.eq(row.scheduled_at))
        .filter(due_condition(now))
        .exec(db)
        .await?;
    Ok(changed.rows_affected == 1)
}

pub(crate) enum Outcome {
    Sent,
    Waiting { retry_at: i64 },
    Failed(String),
}

pub(crate) async fn settle(
    db: &DatabaseConnection,
    id: &str,
    outcome: &Outcome,
) -> Result<(), DbError> {
    let (status, retry_at, error) = match outcome {
        Outcome::Sent => {
            Entity::delete_many()
                .filter(Column::Id.eq(id))
                .filter(Column::Status.eq(Status::Dispatching))
                .exec(db)
                .await?;
            return Ok(());
        }
        Outcome::Waiting { retry_at } => (Status::Waiting, Some(*retry_at), None),
        Outcome::Failed(error) => (Status::Failed, None, Some(error.as_str())),
    };
    Entity::update_many()
        .col_expr(Column::Status, Expr::value(status))
        .col_expr(Column::RetryAt, Expr::value(retry_at))
        .col_expr(Column::Error, Expr::value(error))
        .filter(Column::Id.eq(id))
        .filter(Column::Status.eq(Status::Dispatching))
        .exec(db)
        .await?;
    Ok(())
}

pub(crate) async fn mark_missed(db: &DatabaseConnection, started_at: i64) -> Result<(), DbError> {
    Entity::update_many()
        .col_expr(Column::Status, Expr::value(Status::Missed))
        .filter(Column::Status.is_in([Status::Scheduled, Status::Waiting]))
        .filter(Column::ScheduledAt.lte(started_at))
        .exec(db)
        .await?;
    Ok(())
}

pub(crate) async fn fail_interrupted(db: &DatabaseConnection) -> Result<(), DbError> {
    // Admission may have succeeded before the process exited. Replaying an
    // ambiguous claim could send the same prompt twice.
    Entity::update_many()
        .col_expr(Column::Status, Expr::value(Status::Failed))
        .col_expr(
            Column::Error,
            Expr::value("delivery interrupted before confirmation"),
        )
        .filter(Column::Status.eq(Status::Dispatching))
        .exec(db)
        .await?;
    Ok(())
}
