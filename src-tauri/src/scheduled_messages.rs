use std::collections::HashSet;
use std::fs::{File, OpenOptions, TryLockError};
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::time::Duration;

use chrono::Utc;
use tokio::task::JoinSet;
use tokio::time::{sleep, sleep_until, Instant};

use crate::acp::error::AcpError;
use crate::acp::manager::{ConnectionManager, PromptSessionIdentity};
use crate::acp::types::ConnectionStatus;
use crate::commands::acp::{build_session_runtime_env, verify_agent_installed};
use crate::db::entities::scheduled_message::Model;
use crate::db::error::DbError;
use crate::db::service::{
    conversation_selector_preference_service, conversation_service, folder_service,
    scheduled_message_service as store,
};
use crate::db::AppDatabase;
use crate::web::event_bridge::{emit_event, EventEmitter};

pub const CHANGED_EVENT: &str = "scheduled-message://changed";
const SCAN_INTERVAL: Duration = Duration::from_secs(1);
const MAX_RETRY_DELAY: Duration = Duration::from_secs(8);
const MAX_CONCURRENT_DELIVERIES: usize = 8;

struct Scheduler {
    db: AppDatabase,
    manager: ConnectionManager,
    emitter: EventEmitter,
    data_dir: PathBuf,
}

fn open_lock(data_dir: &Path, suffix: &str) -> io::Result<File> {
    OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(data_dir.join(format!(
            "{}.scheduled-{suffix}.lock",
            crate::db::database_file_name()
        )))
}

fn try_lock(file: &File) -> io::Result<bool> {
    match file.try_lock() {
        Ok(()) => Ok(true),
        Err(TryLockError::WouldBlock) => Ok(false),
        Err(TryLockError::Error(error)) => Err(error),
    }
}

async fn join_runtime(data_dir: &Path, process_started_at: i64) -> io::Result<(File, i64)> {
    // Serialize the exclusive presence probe and the switch to a shared lock.
    // All live runtimes share the same cutoff for missed messages, even when
    // the scheduler cannot acquire ownership or access the database yet.
    let startup = open_lock(data_dir, "startup")?;
    while !try_lock(&startup)? {
        sleep(Duration::from_millis(50)).await;
    }
    let mut presence = open_lock(data_dir, "runtime")?;
    if try_lock(&presence)? {
        presence.set_len(0)?;
        presence.write_all(&process_started_at.to_le_bytes())?;
        presence.unlock()?;
        presence.lock_shared()?;
        return Ok((presence, process_started_at));
    }
    presence.lock_shared()?;
    let started_at = read_runtime_start(&mut presence)?;
    Ok((presence, started_at))
}

fn read_runtime_start(presence: &mut File) -> io::Result<i64> {
    let mut started_at = [0; 8];
    presence.read_exact(&mut started_at)?;
    Ok(i64::from_le_bytes(started_at))
}

pub(crate) fn live_runtime_started_at(data_dir: &Path) -> io::Result<i64> {
    // The startup lock prevents a new runtime from overwriting the timestamp
    // between the presence check and the read. The scheduler owns membership.
    let startup = open_lock(data_dir, "startup")?;
    if try_lock(&startup)? {
        let mut presence = open_lock(data_dir, "runtime")?;
        if !try_lock(&presence)? {
            return read_runtime_start(&mut presence);
        }
    }
    Err(io::Error::new(
        io::ErrorKind::WouldBlock,
        "scheduled message runtime has not finished registering",
    ))
}

pub async fn run(
    db: AppDatabase,
    manager: ConnectionManager,
    emitter: EventEmitter,
    data_dir: PathBuf,
) {
    let engine = std::sync::Arc::new(Scheduler {
        db,
        manager,
        emitter,
        data_dir,
    });
    engine.run().await;
}

fn retry_delay(backoff: &mut Duration) -> Duration {
    let delay = *backoff;
    *backoff = (delay * 2).min(MAX_RETRY_DELAY);
    delay
}

impl Scheduler {
    async fn run(self: &std::sync::Arc<Self>) {
        let process_started_at = Utc::now().timestamp_millis();
        let mut backoff = SCAN_INTERVAL;
        // Keep this membership for the backend task's lifetime. The owner
        // lock, boot reconciliation and due scans can fail independently.
        let (_presence, started_at) = loop {
            match join_runtime(&self.data_dir, process_started_at).await {
                Ok(presence) => break presence,
                Err(err) => tracing::warn!(%err, "[scheduled-message] runtime registration failed"),
            }
            sleep(retry_delay(&mut backoff)).await;
        };

        let mut ownership = None;
        let mut reconciled = false;
        let mut revisions = Vec::new();
        let mut workers = JoinSet::new();
        let mut active = HashSet::new();
        let mut next_scan = Instant::now();
        backoff = SCAN_INTERVAL;
        loop {
            tokio::select! {
                result = workers.join_next(), if !workers.is_empty() => {
                    if let Some(Ok(conversation_id)) = result {
                        active.remove(&conversation_id);
                    }
                }
                _ = sleep_until(next_scan) => {
                    let result = async {
                        if ownership.is_none() {
                            let candidate = open_lock(&self.data_dir, "owner")?;
                            if try_lock(&candidate)? {
                                ownership = Some(candidate);
                            }
                        }
                        if ownership.is_some() {
                            if !reconciled {
                                self.reconcile(started_at).await?;
                                reconciled = true;
                            }
                            self.claim_due(&mut workers, &mut active, started_at).await?;
                        }
                        // Each process publishes shared DB changes to its own clients.
                        let next = store::revisions(&self.db.conn).await?;
                        if next != revisions {
                            revisions = next;
                            emit_event(&self.emitter, CHANGED_EVENT, ());
                        }
                        Ok::<_, DbError>(())
                    }.await;
                    let delay = match result {
                        Ok(()) => {
                            backoff = SCAN_INTERVAL;
                            SCAN_INTERVAL
                        }
                        Err(err) => {
                            tracing::warn!(%err, "[scheduled-message] scan failed");
                            retry_delay(&mut backoff)
                        }
                    };
                    next_scan = Instant::now() + delay;
                }
            }
        }
    }

