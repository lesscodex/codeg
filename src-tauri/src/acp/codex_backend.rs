//! Codex's ACP adapter launch boundary.
//!
//! Codex starts through the repository-packaged `codex-acp` bundle and speaks
//! the same ACP protocol as every other connection. Keeping its adapter-specific
//! launch policy here makes the process boundary explicit without changing the
//! shared connection loop or any other agent launcher.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
#[cfg(feature = "tauri-runtime")]
use std::sync::OnceLock;

use crate::acp::agent_process::AcpAgent;
use crate::acp::connection::{
    agent_debug_callback, apply_codex_env_policy, apply_codex_prompt_suggestion_policy,
    codex_app_server_log_dir, merge_agent_env,
};
use crate::acp::error::AcpError;
use crate::acp::registry::{AcpAgentMeta, AgentDistribution};
use crate::acp::stderr_tail::StderrTail;
use crate::models::agent::AgentType;

const CODEX_ACP_DIR: &str = "codex-acp";
const CODEX_ACP_ENTRY: &str = "dist/index.js";
pub(crate) const CODEX_TURN_STARTED_METHOD: &str = "_codex/turn_started";
#[cfg(not(feature = "tauri-runtime"))]
const DOCKER_CODEX_ACP_DIR: &str = "/opt/codeg/codex-acp";

#[cfg(feature = "tauri-runtime")]
static DESKTOP_RESOURCE_DIR: OnceLock<PathBuf> = OnceLock::new();

#[cfg(feature = "tauri-runtime")]
pub(crate) fn set_desktop_resource_dir(resource_dir: PathBuf) {
    let _ = DESKTOP_RESOURCE_DIR.set(resource_dir);
}

pub(crate) fn packaged_adapter_entry_path(root: &Path) -> PathBuf {
    root.join(CODEX_ACP_ENTRY)
}

fn packaged_adapter_roots() -> Vec<PathBuf> {
    #[cfg(feature = "tauri-runtime")]
    {
        DESKTOP_RESOURCE_DIR
            .get()
            .map(|dir| dir.join(CODEX_ACP_DIR))
            .into_iter()
            .collect()
    }

    #[cfg(not(feature = "tauri-runtime"))]
    {
        let mut roots = Vec::new();
        if let Ok(executable) = std::env::current_exe() {
            if let Some(parent) = executable.parent() {
                roots.push(parent.join(CODEX_ACP_DIR));
            }
        }
        roots.push(PathBuf::from(DOCKER_CODEX_ACP_DIR));
        roots
    }
}

