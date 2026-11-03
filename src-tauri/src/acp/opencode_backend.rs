//! Host boundary for the maintained OpenCode ACP adapter.
//!
//! The stock OpenCode binary remains the fallback launch path.  A maintained
//! adapter is selected only from a staged bundle carrying a source identity
//! manifest; PATH binaries and the stock 2.x release never inherit this
//! adapter's steering policy.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
#[cfg(feature = "tauri-runtime")]
use std::sync::OnceLock;

use serde::Deserialize;

use crate::acp::agent_process::AcpAgent;
use crate::acp::connection::{agent_debug_callback, merge_agent_env};
use crate::acp::error::AcpError;
use crate::acp::registry::{AcpAgentMeta, AgentDistribution};
use crate::acp::stderr_tail::StderrTail;
use crate::models::agent::AgentType;

const OPENCODE_ACP_DIR: &str = "opencode-acp";
#[cfg(windows)]
const OPENCODE_ACP_ENTRY: &str = "bin/opencode.exe";
#[cfg(not(windows))]
const OPENCODE_ACP_ENTRY: &str = "bin/opencode";
const RUNTIME_PREREQUISITES: &str = "runtime-prerequisites.json";
#[cfg(not(feature = "tauri-runtime"))]
const DOCKER_OPENCODE_ACP_DIR: &str = "/opt/codeg/opencode-acp";

/// The maintained source identity is part of the launch gate. A staged
/// manifest must prove both values before OpenCode can opt into native
/// steering; stock and unknown PATH adapters remain on the fallback channel.
pub(crate) const OPENCODE_SOURCE_GITLINK: Option<&str> =
    Some("4c3fedb08a4b47099570d06774c795188282fa84");
pub(crate) const OPENCODE_SOURCE_DIGEST: Option<&str> =
    Some("bdeb3b47399d0a0b0d311b9e732392dfa4c2459215f3e89ecfbc8dc3f32cd29d");

#[cfg(feature = "tauri-runtime")]
static DESKTOP_RESOURCE_DIR: OnceLock<PathBuf> = OnceLock::new();

