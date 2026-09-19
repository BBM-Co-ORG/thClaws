//! `GET /v1/agent/info` — capability snapshot for orchestrators.
//!
//! Read-only, idempotent description of what this thClaws daemon
//! knows: skills, MCP servers, model catalogue, version, optional
//! external-access URL. An orchestrator polls it to show current
//! capability info without the daemon having to push any of it.
//!
//! Cached for ~30s. The skill scan is cheap (filesystem walk + parse)
//! but the cache exists so a dashboard that fans out to N daemons on
//! every refresh doesn't melt them.

use axum::response::Json;
use serde::Serialize;
use std::sync::OnceLock;
use std::time::{Duration, Instant};
use tokio::sync::Mutex;

use super::AuthOk;

const CACHE_TTL: Duration = Duration::from_secs(30);

#[derive(Serialize, Clone, Debug)]
pub struct AgentInfo {
    pub version: &'static str,
    pub git_sha: &'static str,
    pub git_dirty: bool,
    pub build_profile: &'static str,
    /// thClaws's working directory at daemon start. For a pod this is
    /// usually `/workspace`; for a locally-spawned daemon it's the
    /// caller's own working directory.
    pub workspace_dir: String,
    pub skills: Vec<SkillInfo>,
    pub mcp_servers: Vec<McpServerInfo>,
    pub model_capabilities: ModelCapabilities,
    pub external_access: Option<ExternalAccess>,
    pub features: Features,
}

#[derive(Serialize, Clone, Debug)]
pub struct SkillInfo {
    pub key: String,
    pub name: String,
    pub description: String,
    pub when_to_use: String,
    /// Where the skill was discovered: `"builtin"`, `"user"`,
    /// `"plugin"`, or `"project"`.
    pub source: &'static str,
}

#[derive(Serialize, Clone, Debug)]
pub struct McpServerInfo {
    pub name: String,
    pub command: Option<String>,
    /// `null` ⇒ tool count not yet probed. The endpoint doesn't spawn
    /// MCP servers just to count their tools — that's deferred so it
    /// stays cheap. Clients should treat `null` as "unknown, will
    /// populate after first agent run".
    pub tool_count: Option<usize>,
}

#[derive(Serialize, Clone, Debug)]
pub struct ModelCapabilities {
    pub default_model: String,
    pub available_models: Vec<String>,
    pub supports_streaming: bool,
    pub supports_x_callback: bool,
    pub supports_agent_run: bool,
}

#[derive(Serialize, Clone, Debug)]
pub struct ExternalAccess {
    pub ui_url: String,
    pub configured: bool,
}

#[derive(Serialize, Clone, Debug, Default)]
pub struct Features {
    /// dev-plan/26: capability poll endpoint present.
    pub agent_info: bool,
    /// dev-plan/25: agent-shaped endpoint with workspace-scoped runtime.
    pub agent_run: bool,
    /// dev-plan/19: OpenAI-compatible chat completions.
    pub chat_completions: bool,
    /// dev-plan/23: fire-and-forget async webhook delivery.
    pub x_callback: bool,
}

struct Cached {
    snapshot: AgentInfo,
    fetched_at: Instant,
}

fn cache() -> &'static Mutex<Option<Cached>> {
    static CELL: OnceLock<Mutex<Option<Cached>>> = OnceLock::new();
    CELL.get_or_init(|| Mutex::new(None))
}

pub async fn get_info(_auth: AuthOk) -> Json<AgentInfo> {
    let mut guard = cache().lock().await;
    if let Some(entry) = guard.as_ref() {
        if entry.fetched_at.elapsed() < CACHE_TTL {
            return Json(entry.snapshot.clone());
        }
    }
    let fresh = build_snapshot();
    *guard = Some(Cached {
        snapshot: fresh.clone(),
        fetched_at: Instant::now(),
    });
    Json(fresh)
}

/// Test-only: clear the cache so back-to-back assertions can observe
/// a fresh build.
#[cfg(test)]
pub(crate) async fn _reset_cache_for_tests() {
    *cache().lock().await = None;
}

fn build_snapshot() -> AgentInfo {
    let v = crate::version::info();
    let config = crate::config::AppConfig::load().unwrap_or_default();
    AgentInfo {
        version: v.version,
        git_sha: v.git_sha,
        git_dirty: v.git_dirty,
        build_profile: v.build_profile,
        workspace_dir: std::env::current_dir()
            .ok()
            .and_then(|p| p.into_os_string().into_string().ok())
            .unwrap_or_default(),
        skills: collect_skills(),
        mcp_servers: collect_mcp_servers(&config),
        model_capabilities: collect_model_capabilities(&config),
        external_access: collect_external_access(),
        features: Features {
            agent_info: true,
            agent_run: true,
            chat_completions: true,
            x_callback: true,
        },
    }
}

fn collect_skills() -> Vec<SkillInfo> {
    let store = crate::skills::SkillStore::discover();
    let mut entries: Vec<SkillInfo> = store
        .skills
        .values()
        .map(|skill| {
            let dir = skill.dir.display().to_string();
            let source = classify_skill_source(&dir);
            SkillInfo {
                key: skill.name.clone(),
                name: skill.name.clone(),
                description: skill.description.clone(),
                when_to_use: skill.when_to_use.clone(),
                source,
            }
        })
        .collect();
    entries.sort_by(|a, b| a.key.cmp(&b.key));
    entries
}