pub(crate) fn packaged_adapter_entry() -> Option<PathBuf> {
    packaged_adapter_roots()
        .into_iter()
        .map(|root| packaged_adapter_entry_path(&root))
        .find(|entry| entry.is_file())
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

/// Build the Codex ACP adapter process while leaving the generic launcher
/// responsible for all other distributions.
pub(crate) async fn build_agent(
    meta: AcpAgentMeta,
    runtime_env: &BTreeMap<String, String>,
    stderr_tail: &Arc<StderrTail>,
    scratch: Option<&Path>,
    prompt_suggestions: bool,
) -> Result<AcpAgent, AcpError> {
    if meta.agent_type != AgentType::Codex {
        return Err(AcpError::SpawnFailed(
            "Codex ACP backend received metadata for a different agent".to_string(),
        ));
    }

    let AgentDistribution::Npx { args, env, .. } = meta.distribution else {
        return Err(AcpError::SpawnFailed(
            "Codex ACP registry entry must use the npx distribution".to_string(),
        ));
    };

    let node = which::which("node").map_err(|_| {
        AcpError::SdkNotInstalled(
            "Node.js is required to launch the packaged Codex ACP adapter".to_string(),
        )
    })?;
    let entry = packaged_adapter_entry().ok_or_else(|| {
        AcpError::SdkNotInstalled(
            "The repository-packaged Codex ACP adapter is not available".to_string(),
        )
    })?;

    let mut merged_env = merge_agent_env(env, runtime_env, scratch);
    let initial_agent_mode = crate::commands::acp::codex_launch_initial_agent_mode();
    let inherited_codex_config = std::env::var("CODEX_CONFIG").ok();
    apply_codex_env_policy(
        AgentType::Codex,
        &mut merged_env,
        initial_agent_mode.as_deref(),
        inherited_codex_config.as_deref(),
    );
    apply_codex_prompt_suggestion_policy(
        AgentType::Codex,
        &mut merged_env,
        prompt_suggestions,
    );

    // codex-acp honors APP_SERVER_LOGS for adapter-side diagnostics. Keep this
    // opt-in so normal launches retain their existing environment exactly.
    let debug_enabled = std::env::var("CODEG_ACP_DEBUG")
        .map(|v| v == "1" || v.eq_ignore_ascii_case("true"))
        .unwrap_or(false);
    if debug_enabled {
        if let Some(dir) = codex_app_server_log_dir() {
            merged_env.push(("APP_SERVER_LOGS".to_string(), dir));
        }
    }

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
            Path::new("/bundle/codex-acp/dist/index.js"),
            &[],
        );
        assert_eq!(
            parts,
            vec![
                "/runtime/node".to_string(),
                "/bundle/codex-acp/dist/index.js".to_string()
            ]
        );
        assert!(!parts.iter().any(|part| part == "codex-acp"));
    }

    #[test]
    fn packaged_entry_path_matches_staged_layout() {
        assert_eq!(
            packaged_adapter_entry_path(Path::new("/bundle/codex-acp")),
            PathBuf::from("/bundle/codex-acp/dist/index.js")
        );
    }

    #[tokio::test]
    async fn backend_rejects_non_codex_metadata() {
        let meta = crate::acp::registry::get_agent_meta(AgentType::Gemini);
        let result = build_agent(
            meta,
            &BTreeMap::new(),
            &Arc::new(StderrTail::new()),
            None,
            true,
        )
        .await;

        assert!(matches!(
            result,
            Err(AcpError::SpawnFailed(message))
                if message.contains("different agent")
        ));
    }

    #[test]
    fn codex_policy_forces_mcp_filter_off_and_overrides_user_twin() {
        let mut env = vec![("PATH".to_string(), "/usr/bin".to_string())];
        apply_codex_env_policy(AgentType::Codex, &mut env, None, None);
        assert!(env
            .iter()
            .any(|(key, value)| key == "DISABLE_MCP_CONFIG_FILTERING" && value == "true"));

        let mut with_twin = vec![(
            "DISABLE_MCP_CONFIG_FILTERING".to_string(),
            "false".to_string(),
        )];
        apply_codex_env_policy(AgentType::Codex, &mut with_twin, None, None);
        let hits: Vec<_> = with_twin
            .iter()
            .filter(|(key, _)| key == "DISABLE_MCP_CONFIG_FILTERING")
            .collect();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].1, "true");
    }

    #[test]
    fn codex_policy_injects_config_derived_mode() {
        let mut env = vec![("PATH".to_string(), "/usr/bin".to_string())];
        apply_codex_env_policy(AgentType::Codex, &mut env, Some("read-only"), None);
        assert!(env
            .iter()
            .any(|(key, value)| key == "INITIAL_AGENT_MODE" && value == "read-only"));

        let mut no_mode = vec![("PATH".to_string(), "/usr/bin".to_string())];
        apply_codex_env_policy(AgentType::Codex, &mut no_mode, None, None);
        assert!(!no_mode.iter().any(|(key, _)| key == "INITIAL_AGENT_MODE"));
    }

    #[test]
    fn explicit_mode_wins_and_blank_mode_is_replaced() {
        let mut explicit = vec![(
            "INITIAL_AGENT_MODE".to_string(),
            "agent-full-access".to_string(),
        )];
        apply_codex_env_policy(
            AgentType::Codex,
            &mut explicit,
            Some("read-only"),
            None,
        );
        let hits: Vec<_> = explicit
            .iter()
            .filter(|(key, _)| key == "INITIAL_AGENT_MODE")
            .collect();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].1, "agent-full-access");

        let mut blank = vec![("INITIAL_AGENT_MODE".to_string(), "  ".to_string())];
        apply_codex_env_policy(AgentType::Codex, &mut blank, Some("read-only"), None);
        let hits: Vec<_> = blank
            .iter()
            .filter(|(key, _)| key == "INITIAL_AGENT_MODE")
            .collect();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].1, "read-only");
    }

    #[test]
    fn codex_policy_is_noop_for_other_agents() {
        for agent in [AgentType::Grok, AgentType::ClaudeCode, AgentType::Gemini] {
            let mut env = vec![("PATH".to_string(), "/usr/bin".to_string())];
            apply_codex_env_policy(agent, &mut env, Some("read-only"), None);
            assert!(!env
                .iter()
                .any(|(key, _)| key == "DISABLE_MCP_CONFIG_FILTERING"));
            assert!(!env.iter().any(|(key, _)| key == "INITIAL_AGENT_MODE"));
        }
    }
}
