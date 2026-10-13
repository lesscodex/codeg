use std::collections::BTreeMap;

use chrono::Utc;
use sea_orm::sea_query::OnConflict;
use sea_orm::{EntityTrait, Set};

use crate::db::entities::conversation_selector_preference;
use crate::db::error::DbError;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConversationSelectorPreferences {
    pub mode_id: Option<String>,
    pub config_values: BTreeMap<String, String>,
}

pub async fn get(
    conn: &sea_orm::DatabaseConnection,
    conversation_id: i32,
) -> Result<Option<ConversationSelectorPreferences>, DbError> {
    let Some(row) = conversation_selector_preference::Entity::find_by_id(conversation_id)
        .one(conn)
        .await?
    else {
        return Ok(None);
    };
    let config_values = serde_json::from_str(&row.preferred_config_values).map_err(|error| {
        DbError::Validation(format!(
            "invalid selector preferences for conversation {conversation_id}: {error}"
        ))
    })?;
    Ok(Some(ConversationSelectorPreferences {
        mode_id: row.preferred_mode_id,
        config_values,
    }))
}

pub async fn upsert(
    conn: &sea_orm::DatabaseConnection,
    conversation_id: i32,
    mode_id: Option<String>,
    config_values: &BTreeMap<String, String>,
) -> Result<(), DbError> {
    let config_values = serde_json::to_string(config_values).map_err(|error| {
        DbError::Validation(format!(
            "failed to encode selector preferences for conversation {conversation_id}: {error}"
        ))
    })?;
    let now = Utc::now();
    conversation_selector_preference::Entity::insert(
        conversation_selector_preference::ActiveModel {
            conversation_id: Set(conversation_id),
            preferred_mode_id: Set(mode_id),
            preferred_config_values: Set(config_values),
            created_at: Set(now),
            updated_at: Set(now),
        },
    )
    .on_conflict(
        OnConflict::column(conversation_selector_preference::Column::ConversationId)
            .update_columns([
                conversation_selector_preference::Column::PreferredModeId,
                conversation_selector_preference::Column::PreferredConfigValues,
                conversation_selector_preference::Column::UpdatedAt,
            ])
            .to_owned(),
    )
    .exec(conn)
    .await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::service::conversation_service;
    use crate::db::test_helpers::{fresh_in_memory_db, seed_folder};
    use crate::models::AgentType;

    #[tokio::test]
    async fn upsert_round_trips_and_replaces_selector_preferences() {
        let db = fresh_in_memory_db().await;
        let folder_id = seed_folder(&db, "/tmp/prefs").await;
        let conversation =
            conversation_service::create(&db.conn, folder_id, AgentType::Codex, None, None)
                .await
                .expect("create conversation");

        let initial = BTreeMap::from([
            ("model".into(), "gpt-a".into()),
            ("effort".into(), "high".into()),
        ]);
        upsert(&db.conn, conversation.id, Some("plan".into()), &initial)
            .await
            .expect("insert preferences");
        assert_eq!(
            get(&db.conn, conversation.id).await.expect("read"),
            Some(ConversationSelectorPreferences {
                mode_id: Some("plan".into()),
                config_values: initial,
            })
        );

        let replacement = BTreeMap::from([("model".into(), "gpt-b".into())]);
        upsert(&db.conn, conversation.id, None, &replacement)
            .await
            .expect("replace preferences");
        assert_eq!(
            get(&db.conn, conversation.id)
                .await
                .expect("read replacement"),
            Some(ConversationSelectorPreferences {
                mode_id: None,
                config_values: replacement,
            })
        );
    }
}
