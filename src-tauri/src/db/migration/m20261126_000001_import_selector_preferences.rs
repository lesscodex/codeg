use sea_orm_migration::prelude::*;
use sea_orm_migration::sea_orm::{ConnectionTrait, DbBackend, Statement};

#[derive(DeriveMigrationName)]
pub struct Migration;

// Keep the legacy table for recovery; never overwrite newer session state.
const IMPORT: &str = r#"
UPDATE conversation
SET selector_state = (
    SELECT json_object(
        'modeId', NULL,
        'composerModeId', preferred_mode_id,
        'configValues', json(preferred_config_values)
    )
    FROM conversation_selector_preference
    WHERE conversation_id = conversation.id
)
WHERE selector_state IS NULL
AND id IN (SELECT conversation_id FROM conversation_selector_preference)
"#;

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        manager
            .get_connection()
            .execute(Statement::from_string(DbBackend::Sqlite, IMPORT))
            .await?;
        Ok(())
    }

    async fn down(&self, _manager: &SchemaManager) -> Result<(), DbErr> {
        // Imported values may have been edited since migration; retain them.
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use sea_orm_migration::sea_orm::Database;

    #[tokio::test]
    async fn imports_legacy_preferences_without_overwriting_session_state() {
        let db = Database::connect("sqlite::memory:").await.unwrap();
        db.execute_unprepared("CREATE TABLE conversation (id INTEGER PRIMARY KEY, selector_state TEXT);
            CREATE TABLE conversation_selector_preference (conversation_id INTEGER PRIMARY KEY, preferred_mode_id TEXT, preferred_config_values TEXT);
            INSERT INTO conversation VALUES (1, NULL), (2, '{\"modeId\":\"existing\"}'), (3, NULL);
            INSERT INTO conversation_selector_preference VALUES (1, 'plan', '{\"model\":\"saved-model\"}'), (2, 'old', '{}');")
            .await.unwrap();
        let manager = SchemaManager::new(&db);
        Migration.up(&manager).await.unwrap();
        Migration.up(&manager).await.unwrap();
        let rows = db
            .query_all(Statement::from_string(
                DbBackend::Sqlite,
                "SELECT selector_state FROM conversation ORDER BY id",
            ))
            .await
            .unwrap();
        let imported: String = rows[0].try_get("", "selector_state").unwrap();
        let value: serde_json::Value = serde_json::from_str(&imported).unwrap();
        assert_eq!(value["composerModeId"], "plan");
        assert_eq!(value["configValues"]["model"], "saved-model");
        assert_eq!(value["modeId"], serde_json::Value::Null);
        assert_eq!(
            rows[1].try_get::<String>("", "selector_state").unwrap(),
            "{\"modeId\":\"existing\"}"
        );
        assert_eq!(
            rows[2]
                .try_get::<Option<String>>("", "selector_state")
                .unwrap(),
            None
        );
    }
}
