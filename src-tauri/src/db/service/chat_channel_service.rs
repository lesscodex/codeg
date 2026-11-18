use chrono::Utc;
use sea_orm::sea_query::Expr;
use sea_orm::TransactionTrait;
use sea_orm::{
    ActiveModelTrait, ActiveValue::NotSet, ColumnTrait, DatabaseConnection, EntityTrait,
    IntoActiveModel, QueryFilter, QueryOrder, Set,
};

use crate::db::entities::chat_channel;
use crate::db::error::DbError;

pub async fn create(
    conn: &DatabaseConnection,
    name: String,
    channel_type: String,
    config_json: String,
    enabled: bool,
    daily_report_enabled: bool,
    daily_report_time: Option<String>,
) -> Result<chat_channel::Model, DbError> {
    let now = Utc::now();
    let config_json = if is_qqbot_type(&channel_type) {
        normalize_qqbot_config_json(&config_json)?
    } else {
        config_json
    };
    let active = chat_channel::ActiveModel {
        id: NotSet,
        name: Set(name),
        channel_type: Set(channel_type),
        enabled: Set(enabled),
        config_json: Set(config_json),
        event_filter_json: Set(None),
        daily_report_enabled: Set(daily_report_enabled),
        daily_report_time: Set(daily_report_time),
        created_at: Set(now),
        updated_at: Set(now),
    };
    Ok(active.insert(conn).await?)
}

#[allow(clippy::too_many_arguments)]
pub async fn update(
    conn: &DatabaseConnection,
    id: i32,
    name: Option<String>,
    enabled: Option<bool>,
    config_json: Option<String>,
    event_filter_json: Option<Option<String>>,
    daily_report_enabled: Option<bool>,
    daily_report_time: Option<Option<String>>,
) -> Result<chat_channel::Model, DbError> {
    let model = chat_channel::Entity::find_by_id(id)
        .one(conn)
        .await?
        .ok_or_else(|| DbError::Migration(format!("chat channel not found: {id}")))?;

    let config_json = if let Some(raw) = config_json {
        if is_qqbot_type(&model.channel_type) {
            let raw = normalize_qqbot_config_json(&raw)?;
            let current = serde_json::from_str::<serde_json::Value>(&model.config_json).ok();
            let mut next = serde_json::from_str::<serde_json::Value>(&raw).ok();
            let current_app_id = current
                .as_ref()
                .and_then(|value| value.get("app_id"))
                .and_then(serde_json::Value::as_str)
                .map(str::trim);
            let next_app_id = next
                .as_ref()
                .and_then(|value| value.get("app_id"))
                .and_then(serde_json::Value::as_str)
                .map(str::trim);
            if current_app_id != next_app_id {
                if let Some(config) = next.as_mut() {
                    // OpenIDs are scoped to the bot AppID. Never let a stale
                    // owner survive an AppID change, even for older clients
                    // that do not expose the explicit reset control.
                    if let Some(object) = config.as_object_mut() {
                        object.remove("owner_openid");
                    }
                    Some(serde_json::to_string(config).unwrap_or(raw))
                } else {
                    Some(raw)
                }
            } else {
                Some(raw)
            }
        } else {
            Some(raw)
        }
    } else {
        None
    };

    let mut active = model.into_active_model();
    if let Some(v) = name {
        active.name = Set(v);
    }
    if let Some(v) = enabled {
        active.enabled = Set(v);
    }
    if let Some(v) = config_json {
        active.config_json = Set(v);
    }
    if let Some(v) = event_filter_json {
        active.event_filter_json = Set(v);
    }
    if let Some(v) = daily_report_enabled {
        active.daily_report_enabled = Set(v);
    }
    if let Some(v) = daily_report_time {
        active.daily_report_time = Set(v);
    }
    active.updated_at = Set(Utc::now());
    Ok(active.update(conn).await?)
}

fn is_qqbot_type(channel_type: &str) -> bool {
    channel_type == "qqbot" || channel_type == "qq_bot"
}

