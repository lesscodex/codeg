use sea_orm_migration::prelude::*;

#[derive(DeriveMigrationName)]
pub struct Migration;

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        manager
            .create_table(
                Table::create()
                    .table(Message::Table)
                    .col(
                        ColumnDef::new(Message::Id)
                            .string()
                            .not_null()
                            .primary_key(),
                    )
                    .col(ColumnDef::new(Message::ConversationId).integer().not_null())
                    .col(ColumnDef::new(Message::Prompt).text().not_null())
                    .col(
                        ColumnDef::new(Message::ScheduledAt)
                            .big_integer()
                            .not_null(),
                    )
                    .col(ColumnDef::new(Message::RetryAt).big_integer())
                    .col(ColumnDef::new(Message::Status).string().not_null())
                    .col(ColumnDef::new(Message::First).boolean().not_null())
                    .col(ColumnDef::new(Message::InitialConnectionId).string())
                    .col(ColumnDef::new(Message::Error).text())
                    .foreign_key(
                        ForeignKey::create()
                            .from(Message::Table, Message::ConversationId)
                            .to(Alias::new("conversation"), Alias::new("id"))
                            .on_delete(ForeignKeyAction::Cascade),
                    )
                    .to_owned(),
            )
            .await?;
        manager
            .create_index(
                Index::create()
                    .name("idx_scheduled_message_due")
                    .table(Message::Table)
                    .col(Message::Status)
                    .col(Message::ScheduledAt)
                    .to_owned(),
            )
            .await?;
        manager
            .create_index(
                Index::create()
                    .name("idx_scheduled_message_conversation")
                    .table(Message::Table)
                    .col(Message::ConversationId)
                    .to_owned(),
            )
            .await
    }

    async fn down(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        manager
            .drop_table(Table::drop().table(Message::Table).to_owned())
            .await
    }
}

#[derive(DeriveIden)]
enum Message {
    #[sea_orm(iden = "scheduled_message")]
    Table,
    Id,
    ConversationId,
    Prompt,
    ScheduledAt,
    RetryAt,
    Status,
    #[sea_orm(iden = "first_message")]
    First,
    InitialConnectionId,
    Error,
}
