use sea_orm_migration::prelude::*;

#[derive(DeriveMigrationName)]
pub struct Migration;

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        manager
            .create_table(
                Table::create()
                    .table(ConversationSelectorPreference::Table)
                    .if_not_exists()
                    .col(
                        ColumnDef::new(ConversationSelectorPreference::ConversationId)
                            .integer()
                            .not_null()
                            .primary_key(),
                    )
                    .col(
                        ColumnDef::new(ConversationSelectorPreference::PreferredModeId)
                            .text()
                            .null(),
                    )
                    .col(
                        ColumnDef::new(ConversationSelectorPreference::PreferredConfigValues)
                            .text()
                            .not_null(),
                    )
                    .col(
                        ColumnDef::new(ConversationSelectorPreference::CreatedAt)
                            .timestamp_with_time_zone()
                            .not_null(),
                    )
                    .col(
                        ColumnDef::new(ConversationSelectorPreference::UpdatedAt)
                            .timestamp_with_time_zone()
                            .not_null(),
                    )
                    .foreign_key(
                        ForeignKey::create()
                            .name("fk_conversation_selector_preference_conversation_id")
                            .from(
                                ConversationSelectorPreference::Table,
                                ConversationSelectorPreference::ConversationId,
                            )
                            .to(Conversation::Table, Conversation::Id)
                            .on_delete(ForeignKeyAction::Cascade),
                    )
                    .to_owned(),
            )
            .await
    }

    async fn down(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        manager
            .drop_table(
                Table::drop()
                    .table(ConversationSelectorPreference::Table)
                    .to_owned(),
            )
            .await
    }
}

#[derive(DeriveIden)]
enum ConversationSelectorPreference {
    Table,
    ConversationId,
    PreferredModeId,
    PreferredConfigValues,
    CreatedAt,
    UpdatedAt,
}

#[derive(DeriveIden)]
enum Conversation {
    Table,
    Id,
}