    async fn reconcile(&self, started_at: i64) -> Result<(), DbError> {
        store::mark_missed(&self.db.conn, started_at).await?;
        store::fail_interrupted(&self.db.conn).await
    }

    async fn claim_due(
        self: &std::sync::Arc<Self>,
        workers: &mut JoinSet<i32>,
        active: &mut HashSet<i32>,
        started_at: i64,
    ) -> Result<(), DbError> {
        if active.len() >= MAX_CONCURRENT_DELIVERIES {
            return Ok(());
        }
        let now = Utc::now().timestamp_millis();
        for row in store::due(&self.db.conn, now).await? {
            if active.contains(&row.conversation_id)
                || !store::claim(&self.db.conn, &row, now).await?
            {
                continue;
            }
            active.insert(row.conversation_id);
            let engine = self.clone();
            workers.spawn(async move {
                let outcome = match engine.deliver(&row, started_at).await {
                    Ok(()) => store::Outcome::Sent,
                    Err(AcpError::TurnInProgress) => store::Outcome::Waiting {
                        retry_at: Utc::now().timestamp_millis() + SCAN_INTERVAL.as_millis() as i64,
                    },
                    Err(err) => store::Outcome::Failed(err.to_string()),
                };
                // A successful prompt may already be in flight. Retry only the
                // state write if SQLite is temporarily unavailable.
                while let Err(err) = store::settle(&engine.db.conn, &row.id, &outcome).await {
                    tracing::warn!(%err, id = row.id, "[scheduled-message] could not settle delivery");
                    sleep(SCAN_INTERVAL).await;
                }
                row.conversation_id
            });
            if active.len() >= MAX_CONCURRENT_DELIVERIES {
                break;
            }
        }
        Ok(())
    }