fn normalize_qqbot_config_json(raw: &str) -> Result<String, DbError> {
    let mut config: serde_json::Value = serde_json::from_str(raw)
        .map_err(|e| DbError::Validation(format!("invalid QQBot config: {e}")))?;
    let object = config
        .as_object_mut()
        .ok_or_else(|| DbError::Validation("QQBot config must be a JSON object".to_string()))?;
    let app_id = object
        .get("app_id")
        .and_then(serde_json::Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| DbError::Validation("QQBot app_id cannot be empty".to_string()))?;
    object.insert(
        "app_id".to_string(),
        serde_json::Value::String(app_id.to_string()),
    );
    serde_json::to_string(&config)
        .map_err(|e| DbError::Validation(format!("failed to serialize QQBot config: {e}")))
}

fn qqbot_owner(config: &serde_json::Value) -> Option<String> {
    config
        .get("owner_openid")
        .and_then(serde_json::Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(ToString::to_string)
}

/// Return whether changing a QQBot config must invalidate its active runtime.
/// App IDs are compared after trimming, and clearing a durable owner also
/// invalidates the old backend even when the App ID is unchanged.
pub fn qqbot_config_requires_runtime_reset(
    current_json: &str,
    next_json: &str,
) -> Result<bool, DbError> {
    let current: serde_json::Value = serde_json::from_str(current_json)
        .map_err(|e| DbError::Validation(format!("invalid current QQBot config: {e}")))?;
    let next_json = normalize_qqbot_config_json(next_json)?;
    let next: serde_json::Value = serde_json::from_str(&next_json)
        .map_err(|e| DbError::Validation(format!("invalid QQBot config: {e}")))?;
    let current_app_id = current
        .get("app_id")
        .and_then(serde_json::Value::as_str)
        .map(str::trim)
        .unwrap_or_default();
    let next_app_id = next
        .get("app_id")
        .and_then(serde_json::Value::as_str)
        .map(str::trim)
        .unwrap_or_default();
    Ok(current_app_id != next_app_id
        || (qqbot_owner(&current).is_some() && qqbot_owner(&next).is_none()))
}

/// Bind a QQBot channel to the first bot-scoped OpenID received through its
/// private C2C event stream. The binding is deliberately stored in the normal
/// channel config JSON so desktop and server runtimes share the same state.
pub async fn bind_qqbot_owner(
    conn: &DatabaseConnection,
    id: i32,
    owner_openid: &str,
) -> Result<chat_channel::Model, DbError> {
    bind_qqbot_owner_inner(conn, id, owner_openid, None, None).await
}

pub async fn bind_qqbot_owner_for_runtime(
    conn: &DatabaseConnection,
    id: i32,
    owner_openid: &str,
    app_id: &str,
    runtime_owner_openid: Option<&str>,
) -> Result<chat_channel::Model, DbError> {
    bind_qqbot_owner_inner(
        conn,
        id,
        owner_openid,
        Some(app_id),
        Some(runtime_owner_openid),
    )
    .await
}

async fn bind_qqbot_owner_inner(
    conn: &DatabaseConnection,
    id: i32,
    owner_openid: &str,
    expected_app_id: Option<&str>,
    expected_runtime_owner: Option<Option<&str>>,
) -> Result<chat_channel::Model, DbError> {
    let owner_openid = owner_openid.trim();
    if owner_openid.is_empty() {
        return Err(DbError::Validation(
            "QQBot owner OpenID cannot be empty".to_string(),
        ));
    }

    let txn = conn.begin().await?;
    // Acquire SQLite's writer lock before reading the config so two first
    // messages cannot both observe an unbound channel and overwrite each
    // other's owner.
    chat_channel::Entity::update_many()
        .filter(chat_channel::Column::Id.eq(id))
        .col_expr(
            chat_channel::Column::UpdatedAt,
            Expr::col(chat_channel::Column::UpdatedAt).into(),
        )
        .exec(&txn)
        .await?;

    let model = chat_channel::Entity::find_by_id(id)
        .one(&txn)
        .await?
        .ok_or_else(|| DbError::NotFound(format!("chat channel not found: {id}")))?;
    if !is_qqbot_type(&model.channel_type) {
        txn.rollback().await?;
        return Err(DbError::Validation(
            "owner binding is only supported for QQBot channels".to_string(),
        ));
    }

    let mut config: serde_json::Value = serde_json::from_str(&model.config_json)
        .map_err(|e| DbError::Validation(format!("invalid QQBot config: {e}")))?;
    let current_owner = qqbot_owner(&config);
    let object = config
        .as_object_mut()
        .ok_or_else(|| DbError::Validation("QQBot config must be a JSON object".to_string()))?;
    if let Some(expected_app_id) = expected_app_id {
        let current_app_id = object
            .get("app_id")
            .and_then(serde_json::Value::as_str)
            .map(str::trim);
        if current_app_id != Some(expected_app_id.trim()) {
            txn.rollback().await?;
            return Err(DbError::Conflict(
                "QQBot command belongs to a replaced App ID runtime".to_string(),
            ));
        }
        if let Some(Some(expected_runtime_owner)) = expected_runtime_owner {
            if current_owner.as_deref() != Some(expected_runtime_owner.trim()) {
                txn.rollback().await?;
                return Err(DbError::Conflict(
                    "QQBot command belongs to a replaced owner runtime".to_string(),
                ));
            }
        }
    }
    if let Some(existing) = object.get("owner_openid").and_then(|value| value.as_str()) {
        if existing.trim() != owner_openid {
            txn.rollback().await?;
            return Err(DbError::Conflict(
                "QQBot channel is already bound to another owner".to_string(),
            ));
        }
        txn.commit().await?;
        return Ok(model);
    }
    object.insert(
        "owner_openid".to_string(),
        serde_json::Value::String(owner_openid.to_string()),
    );

    let config_json = serde_json::to_string(&config)
        .map_err(|e| DbError::Validation(format!("failed to serialize QQBot config: {e}")))?;
    let mut active = model.into_active_model();
    active.config_json = Set(config_json);
    active.updated_at = Set(Utc::now());
    let updated = active.update(&txn).await?;
    txn.commit().await?;
    Ok(updated)
}

pub async fn delete(conn: &DatabaseConnection, id: i32) -> Result<(), DbError> {
    chat_channel::Entity::delete_by_id(id).exec(conn).await?;
    Ok(())
}

pub async fn get_by_id(
    conn: &DatabaseConnection,
    id: i32,
) -> Result<Option<chat_channel::Model>, DbError> {
    Ok(chat_channel::Entity::find_by_id(id).one(conn).await?)
}

pub async fn list_all(conn: &DatabaseConnection) -> Result<Vec<chat_channel::Model>, DbError> {
    Ok(chat_channel::Entity::find()
        .order_by_asc(chat_channel::Column::Id)
        .all(conn)
        .await?)
}

pub async fn list_enabled(conn: &DatabaseConnection) -> Result<Vec<chat_channel::Model>, DbError> {
    Ok(chat_channel::Entity::find()
        .filter(chat_channel::Column::Enabled.eq(true))
        .order_by_asc(chat_channel::Column::Id)
        .all(conn)
        .await?)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn qqbot_owner_binding_is_persisted_and_isolated() {
        let db = crate::db::test_helpers::fresh_in_memory_db().await;
        let row = create(
            &db.conn,
            "QQBot".to_string(),
            "qqbot".to_string(),
            r#"{"app_id":"app-id"}"#.to_string(),
            true,
            false,
            None,
        )
        .await
        .expect("channel");

        let bound = bind_qqbot_owner(&db.conn, row.id, "owner-openid")
            .await
            .expect("owner binding");
        let config: serde_json::Value = serde_json::from_str(&bound.config_json).unwrap();
        assert_eq!(config["owner_openid"], "owner-openid");

        let same = bind_qqbot_owner(&db.conn, row.id, "owner-openid")
            .await
            .expect("idempotent owner binding");
        assert_eq!(same.config_json, bound.config_json);

        let other = bind_qqbot_owner(&db.conn, row.id, "other-openid").await;
        assert!(matches!(other, Err(DbError::Conflict(_))));
    }

    #[tokio::test]
    async fn qqbot_app_id_change_clears_scoped_owner_binding() {
        let db = crate::db::test_helpers::fresh_in_memory_db().await;
        let row = create(
            &db.conn,
            "QQBot".to_string(),
            "qqbot".to_string(),
            r#"{"app_id":"old-app","owner_openid":"old-owner"}"#.to_string(),
            true,
            false,
            None,
        )
        .await
        .expect("channel");

        let updated = update(
            &db.conn,
            row.id,
            None,
            None,
            Some(r#"{"app_id":"new-app","owner_openid":"old-owner"}"#.to_string()),
            None,
            None,
            None,
        )
        .await
        .expect("app id update");
        let config: serde_json::Value = serde_json::from_str(&updated.config_json).unwrap();
        assert_eq!(config["app_id"], "new-app");
        assert!(config.get("owner_openid").is_none());
    }

    #[tokio::test]
    async fn qqbot_app_id_is_trimmed_and_empty_values_are_rejected() {
        let db = crate::db::test_helpers::fresh_in_memory_db().await;
        let row = create(
            &db.conn,
            "QQBot".to_string(),
            "qqbot".to_string(),
            r#"{"app_id":"  app-id  "}"#.to_string(),
            true,
            false,
            None,
        )
        .await
        .expect("channel");
        let config: serde_json::Value = serde_json::from_str(&row.config_json).unwrap();
        assert_eq!(config["app_id"], "app-id");

        let empty = update(
            &db.conn,
            row.id,
            None,
            None,
            Some(r#"{"app_id":"   "}"#.to_string()),
            None,
            None,
            None,
        )
        .await;
        assert!(matches!(empty, Err(DbError::Validation(message)) if message.contains("app_id")));
    }

    #[test]
    fn qqbot_runtime_reset_compares_normalized_app_ids_and_owner_clears() {
        assert!(!qqbot_config_requires_runtime_reset(
            r#"{"app_id":" app-id ","owner_openid":"owner"}"#,
            r#"{"app_id":"app-id","owner_openid":"owner"}"#,
        )
        .unwrap());
        assert!(qqbot_config_requires_runtime_reset(
            r#"{"app_id":"app-id","owner_openid":"owner"}"#,
            r#"{"app_id":"app-id"}"#,
        )
        .unwrap());
    }

    #[tokio::test]
    async fn stale_runtime_owner_binding_is_rejected_after_reset() {
        let db = crate::db::test_helpers::fresh_in_memory_db().await;
        let row = create(
            &db.conn,
            "QQBot".to_string(),
            "qqbot".to_string(),
            r#"{"app_id":"app-id","owner_openid":"old-owner"}"#.to_string(),
            true,
            false,
            None,
        )
        .await
        .expect("channel");
        let updated = update(
            &db.conn,
            row.id,
            None,
            None,
            Some(r#"{"app_id":"app-id"}"#.to_string()),
            None,
            None,
            None,
        )
        .await
        .expect("reset owner");
        assert!(
            serde_json::from_str::<serde_json::Value>(&updated.config_json)
                .unwrap()
                .get("owner_openid")
                .is_none()
        );

        let result = bind_qqbot_owner_for_runtime(
            &db.conn,
            row.id,
            "old-owner",
            "app-id",
            Some("old-owner"),
        )
        .await;
        assert!(matches!(result, Err(DbError::Conflict(message)) if message.contains("replaced")));
    }

    #[tokio::test]
    async fn unbound_runtime_can_idempotently_bind_same_owner_after_queueing() {
        let db = crate::db::test_helpers::fresh_in_memory_db().await;
        let row = create(
            &db.conn,
            "QQBot".to_string(),
            "qqbot".to_string(),
            r#"{"app_id":"app-id"}"#.to_string(),
            true,
            false,
            None,
        )
        .await
        .expect("channel");

        bind_qqbot_owner_for_runtime(&db.conn, row.id, "owner", "app-id", None)
            .await
            .expect("first owner bind");
        bind_qqbot_owner_for_runtime(&db.conn, row.id, "owner", "app-id", None)
            .await
            .expect("same owner bind");
    }
}
