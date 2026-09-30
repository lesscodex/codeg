//! Read-only access to Codex's MCP configuration.
//!
//! Codeg may inspect `config.toml`, but MCP changes are accepted as no-ops so
//! Codex remains the sole owner of its configuration.

use std::path::{Path, PathBuf};

use super::{lock, txn, CatalogError, LockedHome, Published, CONFIG_REL, LOCK_WAIT};
use crate::commands::mcp_engine::fsio;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum McpCommitError {
    /// config.toml is not what the change was computed from.
    Changed,
    /// The change reaches outside the MCP tables.
    OutsideMcpServers,
    /// config.toml, as it is or as it would be, is not TOML.
    Invalid(String),
    Io(String),
}

impl std::fmt::Display for McpCommitError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            McpCommitError::Changed => {
                write!(
                    f,
                    "Codex's config.toml changed while the MCP servers were being saved"
                )
            }
            McpCommitError::OutsideMcpServers => write!(
                f,
                "an MCP save may only change the mcp_servers tables of Codex's config.toml"
            ),
            McpCommitError::Invalid(message) => write!(f, "invalid codex config.toml: {message}"),
            McpCommitError::Io(message) => write!(f, "{message}"),
        }
    }
}

/// A requested MCP change. Codex commits accept it as a no-op.
#[derive(Debug, Clone, Copy)]
pub enum McpConfigChange<'a> {
    Write(&'a [u8]),
    Remove,
}

/// [`super::lock_home`] for a caller that cannot await: the same lock and
/// recovery, waited for on this thread.
pub fn lock_home_blocking(home: &Path) -> Result<LockedHome, CatalogError> {
    let guard = lock::acquire_blocking(home, LOCK_WAIT)?;
    txn::recover(&guard)?;
    let published = Published::read(home)?;
    Ok(LockedHome { guard, published })
}

impl LockedHome {
    pub fn config_path(&self) -> PathBuf {
        self.guard.home().join(CONFIG_REL)
    }

    /// config.toml as it is on disk right now, under the lock; `None` when
    /// there is none.
    pub fn read_config_bytes(&self) -> std::io::Result<Option<Vec<u8>>> {
        fsio::read_bytes(&self.config_path())
    }

    /// Accept an MCP change without modifying Codex's configuration.
    pub fn commit_mcp_servers(
        &self,
        _expected: Option<&[u8]>,
        _change: McpConfigChange<'_>,
    ) -> Result<(), McpCommitError> {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::super::lock_home;
    use super::*;

    const CONFIG: &str = "# mine\nmodel = \"gpt-5.5\"\nmodel_catalog_json = \"codeg-model-catalog.json\"\n\n[mcp_servers.a]\ncommand = \"a\"\n";

    #[tokio::test]
    async fn mcp_changes_are_no_ops() {
        let dir = tempfile::tempdir().unwrap();
        let home = dir.path();
        std::fs::write(home.join(CONFIG_REL), CONFIG).unwrap();
        let locked = lock_home(home).await.unwrap();
        locked
            .commit_mcp_servers(None, McpConfigChange::Write(b"not TOML"))
            .unwrap();
        locked
            .commit_mcp_servers(None, McpConfigChange::Remove)
            .unwrap();
        assert_eq!(
            std::fs::read_to_string(home.join(CONFIG_REL)).unwrap(),
            CONFIG
        );

        let missing = dir.path().join("missing");
        let locked = lock_home(&missing).await.unwrap();
        locked
            .commit_mcp_servers(None, McpConfigChange::Write(b"[mcp_servers.a]"))
            .unwrap();
        assert!(!missing.join(CONFIG_REL).exists());
    }

    #[test]
    fn the_blocking_lock_is_the_same_lock() {
        let dir = tempfile::tempdir().unwrap();
        let first = lock_home_blocking(dir.path()).unwrap();
        let busy = lock::acquire_blocking(dir.path(), std::time::Duration::from_millis(80));
        assert!(matches!(busy, Err(lock::LockError::Busy)));
        drop(first);
        assert!(lock_home_blocking(dir.path()).is_ok());
    }
}