    async fn deliver(&self, message: &Model, started_at: i64) -> Result<(), AcpError> {
        let blocks = store::decode_prompt(message).map_err(protocol)?.blocks;
        let row = conversation_service::get_by_id(&self.db.conn, message.conversation_id)
            .await
            .map_err(protocol)?;
        let external_id = row
            .external_id
            .as_deref()
            .filter(|id| !id.trim().is_empty());
        if external_id.is_none() && !(message.first_message && row.message_count == 0) {
            return Err(AcpError::protocol(
                "scheduled conversation has no historical session ID",
            ));
        }
        let folder = folder_service::get_folder_by_id(&self.db.conn, row.folder_id)
            .await
            .map_err(protocol)?
            .ok_or_else(|| AcpError::protocol("scheduled conversation folder no longer exists"))?;
        let working_dir = PathBuf::from(&folder.path);
        let mut connection = self
            .manager
            .find_connection_by_conversation_id(row.id)
            .await;
        if let Some(id) = connection.as_ref() {
            match self.manager.get_state(id).await {
                Some(state)
                    if matches!(
                        state.read().await.status,
                        ConnectionStatus::Connecting
                            | ConnectionStatus::Connected
                            | ConnectionStatus::Prompting
                    ) => {}
                _ => connection = None,
            }
        }
        if connection.is_none() {
            connection = self
                .manager
                .find_connection_for_reuse(row.agent_type, Some(&working_dir), external_id)
                .await;
        }
        if connection.is_none() && external_id.is_none() {
            if let Some(initial) = message.initial_connection_id.as_ref() {
                if let Some(state) = self.manager.get_state(initial).await {
                    let state = state.read().await;
                    if state.agent_type == row.agent_type
                        && state.working_dir.as_ref() == Some(&working_dir)
                        && state.conversation_id.is_none()
                        && state.status == ConnectionStatus::Connected
                    {
                        connection = Some(initial.clone());
                    }
                }
            }
        }
        let reuse_initial = connection.is_some()
            && external_id.is_none()
            && connection == message.initial_connection_id;
        let mut spawned = false;
        let connection = match connection {
            Some(connection) => connection,
            None => {
                // A peer process can own a turn on this row. Wait for its
                // normal lifecycle to settle before trying a cold resume.
                if row.status == "in_progress" && row.updated_at.timestamp_millis() >= started_at {
                    return Err(AcpError::TurnInProgress);
                }
                verify_agent_installed(row.agent_type).await?;
                let env = build_session_runtime_env(
                    &self.db,
                    row.agent_type,
                    external_id,
                    &self.data_dir,
                )
                .await?;
                let preferences =
                    conversation_selector_preference_service::get(&self.db.conn, row.id)
                        .await
                        .map_err(protocol)?;
                let (mode, config) = preferences
                    .map(|prefs| (prefs.mode_id, prefs.config_values))
                    .unwrap_or_default();
                let result = self
                    .manager
                    .spawn_for_scheduled_message(
                        row.agent_type,
                        folder.path,
                        external_id.map(str::to_owned),
                        env,
                        self.emitter.clone(),
                        mode,
                        config,
                    )
                    .await?;
                spawned = !result.reused;
                result.connection_id
            }
        };

        let result = async {
            let state = self
                .manager
                .get_state(&connection)
                .await
                .ok_or(AcpError::ProcessExited)?;
            let (session_id, current_mode) = tokio::time::timeout(Duration::from_secs(60), async {
                loop {
                    {
                        let state = state.read().await;
                        match state.status {
                            ConnectionStatus::Prompting => return Err(AcpError::TurnInProgress),
                            ConnectionStatus::Error | ConnectionStatus::Disconnected => {
                                return Err(AcpError::ProcessExited)
                            }
                            ConnectionStatus::Connected => {
                                if let Some(id) = state.external_id.clone() {
                                    break Ok((id, state.current_mode.clone()));
                                }
                            }
                            ConnectionStatus::Connecting => {}
                        }
                    }
                    sleep(Duration::from_millis(50)).await;
                }
            })
            .await
            .map_err(|_| AcpError::InitializeTimeout)??;
            if external_id.is_some_and(|id| id != session_id) {
                return Err(AcpError::SessionIdentityMismatch);
            }
            if reuse_initial {
                // Config selectors already go to the live draft connection.
                // Its legacy mode, like an immediate first send, is applied
                // just before the prompt.
                let mode = conversation_selector_preference_service::get(&self.db.conn, row.id)
                    .await
                    .map_err(protocol)?
                    .and_then(|prefs| prefs.mode_id);
                if let Some(mode) = mode.filter(|mode| Some(mode) != current_mode.as_ref()) {
                    tokio::time::timeout(
                        Duration::from_secs(30),
                        self.manager.set_mode_and_wait(&connection, mode),
                    )
                    .await
                    .map_err(|_| AcpError::protocol("scheduled mode selection timed out"))??;
                }
            }
            self.manager
                .send_prompt_linked_for_session(
                    &self.db,
                    &connection,
                    blocks,
                    row.folder_id,
                    row.id,
                    PromptSessionIdentity {
                        agent_type: row.agent_type,
                        external_id: session_id,
                        working_dir,
                        conversation_id: row.id,
                        allow_unlinked: true,
                        expected_persisted_external_id: Some(row.external_id.clone()),
                    },
                )
                .await?;
            Ok(())
        }
        .await;
        if spawned && !matches!(result, Ok(()) | Err(AcpError::TurnInProgress)) {
            if let Some(lease) = self.manager.prompt_connection_lease(&connection).await {
                let _ = self
                    .manager
                    .disconnect_if_unclaimed(&connection, &lease)
                    .await;
            }
        }
        result
    }
}

