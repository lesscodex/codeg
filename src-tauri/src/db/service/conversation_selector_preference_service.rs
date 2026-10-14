use std::collections::BTreeMap;

use sea_orm::{ActiveModelTrait, ConnectionTrait, EntityTrait, Set};
use serde::Deserialize;

use crate::db::entities::conversation;
use crate::db::error::DbError;
use crate::models::ConversationSelection;

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ConversationSelectorPreferences {
    pub mode_id: Option<String>,
    pub config_values: BTreeMap<String, String>,
}

pub async fn get(
    conn: &impl ConnectionTrait,
    conversation_id: i32,
) -> Result<Option<ConversationSelectorPreferences>, DbError> {
    let row = conversation::Entity::find_by_id(conversation_id).one(conn).await?;
    Ok(row.and_then(|row| ConversationSelection::parse(row.selector_state.as_deref()))
        .map(|selection| ConversationSelectorPreferences {
            mode_id: selection.composer_mode_id.or(selection.mode_id),
            config_values: selection.config_values,
        }))
}

pub async fn upsert(
    conn: &impl ConnectionTrait,
    conversation_id: i32,
    mode_id: Option<String>,
    config_values: &BTreeMap<String, String>,
) -> Result<(), DbError> {
    let row = conversation::Entity::find_by_id(conversation_id)
        .one(conn).await?
        .ok_or_else(|| DbError::Validation(format!("Conversation not found: {conversation_id}")))?;
    let selection = ConversationSelection {
        mode_id,
        config_values: config_values.clone(),
        ..Default::default()
    };
    let mut active: conversation::ActiveModel = row.into();
    active.selector_state = Set(Some(selection.to_json()));
    active.update(conn).await?;
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