fn classify_skill_source(dir: &str) -> &'static str {
    if dir.starts_with("<builtin>/") {
        "builtin"
    } else if dir.contains("/.thclaws/skills") || dir.contains("/.claude/skills") {
        // Both relative-to-cwd (project) and home-relative (user) match
        // this; distinguish on absolute-path prefix.
        if let Some(home) = crate::util::home_dir() {
            if let Some(home_str) = home.to_str() {
                if dir.starts_with(home_str) {
                    return "user";
                }
            }
        }
        "project"
    } else {
        "plugin"
    }
}

fn collect_mcp_servers(config: &crate::config::AppConfig) -> Vec<McpServerInfo> {
    let mut out: Vec<McpServerInfo> = config
        .mcp_servers
        .iter()
        .map(|s| McpServerInfo {
            name: s.name.clone(),
            command: mcp_command_summary(s),
            tool_count: None,
        })
        .collect();
    for p in crate::plugins::plugin_mcp_servers() {
        if out.iter().any(|s| s.name == p.name) {
            continue;
        }
        out.push(McpServerInfo {
            name: p.name.clone(),
            command: mcp_command_summary(&p),
            tool_count: None,
        });
    }
    out.sort_by(|a, b| a.name.cmp(&b.name));
    out
}

fn mcp_command_summary(s: &crate::mcp::McpServerConfig) -> Option<String> {
    if !s.command.is_empty() {
        let mut full = s.command.clone();
        if !s.args.is_empty() {
            full.push(' ');
            full.push_str(&s.args.join(" "));
        }
        Some(full)
    } else if !s.url.is_empty() {
        // HTTP-transport MCP servers don't have a command. Show the
        // URL so operators can see what they configured.
        Some(s.url.clone())
    } else {
        None
    }
}

fn collect_model_capabilities(config: &crate::config::AppConfig) -> ModelCapabilities {
    let cat = crate::model_catalogue::EffectiveCatalogue::load();
    let mut available: Vec<String> = Vec::new();
    let mut seen = std::collections::HashSet::new();
    let layers = [cat.cache.as_ref(), Some(&cat.baseline)];
    for layer in layers.into_iter().flatten() {
        for provider_cat in layer.providers.values() {
            for (model_id, entry) in &provider_cat.models {
                if entry.chat == Some(false) {
                    continue;
                }
                if seen.insert(model_id.clone()) {
                    available.push(model_id.clone());
                }
            }
        }
    }
    available.sort();
    ModelCapabilities {
        default_model: config.model.clone(),
        available_models: available,
        supports_streaming: true,
        supports_x_callback: true,
        supports_agent_run: true,
    }
}

fn collect_external_access() -> Option<ExternalAccess> {
    let url = std::env::var("THCLAWS_EXTERNAL_URL")
        .ok()
        .filter(|s| !s.trim().is_empty())?;
    Some(ExternalAccess {
        configured: true,
        ui_url: url,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classify_skill_source_paths() {
        assert_eq!(classify_skill_source("<builtin>/foo"), "builtin");
        // Project skills land under cwd-relative paths.
        if let Some(home) = crate::util::home_dir() {
            let user_path = format!("{}/.claude/skills/foo", home.display());
            assert_eq!(classify_skill_source(&user_path), "user");
        }
        // Anything else (e.g. plugin-skill dir) is bucketed as plugin.
        assert_eq!(
            classify_skill_source("/var/plugins/something/skills/foo"),
            "plugin"
        );
    }

    #[tokio::test]
    async fn build_snapshot_has_required_fields() {
        let snap = build_snapshot();
        assert!(!snap.workspace_dir.is_empty());
        assert!(snap.features.agent_info);
        assert!(snap.features.agent_run);
        // Model capabilities always populated even when config has no
        // explicit default.
        assert!(snap.model_capabilities.supports_streaming);
    }

    #[tokio::test]
    async fn cache_returns_same_instance_within_ttl() {
        let _guard = super::super::ENV_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        _reset_cache_for_tests().await;
        let prior_token = std::env::var("THCLAWS_API_TOKEN").ok();
        std::env::set_var("THCLAWS_API_TOKEN", "test-cache");

        let first = get_info(AuthOk).await;
        let second = get_info(AuthOk).await;
        // Same workspace_dir + same skills list (we didn't mutate state
        // between calls). Strong equality on a few stable fields.
        assert_eq!(first.workspace_dir, second.workspace_dir);
        assert_eq!(first.skills.len(), second.skills.len());

        match prior_token {
            Some(v) => std::env::set_var("THCLAWS_API_TOKEN", v),
            None => std::env::remove_var("THCLAWS_API_TOKEN"),
        }
    }

    #[tokio::test]
    async fn external_access_reflects_env() {
        let prior = std::env::var("THCLAWS_EXTERNAL_URL").ok();
        std::env::remove_var("THCLAWS_EXTERNAL_URL");
        assert!(collect_external_access().is_none());

        std::env::set_var("THCLAWS_EXTERNAL_URL", "https://agent.example.com");
        let ea = collect_external_access().unwrap();
        assert_eq!(ea.ui_url, "https://agent.example.com");
        assert!(ea.configured);

        match prior {
            Some(v) => std::env::set_var("THCLAWS_EXTERNAL_URL", v),
            None => std::env::remove_var("THCLAWS_EXTERNAL_URL"),
        }
    }
}