#[cfg(feature = "tauri-runtime")]
pub(crate) fn set_desktop_resource_dir(resource_dir: PathBuf) {
    let _ = DESKTOP_RESOURCE_DIR.set(resource_dir);
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct MaintainedAdapterIdentity {
    pub(crate) name: String,
    pub(crate) version: String,
}

#[derive(Debug, Deserialize)]
struct RuntimePrerequisites {
    #[serde(rename = "schemaVersion")]
    schema_version: u64,
    adapter: AdapterPrerequisites,
}

#[derive(Debug, Deserialize)]
struct AdapterPrerequisites {
    name: String,
    version: String,
    gitlink: String,
    entry: String,
    #[serde(rename = "sourceDigest")]
    source_digest: String,
}

fn packaged_adapter_roots() -> Vec<PathBuf> {
    #[cfg(feature = "tauri-runtime")]
    {
        DESKTOP_RESOURCE_DIR
            .get()
            .map(|dir| dir.join(OPENCODE_ACP_DIR))
            .into_iter()
            .collect()
    }

    #[cfg(not(feature = "tauri-runtime"))]
    {
        let mut roots = Vec::new();
        if let Ok(executable) = std::env::current_exe() {
            if let Some(parent) = executable.parent() {
                roots.push(parent.join(OPENCODE_ACP_DIR));
            }
        }
        roots.push(PathBuf::from(DOCKER_OPENCODE_ACP_DIR));
        roots
    }
}

pub(crate) fn packaged_adapter_entry_path(root: &Path) -> PathBuf {
    root.join(OPENCODE_ACP_ENTRY)
}

fn valid_hex(value: &str, length: usize) -> bool {
    value.len() == length && value.bytes().all(|byte| byte.is_ascii_hexdigit())
}

fn read_identity_with_policy(
    root: &Path,
    expected_gitlink: Option<&str>,
    expected_source_digest: Option<&str>,
) -> Option<MaintainedAdapterIdentity> {
    let manifest = std::fs::read_to_string(root.join(RUNTIME_PREREQUISITES)).ok()?;
    let manifest: RuntimePrerequisites = serde_json::from_str(&manifest).ok()?;
    let adapter = manifest.adapter;
    if manifest.schema_version != 1
        || expected_gitlink.is_none()
        || expected_source_digest.is_none()
        || adapter.name.trim().is_empty()
        || adapter.version.trim().is_empty()
        || adapter.entry != OPENCODE_ACP_ENTRY
        || !valid_hex(&adapter.gitlink, 40)
        || !valid_hex(&adapter.source_digest, 64)
        || Some(adapter.gitlink.as_str()) != expected_gitlink
        || Some(adapter.source_digest.as_str()) != expected_source_digest
        || !packaged_adapter_entry_path(root).is_file()
    {
        return None;
    }
    Some(MaintainedAdapterIdentity {
        name: adapter.name,
        version: adapter.version,
    })
}

fn read_identity(root: &Path) -> Option<MaintainedAdapterIdentity> {
    read_identity_with_policy(root, OPENCODE_SOURCE_GITLINK, OPENCODE_SOURCE_DIGEST)
}

/// Resolve a complete, source-identified maintained adapter bundle.
pub(crate) fn packaged_adapter_identity() -> Option<MaintainedAdapterIdentity> {
    packaged_adapter_roots()
        .into_iter()
        .find_map(|root| read_identity(&root))
}

pub(crate) fn packaged_adapter_entry() -> Option<PathBuf> {
    packaged_adapter_roots()
        .into_iter()
        .find_map(|root| read_identity(&root).map(|_| packaged_adapter_entry_path(&root)))
}

fn packaged_adapter_process_args(entry: &Path, args: &[&str]) -> Vec<String> {
    let mut parts = vec![crate::process::normalized_program(entry)
        .to_string_lossy()
        .into_owned()];
    parts.extend(args.iter().map(|arg| (*arg).to_string()));
    parts
}

pub(crate) async fn maintained_adapter_available() -> bool {
    packaged_adapter_identity().is_some() && packaged_adapter_entry().is_some()
}

/// Build the maintained adapter when its bundle is present. `Ok(None)` is the
/// deliberate fallback signal for stock OpenCode; errors after selection are
/// returned rather than silently starting a second process with another source.
pub(crate) async fn build_agent(
    meta: AcpAgentMeta,
    runtime_env: &BTreeMap<String, String>,
    stderr_tail: &Arc<StderrTail>,
    scratch: Option<&Path>,
) -> Result<Option<(AcpAgent, MaintainedAdapterIdentity)>, AcpError> {
    if meta.agent_type != AgentType::OpenCode {
        return Err(AcpError::SpawnFailed(
            "OpenCode ACP backend received metadata for a different agent".to_string(),
        ));
    }

    let agent_name = meta.name.to_string();
    let AgentDistribution::Binary { args, env, .. } = meta.distribution else {
        return Err(AcpError::SpawnFailed(
            "OpenCode ACP registry entry must use the binary distribution".to_string(),
        ));
    };

    let identity = packaged_adapter_identity();
    if !maintained_adapter_available().await {
        return Ok(None);
    }
    let identity = identity.expect("bundle availability checked above");
    let entry = packaged_adapter_entry().ok_or_else(|| {
        AcpError::SdkNotInstalled(
            "The maintained OpenCode ACP adapter bundle is incomplete".to_string(),
        )
    })?;

    let merged_env = merge_agent_env(env, runtime_env, scratch);
    let mut parts: Vec<String> = merged_env
        .iter()
        .map(|(key, value)| format!("{key}={value}"))
        .collect();
    parts.extend(packaged_adapter_process_args(&entry, args));

    let refs: Vec<&str> = parts.iter().map(String::as_str).collect();
    let tail = Arc::clone(stderr_tail);
    let agent = AcpAgent::from_args(&refs)
        .map(|agent| agent.with_debug(agent_debug_callback(agent_name, tail, false)))
        .map_err(|err| AcpError::SpawnFailed(err.to_string()))?;
    Ok(Some((agent, identity)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn manifest_identity_requires_entry_and_source_proof() {
        let dir = tempfile::tempdir().expect("temporary OpenCode bundle");
        let root = dir.path();
        std::fs::create_dir_all(root.join("bin")).expect("bundle entry directory");
        std::fs::write(root.join(OPENCODE_ACP_ENTRY), "entry").expect("bundle entry");
        let manifest = serde_json::json!({
            "schemaVersion": 1,
            "adapter": {
                "name": "OpenCode",
                "version": "2.2.0",
                "gitlink": "0123456789abcdef0123456789abcdef01234567",
                "sourceDigest": "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef",
                "entry": OPENCODE_ACP_ENTRY,
            }
        });
        std::fs::write(
            root.join(RUNTIME_PREREQUISITES),
            serde_json::to_vec(&manifest).expect("manifest JSON"),
        )
        .expect("bundle manifest");

        assert_eq!(
            read_identity_with_policy(
                root,
                Some("0123456789abcdef0123456789abcdef01234567"),
                Some("0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef"),
            ),
            Some(MaintainedAdapterIdentity {
                name: "OpenCode".to_string(),
                version: "2.2.0".to_string(),
            })
        );
        assert!(
            read_identity(root).is_none(),
            "the configured source policy must fail closed"
        );
        assert!(read_identity_with_policy(
            root,
            Some("fedcba9876543210fedcba9876543210fedcba98"),
            Some("0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef"),
        )
        .is_none());
        std::fs::write(
            root.join(RUNTIME_PREREQUISITES),
            serde_json::json!({"schemaVersion": 1, "adapter": {"name": "stock", "version": "2.2.0"}}).to_string(),
        )
        .expect("invalid manifest");
        assert!(read_identity(root).is_none());
    }

    #[test]
    fn packaged_launch_uses_bundled_executable() {
        let parts =
            packaged_adapter_process_args(Path::new("/bundle/opencode-acp/bin/opencode"), &["acp"]);
        assert_eq!(
            parts,
            vec![
                "/bundle/opencode-acp/bin/opencode".to_string(),
                "acp".to_string(),
            ]
        );
    }

    #[tokio::test]
    async fn backend_rejects_non_opencode_metadata() {
        let meta = crate::acp::registry::get_agent_meta(AgentType::Gemini);
        let result = build_agent(meta, &BTreeMap::new(), &Arc::new(StderrTail::new()), None).await;
        assert!(matches!(
            result,
            Err(AcpError::SpawnFailed(message)) if message.contains("different agent")
        ));
    }

    #[tokio::test]
    async fn missing_bundle_returns_the_stock_fallback_signal() {
        let meta = crate::acp::registry::get_agent_meta(AgentType::OpenCode);
        let result = build_agent(meta, &BTreeMap::new(), &Arc::new(StderrTail::new()), None)
            .await
            .expect("missing maintained bundle should not fail stock launch");
        assert!(result.is_none());
    }
}
