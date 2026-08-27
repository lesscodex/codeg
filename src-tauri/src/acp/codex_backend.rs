//! Codex's ACP adapter launch boundary.
//!
//! Codex still starts through the published `codex-acp` package and speaks the
//! same ACP protocol as every other connection. Keeping its adapter-specific
//! launch policy here makes the process boundary explicit without changing the
//! shared connection loop or any other agent launcher.

use std::collections::BTreeMap;
use std::path::Path;
use std::sync::Arc;

use crate::acp::agent_process::AcpAgent;
use crate::acp::connection::{agent_debug_callback, merge_agent_env};
use crate::acp::error::AcpError;
use crate::acp::registry::{AcpAgentMeta, AgentDistribution};
use crate::acp::stderr_tail::StderrTail;
use crate::models::agent::AgentType;

/// Directory handed to codex-acp via `APP_SERVER_LOGS` so its adapter-side
/// (ACP ↔ Codex app-server translation) logs land on disk for support.
///
/// Returns `None` — and the caller injects nothing — when the system cache dir
/// is unknown or the directory cannot be created: diagnostics must never block
/// a connection.
fn app_server_log_dir() -> Option<String> {
    let dir = dirs::cache_dir()?
        .join("app.codeg")
        .join("acp-logs")
        .join("codex-acp");
    std::fs::create_dir_all(&dir).ok()?;
    Some(dir.to_string_lossy().into_owned())
}

/// Codex-only launch policy. The adapter otherwise filters ACP MCP entries
/// when their names collide with `config.toml`; codeg's injected `codeg-mcp`
/// must survive that filter so its collaboration tools remain available.
///
/// `initial_agent_mode` is resolved by the caller from Codex's config file and
/// passed in so this function remains pure.
pub(crate) fn apply_codex_env_policy(
    agent_type: AgentType,
    merged: &mut Vec<(String, String)>,
    initial_agent_mode: Option<&str>,
) {
    if agent_type != AgentType::Codex {
        return;
    }

    let key = "DISABLE_MCP_CONFIG_FILTERING";
    merged.retain(|(k, _)| k != key);
    merged.push((key.to_string(), "true".to_string()));

    // codex-acp re-sends its selected approval and sandbox policy every turn.
    // Seed the adapter from the user's config unless an explicit launch env
    // value already wins.
    let mode_key = "INITIAL_AGENT_MODE";
    if merged
        .iter()
        .any(|(k, v)| k == mode_key && !v.trim().is_empty())
    {
        return;
    }
    if let Some(mode) = initial_agent_mode {
        merged.retain(|(k, _)| k != mode_key);
        merged.push((mode_key.to_string(), mode.to_string()));
    }
}

/// Build the Codex ACP adapter process while leaving the generic launcher
/// responsible for all other distributions.
pub(crate) async fn build_agent(
    meta: AcpAgentMeta,
    runtime_env: &BTreeMap<String, String>,
    stderr_tail: &Arc<StderrTail>,
    scratch: Option<&Path>,
) -> Result<AcpAgent, AcpError> {
    if meta.agent_type != AgentType::Codex {
        return Err(AcpError::SpawnFailed(
            "Codex ACP backend received metadata for a different agent".to_string(),
        ));
    }

    let AgentDistribution::Npx { cmd, args, env, .. } = meta.distribution else {
        return Err(AcpError::SpawnFailed(
            "Codex ACP registry entry must use the npx distribution".to_string(),
        ));
    };

    let mut merged_env = merge_agent_env(env, runtime_env, scratch);
    let initial_agent_mode = crate::commands::acp::codex_launch_initial_agent_mode();
    apply_codex_env_policy(
        AgentType::Codex,
        &mut merged_env,
        initial_agent_mode.as_deref(),
    );

    // codex-acp honors APP_SERVER_LOGS for adapter-side diagnostics. Keep this
    // opt-in so normal launches retain their existing environment exactly.
    let debug_enabled = std::env::var("CODEG_ACP_DEBUG")
        .map(|v| v == "1" || v.eq_ignore_ascii_case("true"))
        .unwrap_or(false);
    if debug_enabled {
        if let Some(dir) = app_server_log_dir() {
            merged_env.push(("APP_SERVER_LOGS".to_string(), dir));
        }
    }

    let mut parts: Vec<String> = merged_env
        .iter()
        .map(|(key, value)| format!("{key}={value}"))
        .collect();
    parts.push(
        crate::commands::acp::resolve_npx_command(cmd)
            .await
            .map(|path| path.to_string_lossy().to_string())
            .unwrap_or_else(|| {
                crate::process::normalized_program(cmd)
                    .to_string_lossy()
                    .to_string()
            }),
    );
    parts.extend(args.iter().map(|arg| (*arg).to_string()));

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

    #[tokio::test]
    async fn backend_rejects_non_codex_metadata() {
        let meta = crate::acp::registry::get_agent_meta(AgentType::Gemini);
        let result =
            build_agent(meta, &BTreeMap::new(), &Arc::new(StderrTail::new()), None).await;

        assert!(matches!(
            result,
            Err(AcpError::SpawnFailed(message))
                if message.contains("different agent")
        ));
    }

    #[test]
    fn codex_policy_forces_mcp_filter_off_and_overrides_user_twin() {
        let mut env = vec![("PATH".to_string(), "/usr/bin".to_string())];
        apply_codex_env_policy(AgentType::Codex, &mut env, None);
        assert!(env
            .iter()
            .any(|(key, value)| key == "DISABLE_MCP_CONFIG_FILTERING" && value == "true"));

        let mut with_twin = vec![(
            "DISABLE_MCP_CONFIG_FILTERING".to_string(),
            "false".to_string(),
        )];
        apply_codex_env_policy(AgentType::Codex, &mut with_twin, None);
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
        apply_codex_env_policy(AgentType::Codex, &mut env, Some("read-only"));
        assert!(env
            .iter()
            .any(|(key, value)| key == "INITIAL_AGENT_MODE" && value == "read-only"));

        let mut no_mode = vec![("PATH".to_string(), "/usr/bin".to_string())];
        apply_codex_env_policy(AgentType::Codex, &mut no_mode, None);
        assert!(!no_mode.iter().any(|(key, _)| key == "INITIAL_AGENT_MODE"));
    }

    #[test]
    fn explicit_mode_wins_and_blank_mode_is_replaced() {
        let mut explicit = vec![(
            "INITIAL_AGENT_MODE".to_string(),
            "agent-full-access".to_string(),
        )];
        apply_codex_env_policy(AgentType::Codex, &mut explicit, Some("read-only"));
        let hits: Vec<_> = explicit
            .iter()
            .filter(|(key, _)| key == "INITIAL_AGENT_MODE")
            .collect();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].1, "agent-full-access");

        let mut blank = vec![("INITIAL_AGENT_MODE".to_string(), "  ".to_string())];
        apply_codex_env_policy(AgentType::Codex, &mut blank, Some("read-only"));
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
            apply_codex_env_policy(agent, &mut env, Some("read-only"));
            assert!(!env
                .iter()
                .any(|(key, _)| key == "DISABLE_MCP_CONFIG_FILTERING"));
            assert!(!env.iter().any(|(key, _)| key == "INITIAL_AGENT_MODE"));
        }
    }
}