fn protocol(error: DbError) -> AcpError {
    AcpError::protocol(error.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::acp::connection::ConnectionCommand;
    use crate::acp::types::PromptInputBlock;
    use crate::commands::scheduled_messages::{
        import_legacy_scheduled_message_core, schedule_message_core, LegacyMessage,
        ScheduleRequest, Target,
    };
    use crate::db::entities::scheduled_message::Status;
    use crate::db::test_helpers::{
        fresh_disk_db, fresh_in_memory_db, seed_conversation, seed_folder,
    };
    use crate::models::AgentType;

    fn draft() -> store::Prompt {
        store::Prompt {
            blocks: vec![PromptInputBlock::Text {
                text: "scheduled follow-up".into(),
            }],
            display_text: "scheduled follow-up".into(),
        }
    }

    fn legacy_message(id: &str, scope: &str, scheduled_at: i64, status: &str) -> LegacyMessage {
        serde_json::from_value(serde_json::json!({
            "id": id,
            "scopeId": scope,
            "draft": draft(),
            "scheduledAt": scheduled_at,
            "status": status,
        }))
        .unwrap()
    }

    #[tokio::test]
    async fn scheduled_messages_import_legacy_future_and_preserve_outage_and_terminal_statuses() {
        let (engine, folder_id, dir) = for_folder().await;
        let id = seed_conversation(&engine.db, folder_id, AgentType::OpenClaw).await;
        conversation_service::bind_external_id(&engine.db.conn, id, "historical", &[])
            .await
            .unwrap();
        let mut commands = fake_connection(&engine, "historical", dir.path()).await;
        let scope = format!("conversation:{id}");
        let now = Utc::now().timestamp_millis();
        let first = join_runtime(dir.path(), now - 300).await.unwrap();
        drop(first);
        let (_presence, started_at) = join_runtime(dir.path(), now - 100).await.unwrap();

        for (key, time, status) in [
            ("outage", now - 200, "waiting"),
            ("online", now - 50, "scheduled"),
            ("old-missed", now + 10_000, "missed"),
            ("old-failed", now + 20_000, "failed"),
            ("future", now + 30_000, "scheduled"),
        ] {
            assert!(import_legacy_scheduled_message_core(
                &engine.db,
                &engine.emitter,
                dir.path(),
                legacy_message(key, &scope, time, status),
            )
            .await
            .unwrap());
        }
        let list = store::list(&engine.db.conn, id).await.unwrap();
        let future = list.last().unwrap();
        assert_eq!(future.scheduled_at, now + 30_000);
        assert_eq!(future.conversation_id, id);
        assert_eq!(future.draft.blocks, draft().blocks);
        assert_eq!(future.draft.display_text, draft().display_text);
        assert_eq!(
            list.iter()
                .map(|message| message.status)
                .collect::<Vec<_>>(),
            [
                Status::Missed,
                Status::Scheduled,
                Status::Missed,
                Status::Failed,
                Status::Scheduled
            ]
        );

        engine.reconcile(started_at).await.unwrap();
        tick(&engine).await;
        assert!(
            matches!(commands.try_recv(), Ok(ConnectionCommand::Prompt { blocks, .. }) if blocks == draft().blocks)
        );
        assert_eq!(store::list(&engine.db.conn, id).await.unwrap().len(), 4);
        assert!(commands.try_recv().is_err());
        assert_original_row(&engine, id, "historical").await;

        // A lost confirmation can be retried after the scheduler deleted the job.
        assert!(import_legacy_scheduled_message_core(
            &engine.db,
            &engine.emitter,
            dir.path(),
            legacy_message("online", &scope, now - 50, "scheduled"),
        )
        .await
        .unwrap());
        assert!(store::due(&engine.db.conn, Utc::now().timestamp_millis())
            .await
            .unwrap()
            .is_empty());
    }

    #[tokio::test]
    async fn scheduled_messages_import_legacy_once_across_database_connections() {
        let dir = tempfile::tempdir().unwrap();
        let db = fresh_disk_db(dir.path()).await;
        let peer = AppDatabase {
            conn: sea_orm::Database::connect(format!(
                "sqlite:{}",
                dir.path().join("source.db").display()
            ))
            .await
            .unwrap(),
        };
        let folder_id = seed_folder(&db, dir.path().to_str().unwrap()).await;
        let id = seed_conversation(&db, folder_id, AgentType::OpenClaw).await;
        let scope = format!("conversation:{id}");
        let now = Utc::now().timestamp_millis();
        let _presence = join_runtime(dir.path(), now).await.unwrap();
        let import = |db| {
            import_legacy_scheduled_message_core(
                db,
                &EventEmitter::Noop,
                dir.path(),
                legacy_message("same-legacy-id", &scope, now + 1000, "scheduled"),
            )
        };
        let (first, second) = tokio::join!(import(&db), import(&peer));
        assert!(first.unwrap() && second.unwrap());
        let rows = store::list(&db.conn, id).await.unwrap();
        assert_eq!(rows.len(), 1);
        store::cancel(&db.conn, rows[0].id.clone()).await.unwrap();
        assert!(import(&peer).await.unwrap());
        assert!(store::list(&db.conn, id).await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn scheduled_messages_import_legacy_rejects_invalid_or_deleted_targets() {
        let (engine, folder_id, dir) = for_folder().await;
        let id = seed_conversation(&engine.db, folder_id, AgentType::OpenClaw).await;
        let deleted = seed_conversation(&engine.db, folder_id, AgentType::OpenClaw).await;
        conversation_service::soft_delete(&engine.db.conn, deleted)
            .await
            .unwrap();
        let now = Utc::now().timestamp_millis();
        let _presence = join_runtime(dir.path(), now).await.unwrap();
        let import = |scope: &str| {
            import_legacy_scheduled_message_core(
                &engine.db,
                &engine.emitter,
                dir.path(),
                legacy_message("retry-after-rejection", scope, now + 1000, "scheduled"),
            )
        };
        for scope in [
            "draft:12",
            "conversation:0",
            "conversation:-1",
            "conversation:1.0",
            "conversation:2147483648",
            "conversation:99999",
            &format!("conversation:{deleted}"),
        ] {
            assert!(!import(scope).await.unwrap());
        }
        assert_eq!(
            conversation_service::list_all(&engine.db.conn, None, None, None, None, None, true)
                .await
                .unwrap()
                .len(),
            1
        );
        assert!(store::list(&engine.db.conn, id).await.unwrap().is_empty());
        assert!(import(&format!("conversation:{id}")).await.unwrap());
        assert_eq!(store::list(&engine.db.conn, id).await.unwrap().len(), 1);
    }

    async fn fake_connection(
        engine: &Scheduler,
        external_id: &str,
        dir: &Path,
    ) -> tokio::sync::mpsc::Receiver<ConnectionCommand> {
        let commands = engine
            .manager
            .insert_test_connection_live(
                "connection",
                AgentType::OpenClaw,
                Some(dir.to_owned()),
                EventEmitter::Noop,
            )
            .await;
        let state = engine.manager.get_state("connection").await.unwrap();
        state.write().await.external_id = Some(external_id.into());
        commands
    }

    async fn tick(engine: &std::sync::Arc<Scheduler>) {
        let mut workers = JoinSet::new();
        engine
            .claim_due(&mut workers, &mut HashSet::new(), 0)
            .await
            .unwrap();
        while let Some(result) = workers.join_next().await {
            result.unwrap();
        }
    }

    async fn for_folder() -> (std::sync::Arc<Scheduler>, i32, tempfile::TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let db = fresh_in_memory_db().await;
        let folder = seed_folder(&db, dir.path().to_str().unwrap()).await;
        let engine = std::sync::Arc::new(Scheduler {
            db,
            manager: ConnectionManager::new(),
            emitter: EventEmitter::Noop,
            data_dir: dir.path().to_owned(),
        });
        (engine, folder, dir)
    }

    async fn assert_original_row(engine: &Scheduler, id: i32, external_id: &str) {
        let rows =
            conversation_service::list_all(&engine.db.conn, None, None, None, None, None, true)
                .await
                .unwrap();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].id, id);
        assert_eq!(rows[0].external_id.as_deref(), Some(external_id));
    }

    #[tokio::test]
    async fn scheduled_messages_deliver_using_original_session_and_row_without_ui() {
        let (engine, folder_id, dir) = for_folder().await;
        let id = seed_conversation(&engine.db, folder_id, AgentType::OpenClaw).await;
        conversation_service::bind_external_id(&engine.db.conn, id, "historical", &[])
            .await
            .unwrap();
        let mut commands = fake_connection(&engine, "historical", dir.path()).await;
        let _presence = join_runtime(dir.path(), Utc::now().timestamp_millis())
            .await
            .unwrap();
        let worker = tokio::spawn({
            let engine = engine.clone();
            async move { engine.run().await }
        });
        store::create(
            &engine.db.conn,
            id,
            draft(),
            Utc::now().timestamp_millis() + 300,
            false,
            None,
        )
        .await
        .unwrap();
        assert!(
            tokio::time::timeout(Duration::from_millis(50), commands.recv())
                .await
                .is_err()
        );
        let command = tokio::time::timeout(Duration::from_secs(3), commands.recv())
            .await
            .expect("the backend runtime delivered the prompt");
        assert!(
            matches!(command, Some(ConnectionCommand::Prompt { blocks, .. }) if blocks == draft().blocks)
        );
        assert_original_row(&engine, id, "historical").await;
        tokio::time::timeout(Duration::from_secs(2), async {
            while !store::list(&engine.db.conn, id).await.unwrap().is_empty() {
                sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("the accepted job was removed");
        assert!(commands.try_recv().is_err());
        worker.abort();
    }

    #[tokio::test]
    async fn scheduled_messages_recover_from_startup_reconciliation_failure() {
        use sea_orm::ConnectionTrait;

        let (engine, folder_id, dir) = for_folder().await;
        let id = seed_conversation(&engine.db, folder_id, AgentType::OpenClaw).await;
        conversation_service::bind_external_id(&engine.db.conn, id, "historical", &[])
            .await
            .unwrap();
        let mut commands = fake_connection(&engine, "historical", dir.path()).await;
        store::create(&engine.db.conn, id, draft(), 1, false, None)
            .await
            .unwrap();
        engine
            .db
            .conn
            .execute_unprepared(
                "CREATE TRIGGER reject_boot_reconcile BEFORE UPDATE ON scheduled_message
             WHEN NEW.status = 'missed'
             BEGIN SELECT RAISE(ABORT, 'startup database failure'); END",
            )
            .await
            .unwrap();

        let worker = tokio::spawn({
            let engine = engine.clone();
            async move { engine.run().await }
        });
        let owner = open_lock(dir.path(), "owner").unwrap();
        tokio::time::timeout(Duration::from_secs(3), async {
            while try_lock(&owner).unwrap() {
                owner.unlock().unwrap();
                sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("scheduler acquired ownership before the failing reconciliation");
        let runtime = open_lock(dir.path(), "runtime").unwrap();
        assert!(!try_lock(&runtime).unwrap());

        let scheduled_at = Utc::now().timestamp_millis() + 100;
        store::create(&engine.db.conn, id, draft(), scheduled_at, false, None)
            .await
            .unwrap();
        assert!(
            tokio::time::timeout(Duration::from_millis(200), commands.recv())
                .await
                .is_err()
        );
        assert!(!worker.is_finished());
        assert!(!try_lock(&runtime).unwrap());
        engine
            .db
            .conn
            .execute_unprepared("DROP TRIGGER reject_boot_reconcile")
            .await
            .unwrap();

        assert!(matches!(
            tokio::time::timeout(Duration::from_secs(5), commands.recv())
                .await
                .unwrap(),
            Some(ConnectionCommand::Prompt { .. })
        ));
        tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                let rows = store::list(&engine.db.conn, id).await.unwrap();
                assert_eq!(rows[0].status, Status::Missed);
                if rows.len() == 1 {
                    break;
                }
                sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("only the job that predated startup was missed");
        worker.abort();
    }

    #[tokio::test]
    async fn scheduled_messages_retry_a_busy_connection_once() {
        let (engine, folder_id, dir) = for_folder().await;
        let id = seed_conversation(&engine.db, folder_id, AgentType::OpenClaw).await;
        conversation_service::bind_external_id(&engine.db.conn, id, "historical", &[])
            .await
            .unwrap();
        let mut commands = fake_connection(&engine, "historical", dir.path()).await;
        let state = engine.manager.get_state("connection").await.unwrap();
        state.write().await.turn_in_flight = true;
        store::create(&engine.db.conn, id, draft(), 1, false, None)
            .await
            .unwrap();
        tick(&engine).await;
        assert_eq!(
            store::list(&engine.db.conn, id).await.unwrap()[0].status,
            Status::Waiting
        );
        assert!(commands.try_recv().is_err());
        state.write().await.turn_in_flight = false;
        tick(&engine).await;
        assert!(commands.try_recv().is_err());
        sleep(SCAN_INTERVAL).await;
        tick(&engine).await;
        assert!(matches!(
            commands.try_recv(),
            Ok(ConnectionCommand::Prompt { .. })
        ));
        assert!(store::list(&engine.db.conn, id).await.unwrap().is_empty());
    }

    #[tokio::test]
    async fn scheduled_messages_first_send_creates_and_uses_a_real_row() {
        let (engine, folder_id, dir) = for_folder().await;
        let mut commands = fake_connection(&engine, "first-session", dir.path()).await;
        let result = schedule_message_core(
            &engine.db,
            &EventEmitter::Noop,
            dir.path(),
            ScheduleRequest {
                target: Target::Project {
                    folder_id,
                    agent_type: AgentType::OpenClaw,
                    title: "Scheduled".into(),
                },
                draft: draft(),
                scheduled_at: Utc::now().timestamp_millis() + 300,
                initial_connection_id: Some("connection".into()),
                selector_preferences: None,
            },
        )
        .await
        .unwrap();
        let id = result.conversation_id;
        assert_eq!(
            store::list(&engine.db.conn, id).await.unwrap()[0].status,
            Status::Scheduled
        );
        assert!(conversation_service::get_by_id(&engine.db.conn, id)
            .await
            .unwrap()
            .external_id
            .is_none());
        tick(&engine).await;
        assert!(commands.try_recv().is_err());
        sleep(Duration::from_millis(301)).await;
        tick(&engine).await;
        assert!(matches!(
            commands.try_recv(),
            Ok(ConnectionCommand::Prompt { .. })
        ));
        assert_original_row(&engine, id, "first-session").await;
    }

    async fn queue_due_first_message(engine: &Scheduler, folder_id: i32) -> i32 {
        use crate::db::entities::conversation::ConversationStatus;

        let id = seed_conversation(&engine.db, folder_id, AgentType::OpenClaw).await;
        conversation_service::update_status(&engine.db.conn, id, ConversationStatus::Completed)
            .await
            .unwrap();
        store::create(
            &engine.db.conn,
            id,
            draft(),
            1,
            true,
            Some("connection".into()),
        )
        .await
        .unwrap();
        id
    }

    async fn assert_scheduled_first_identity_mismatch(
        engine: &Scheduler,
        id: i32,
        commands: &mut tokio::sync::mpsc::Receiver<ConnectionCommand>,
    ) {
        assert!(matches!(
            commands.try_recv(),
            Err(tokio::sync::mpsc::error::TryRecvError::Empty)
        ));
        let messages = store::list(&engine.db.conn, id).await.unwrap();
        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0].status, Status::Failed);
        assert_eq!(
            messages[0].error,
            Some(AcpError::SessionIdentityMismatch.to_string())
        );
        assert_original_row(engine, id, "other-session").await;
        let row = conversation_service::get_by_id(&engine.db.conn, id)
            .await
            .unwrap();
        assert_eq!(row.status, "completed");
        let state = engine.manager.get_state("connection").await.unwrap();
        let state = state.read().await;
        assert!(state.conversation_id.is_none());
        assert!(!state.turn_in_flight);
    }

    #[tokio::test]
    async fn scheduled_messages_first_send_rejects_a_bind_after_resolution() {
        use sea_orm::ConnectionTrait;

        use crate::acp::connection::SetterOutcome;

        let (engine, folder_id, dir) = for_folder().await;
        let id = queue_due_first_message(&engine, folder_id).await;
        let mut commands = fake_connection(&engine, "first-session", dir.path()).await;
        conversation_selector_preference_service::upsert(
            &engine.db.conn,
            id,
            Some("plan".into()),
            &Default::default(),
        )
        .await
        .unwrap();

        tokio::time::timeout(Duration::from_secs(5), async {
            tokio::join!(tick(&engine), async {
                // The scheduler has already read the NULL binding. Hold its
                // mode acknowledgement until another flow has bound the row.
                let Some(ConnectionCommand::SetMode {
                    reply: Some(reply), ..
                }) = commands.recv().await
                else {
                    panic!("expected the scheduled mode selection");
                };
                conversation_service::bind_external_id(&engine.db.conn, id, "other-session", &[])
                    .await
                    .unwrap();
                engine
                    .db
                    .conn
                    .execute_unprepared(
                        "CREATE TRIGGER reject_unexpected_bind
                         BEFORE UPDATE ON conversation
                         BEGIN
                             SELECT RAISE(ABORT, 'the pre-admission check must reject first');
                         END",
                    )
                    .await
                    .unwrap();
                reply
                    .send(Ok(SetterOutcome::Confirmed {
                        current: "plan".into(),
                    }))
                    .unwrap();
            });
        })
        .await
        .expect("the scheduled prompt reached admission");
        assert_scheduled_first_identity_mismatch(&engine, id, &mut commands).await;
    }

    #[tokio::test]
    async fn scheduled_messages_first_send_rejects_a_bind_after_admission_check() {
        use sea_orm::ConnectionTrait;

        let (engine, folder_id, dir) = for_folder().await;
        let id = queue_due_first_message(&engine, folder_id).await;
        let mut commands = fake_connection(&engine, "first-session", dir.path()).await;
        // Inject the competing bind at the writer-lock claim, after the
        // manager's row check and before the binding transaction reads it.
        engine
            .db
            .conn
            .execute_unprepared(
                "CREATE TRIGGER bind_before_scheduled_compare
             BEFORE UPDATE OF updated_at ON conversation
             WHEN OLD.external_id IS NULL AND NEW.updated_at = OLD.updated_at
             BEGIN
                 UPDATE conversation SET external_id = 'other-session' WHERE id = OLD.id;
             END",
            )
            .await
            .unwrap();
        tick(&engine).await;
        assert_scheduled_first_identity_mismatch(&engine, id, &mut commands).await;
    }

    #[cfg(unix)]
    #[allow(clippy::await_holding_lock)]
    #[tokio::test]
    async fn scheduled_messages_replay_first_message_selectors_after_the_draft_closes() {
        use std::collections::BTreeMap;

        use crate::acp::custom_registry::{self, CustomAgentDef};
        use crate::db::service::conversation_selector_preference_service::ConversationSelectorPreferences;

        let (engine, folder_id, dir) = for_folder().await;
        let script = dir.path().join("scheduled-agent");
        crate::process::write_test_script(
            &script,
            r#"#!/bin/sh
set -eu
while IFS= read -r request; do
    id=$(printf '%s' "$request" | sed -nE 's/.*"id":[[:space:]]*("[^"]*"|[0-9]+).*/\1/p')
    [ -n "$id" ] || continue
    printf '%s\n' "$request" >> wire.jsonl
    case "$request" in
        *'"initialize"'*)
            result='{"protocolVersion":1,"agentCapabilities":{}}';;
        *'"session/new"'*)
            result='{"sessionId":"cold-scheduled","modes":{"currentModeId":"act","availableModes":[{"id":"act","name":"Act"},{"id":"plan","name":"Plan"}]},"configOptions":[{"type":"select","id":"model","name":"Model","category":"model","currentValue":"fast","options":[{"value":"fast","name":"Fast"},{"value":"careful","name":"Careful"}]}]}';;
        *'"session/set_mode"'*) result='{}';;
        *'"session/set_config_option"'*)
            result='{"configOptions":[{"type":"select","id":"model","name":"Model","category":"model","currentValue":"careful","options":[{"value":"fast","name":"Fast"},{"value":"careful","name":"Careful"}]}]}';;
        *'"session/prompt"'*) result='{"stopReason":"end_turn"}';;
        *) printf '{"jsonrpc":"2.0","id":%s,"error":{"code":-32601,"message":"Method not found"}}\n' "$id"; continue;;
    esac
    printf '{"jsonrpc":"2.0","id":%s,"result":%s}\n' "$id" "$result"
done
"#,
        );
        let agent: CustomAgentDef = serde_json::from_value(serde_json::json!({
            "registry_id": "scheduled-test-agent",
            "name": "Scheduled test agent",
            "version": "1.0.0",
            "distribution_kind": "npx",
            "spec": { "npx": { "package": "scheduled-test-agent@1.0.0", "cmd": script } },
            "supports_mcp": false
        }))
        .unwrap();
        let _registry_guard = custom_registry::hydrate_test_guard();
        assert!(custom_registry::hydrate(&[agent]).is_empty());
        let agent_type = AgentType::custom("scheduled-test-agent").unwrap();

        let _draft_commands = engine
            .manager
            .insert_test_connection_live(
                "draft",
                agent_type,
                Some(dir.path().to_owned()),
                EventEmitter::Noop,
            )
            .await;
        let saved = ConversationSelectorPreferences {
            mode_id: Some("plan".into()),
            config_values: BTreeMap::from([("model".into(), "careful".into())]),
        };
        let result = schedule_message_core(
            &engine.db,
            &EventEmitter::Noop,
            dir.path(),
            ScheduleRequest {
                target: Target::Project {
                    folder_id,
                    agent_type,
                    title: "First prompt".into(),
                },
                draft: draft(),
                scheduled_at: Utc::now().timestamp_millis() + 150,
                initial_connection_id: Some("draft".into()),
                selector_preferences: Some(saved.clone()),
            },
        )
        .await
        .unwrap();
        engine.manager.disconnect("draft").await.unwrap();
        assert!(engine.manager.get_state("draft").await.is_none());
        assert_eq!(
            conversation_selector_preference_service::get(&engine.db.conn, result.conversation_id)
                .await
                .unwrap(),
            Some(saved)
        );
        sleep(Duration::from_millis(151)).await;
        tokio::time::timeout(Duration::from_secs(10), tick(&engine))
            .await
            .unwrap();
        let wire_path = dir.path().join("wire.jsonl");
        let wire = tokio::time::timeout(Duration::from_secs(3), async {
            loop {
                let wire = std::fs::read_to_string(&wire_path).unwrap_or_default();
                if wire.contains("session/prompt") {
                    break wire;
                }
                sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("the cold connection received the prompt");
        let requests: Vec<serde_json::Value> = wire
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect();
        let find = |method| {
            requests
                .iter()
                .position(|request| request["method"] == method)
                .unwrap()
        };
        let mode = find("session/set_mode");
        let config = find("session/set_config_option");
        let prompt = find("session/prompt");
        assert!(mode < prompt && config < prompt);
        assert_eq!(requests[mode]["params"]["modeId"], "plan");
        assert_eq!(requests[config]["params"]["configId"], "model");
        assert_eq!(requests[config]["params"]["value"], "careful");
        assert_eq!(requests[prompt]["params"]["sessionId"], "cold-scheduled");
        assert_original_row(&engine, result.conversation_id, "cold-scheduled").await;
        let live = engine
            .manager
            .find_connection_by_conversation_id(result.conversation_id)
            .await
            .unwrap();
        engine.manager.disconnect(&live).await.unwrap();
        custom_registry::hydrate(&[]);
    }

    #[tokio::test]
    async fn scheduled_messages_refuse_missing_or_replacement_session_ids() {
        let (engine, folder_id, dir) = for_folder().await;
        let id = seed_conversation(&engine.db, folder_id, AgentType::OpenClaw).await;
        let mut commands = fake_connection(&engine, "replacement", dir.path()).await;
        engine
            .manager
            .get_state("connection")
            .await
            .unwrap()
            .write()
            .await
            .conversation_id = Some(id);
        store::create(&engine.db.conn, id, draft(), 1, false, None)
            .await
            .unwrap();
        tick(&engine).await;
        assert_eq!(
            store::list(&engine.db.conn, id).await.unwrap()[0].status,
            Status::Failed
        );
        assert!(commands.try_recv().is_err());

        conversation_service::bind_external_id(&engine.db.conn, id, "historical", &[])
            .await
            .unwrap();
        store::create(&engine.db.conn, id, draft(), 2, false, None)
            .await
            .unwrap();
        tick(&engine).await;
        assert_eq!(
            store::list(&engine.db.conn, id).await.unwrap()[1].status,
            Status::Failed
        );
        assert!(commands.try_recv().is_err());
        assert_original_row(&engine, id, "historical").await;
    }

    #[tokio::test]
    async fn scheduled_messages_mark_missed_only_after_all_runtimes_exit() {
        let (engine, folder_id, dir) = for_folder().await;
        let id = seed_conversation(&engine.db, folder_id, AgentType::OpenClaw).await;
        let first = join_runtime(dir.path(), Utc::now().timestamp_millis())
            .await
            .unwrap();
        store::create(&engine.db.conn, id, draft(), 1, false, None)
            .await
            .unwrap();
        let second = join_runtime(dir.path(), Utc::now().timestamp_millis())
            .await
            .unwrap();
        assert_eq!(
            store::list(&engine.db.conn, id).await.unwrap()[0].status,
            Status::Scheduled
        );
        drop(first);
        drop(second);
        let (_restart, started_at) = join_runtime(dir.path(), Utc::now().timestamp_millis())
            .await
            .unwrap();
        engine.reconcile(started_at).await.unwrap();
        assert_eq!(
            store::list(&engine.db.conn, id).await.unwrap()[0].status,
            Status::Missed
        );
        assert!(store::due(&engine.db.conn, i64::MAX)
            .await
            .unwrap()
            .is_empty());
    }

    #[tokio::test]
    async fn scheduled_messages_claim_once_across_independent_database_connections() {
        let dir = tempfile::tempdir().unwrap();
        let db = fresh_disk_db(dir.path()).await;
        let peer = sea_orm::Database::connect(format!(
            "sqlite:{}",
            dir.path().join("source.db").display()
        ))
        .await
        .unwrap();
        let folder_id = seed_folder(&db, dir.path().to_str().unwrap()).await;
        let id = seed_conversation(&db, folder_id, AgentType::OpenClaw).await;
        store::create(&db.conn, id, draft(), 123, false, None)
            .await
            .unwrap();
        let row = store::due(&db.conn, 123).await.unwrap().remove(0);
        let (first, second) = tokio::join!(
            store::claim(&db.conn, &row, 123),
            store::claim(&peer, &row, 123)
        );
        assert_ne!(first.unwrap(), second.unwrap());
        assert!(store::cancel(&peer, row.id.clone()).await.is_err());
        store::fail_interrupted(&peer).await.unwrap();
        assert_eq!(
            store::list(&db.conn, id).await.unwrap()[0].status,
            Status::Failed
        );
        assert!(store::due(&db.conn, 123).await.unwrap().is_empty());
    }
}
