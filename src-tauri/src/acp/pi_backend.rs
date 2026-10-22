//! Maintained pi-acp launch boundary.
//!
//! Pi's ACP adapter is built and shipped by Codeg from the pinned `pi-acp`
//! submodule.  The adapter then starts the separately installed Pi CLI, so the
//! package and the vendor runtime remain distinct prerequisites.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
#[cfg(feature = "tauri-runtime")]
use std::sync::OnceLock;

use crate::acp::agent_process::AcpAgent;
use crate::acp::connection::{agent_debug_callback, merge_agent_env};
use crate::acp::error::AcpError;
use crate::acp::registry::{AcpAgentMeta, AgentDistribution};
use crate::acp::stderr_tail::StderrTail;
use crate::models::agent::AgentType;

const PI_ACP_DIR: &str = "pi-acp";
const PI_ACP_ENTRY: &str = "dist/index.js";
const PI_ACP_EXTENSION: &str = "dist/pi-mcp-extension.js";
#[cfg(not(feature = "tauri-runtime"))]
const DOCKER_PI_ACP_DIR: &str = "/opt/codeg/pi-acp";

/// Tauri's resource directory is captured once during setup so desktop
/// preflight, status and connection launch all inspect the same bundle.
#[cfg(feature = "tauri-runtime")]
static DESKTOP_RESOURCE_DIR: OnceLock<PathBuf> = OnceLock::new();

#[cfg(feature = "tauri-runtime")]
pub(crate) fn set_desktop_resource_dir(resource_dir: PathBuf) {
    let _ = DESKTOP_RESOURCE_DIR.set(resource_dir);
}

pub(crate) fn packaged_adapter_entry_path(root: &Path) -> PathBuf {
    root.join(PI_ACP_ENTRY)
}

pub(crate) fn packaged_adapter_extension_path(root: &Path) -> PathBuf {
    root.join(PI_ACP_EXTENSION)
}

fn packaged_adapter_roots() -> Vec<PathBuf> {
    #[cfg(feature = "tauri-runtime")]
    {
        return DESKTOP_RESOURCE_DIR
            .get()
            .map(|dir| dir.join(PI_ACP_DIR))
            .into_iter()
            .collect();
    }

    #[cfg(not(feature = "tauri-runtime"))]
    {
        let mut roots = Vec::new();
        if let Ok(executable) = std::env::current_exe() {
            if let Some(parent) = executable.parent() {
                // Release tarballs keep codeg-server and pi-acp side by side.
                roots.push(parent.join(PI_ACP_DIR));
            }
        }
        // Dockerfile and Dockerfile.ci copy the same artifact here.
        roots.push(PathBuf::from(DOCKER_PI_ACP_DIR));
        roots
    }
}

fn packaged_adapter_is_complete(root: &Path) -> bool {
    packaged_adapter_entry_path(root).is_file() && packaged_adapter_extension_path(root).is_file()
}

/// Resolve the complete repository-packaged adapter. There is deliberately no
/// PATH/npm fallback: a missing staged bundle is a packaging/runtime error.
pub(crate) fn packaged_adapter_entry() -> Option<PathBuf> {
    packaged_adapter_roots()
        .into_iter()
        .find(|root| packaged_adapter_is_complete(root))
        .map(|root| packaged_adapter_entry_path(&root))
}

fn packaged_adapter_process_args(node: &Path, entry: &Path, args: &[&str]) -> Vec<String> {
    let mut parts = vec![
        crate::process::normalized_program(node)
            .to_string_lossy()
            .into_owned(),
        entry.to_string_lossy().into_owned(),
    ];
    parts.extend(args.iter().map(|arg| (*arg).to_string()));
    parts
}

/// Build the maintained Pi ACP adapter with Node and the staged local entry.
/// The generic launcher remains responsible for every other distribution.
pub(crate) async fn build_agent(
    meta: AcpAgentMeta,
    runtime_env: &BTreeMap<String, String>,
    stderr_tail: &Arc<StderrTail>,
    scratch: Option<&Path>,
) -> Result<AcpAgent, AcpError> {
    if meta.agent_type != AgentType::Pi {
        return Err(AcpError::SpawnFailed(
            "Pi ACP backend received metadata for a different agent".to_string(),
        ));
    }

    let AgentDistribution::Npx { args, env, .. } = meta.distribution else {
        return Err(AcpError::SpawnFailed(
            "Pi ACP registry entry must use the npx distribution".to_string(),
        ));
    };

    let node = which::which("node").map_err(|_| {
        AcpError::SdkNotInstalled(
            "Node.js is required to launch the packaged Pi ACP adapter".to_string(),
        )
    })?;
    let entry = packaged_adapter_entry().ok_or_else(|| {
        AcpError::SdkNotInstalled(
            "The repository-packaged Pi ACP adapter is not available".to_string(),
        )
    })?;

    let merged_env = merge_agent_env(env, runtime_env, scratch);
    let mut parts: Vec<String> = merged_env
        .iter()
        .map(|(key, value)| format!("{key}={value}"))
        .collect();
    parts.extend(packaged_adapter_process_args(&node, &entry, args));

    let refs: Vec<&str> = parts.iter().map(String::as_str).collect();
    let agent_name = meta.name.to_string();
    let tail = Arc::clone(stderr_tail);
    AcpAgent::from_args(&refs)
        .map(|agent| agent.with_debug(agent_debug_callback(agent_name, tail, false)))
        .map_err(|err| AcpError::SpawnFailed(err.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn packaged_launch_uses_node_and_local_entry() {
        let parts = packaged_adapter_process_args(
            Path::new("/runtime/node"),
            Path::new("/bundle/pi-acp/dist/index.js"),
            &[],
        );

        assert_eq!(
            parts,
            vec![
                "/runtime/node".to_string(),
                "/bundle/pi-acp/dist/index.js".to_string()
            ]
        );
        assert!(!parts.iter().any(|part| part == "pi-acp"));
    }

    #[test]
    fn packaged_layout_requires_the_mcp_extension() {
        let root = Path::new("/bundle/pi-acp");
        assert_eq!(
            packaged_adapter_entry_path(root),
            PathBuf::from("/bundle/pi-acp/dist/index.js")
        );
        assert_eq!(
            packaged_adapter_extension_path(root),
            PathBuf::from("/bundle/pi-acp/dist/pi-mcp-extension.js")
        );
    }

    #[tokio::test]
    async fn backend_rejects_non_pi_metadata() {
        let meta = crate::acp::registry::get_agent_meta(AgentType::Gemini);
        let result =
            build_agent(meta, &BTreeMap::new(), &Arc::new(StderrTail::new()), None).await;

        assert!(matches!(
            result,
            Err(AcpError::SpawnFailed(message))
                if message.contains("different agent")
        ));
    }
}
