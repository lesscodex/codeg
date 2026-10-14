use sea_orm::entity::prelude::*;
use serde::Serialize;

#[derive(Clone, Copy, Debug, PartialEq, Eq, EnumIter, DeriveActiveEnum, Serialize)]
#[sea_orm(rs_type = "String", db_type = "String(StringLen::None)")]
#[serde(rename_all = "snake_case")]
pub enum Status {
    #[sea_orm(string_value = "scheduled")]
    Scheduled,
    #[sea_orm(string_value = "waiting")]
    Waiting,
    #[sea_orm(string_value = "dispatching")]
    Dispatching,
    #[sea_orm(string_value = "missed")]
    Missed,
    #[sea_orm(string_value = "failed")]
    Failed,
}

#[derive(Clone, Debug, PartialEq, DeriveEntityModel)]
#[sea_orm(table_name = "scheduled_message")]
pub struct Model {
    #[sea_orm(primary_key, auto_increment = false)]
    pub id: String,
    pub conversation_id: i32,
    pub prompt: String,
    pub scheduled_at: i64,
    pub retry_at: Option<i64>,
    pub status: Status,
    pub first_message: bool,
    pub initial_connection_id: Option<String>,
    pub error: Option<String>,
}

#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {}

impl ActiveModelBehavior for ActiveModel {}
