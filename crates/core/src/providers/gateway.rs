//! Gateway enforcement (Enterprise Edition Phase 3).
//!
//! When `policies.gateway.enabled: true`, every cloud-provider HTTP
//! call is rerouted to the org's private gateway (LiteLLM, Portkey,
//! Helicone, internal proxy, etc.). The gateway is assumed to speak
//! the OpenAI Chat Completions wire format and route to upstream
//! providers based on the `model` field — that's what every common
//! LLM gateway product does today.
//!
//! ## What this means at the provider layer
//!
//! When the gate is active:
//!
//! - `build_provider` returns a single `OpenAIProvider` pointing at the
//!   gateway URL, regardless of which `ProviderKind` the user picked.
//! - The user's per-provider API keys (`ANTHROPIC_API_KEY`, `OPENAI_API_KEY`,
//!   etc.) are **ignored**. The gateway has its own credentials.
//! - The auth header is rendered from the policy's
//!   `auth_header_template` (`{{env:NAME}}` substitutions for tokens
//!   stored in env vars; `{{sso_token}}` substitution post-Phase 4).
//! - The model id is passed to the gateway unchanged. The gateway is
//!   responsible for upstream provider selection.
//!
//! ## Local models — the escape valve
//!
//! `policies.gateway.read_only_local_models_allowed: true` lets local
//! providers (Ollama, LMStudio, AgentSdk) bypass the gateway and run
//! directly. The use case: an org wants gateway-mediated access to
//! cloud LLMs but still allows engineers to use their own local
//! models for offline / private work. Off by default — strict enterprise
//! deployments expect everything to route through the gateway.
//!
//! ## What this module does *not* yet enforce
//!
//! `policies.gateway.fail_closed: true` is *declared* but not yet
//! enforced at the network layer. The current implementation prevents
//! direct provider calls by replacing the provider entirely at
//! `build_provider`-time — there's no path through the agent loop
//! that can bypass it. A future hardening pass could add a
//! `reqwest::Client` wrapper that rejects any HTTP request whose host
//! doesn't match the gateway, as defense in depth. Tracked in the
//! dev-plan as a Phase 3 follow-up.

use crate::policy::GatewayPolicy;
use crate::providers::ProviderKind;

/// The generic gateway block. A `kind: "thclaws"` gateway is not this:
/// it keeps the thClaws providers and locks them to the customer's own
/// gateway (`crate::policy::thclaws_gateway`), so nothing here applies.
fn generic() -> Option<&'static GatewayPolicy> {
    crate::policy::active()
        .and_then(|a| a.policy.policies.gateway.as_ref())
        .filter(|g| !g.is_thclaws())
}

/// `true` when a verified org policy is active and `policies.gateway`
/// is `enabled`. Cheap — doesn't allocate.
pub fn is_active() -> bool {
    generic().map(|g| g.enabled).unwrap_or(false)
}

/// The gateway URL when active. Returns `None` when the gate is off
/// or no policy is loaded.
pub fn gateway_url() -> Option<String> {
    generic()
        .filter(|g| g.enabled && !g.url.trim().is_empty())
        .map(|g| g.url.clone())
}

/// Whether this `ProviderKind` should be routed through the gateway.
/// Returns `true` when the gate is active AND this is a cloud provider
/// (or `read_only_local_models_allowed` is false). Local providers
/// (Ollama, LMStudio, AgentSdk) bypass when `read_only_local_models_allowed`
/// is set.
pub fn should_route(kind: ProviderKind) -> bool {
    let g = match generic() {
        Some(g) if g.enabled => g,
        _ => return false,
    };
    if g.read_only_local_models_allowed && is_local_provider(kind) {
        return false;
    }
    true
}

/// Local-execution providers — the ones the `read_only_local_models_allowed`
/// escape valve unblocks. Cloud providers always route through the gateway
/// when the gate is active.
fn is_local_provider(kind: ProviderKind) -> bool {
    matches!(
        kind,
        ProviderKind::Ollama
            | ProviderKind::OllamaAnthropic
            | ProviderKind::LMStudio
            | ProviderKind::VLlm
            | ProviderKind::LlamaCpp
            | ProviderKind::AgentSdk
    )
}

/// Resolve the auth header value to send to the gateway. Substitutes:
///
/// - `{{env:NAME}}` → contents of the `NAME` env var (empty string if
///   unset; deliberate so a missing token surfaces as a clean upstream
///   401 rather than a panic at startup).
/// - `{{sso_token}}` → empty string in Phase 3; Phase 4 will populate
///   from the active OIDC session.
///
/// Returns `None` when the gate is off or the template is empty.
/// Returns `Some("")` when the template renders to an empty string —
/// the OpenAI client always sends *some* Authorization header, so an
/// empty Bearer is fine for testing / unauthenticated gateway proxies.
pub fn resolve_auth_header() -> Option<String> {
    let template = generic()
        .filter(|g| g.enabled)
        .and_then(|g| g.auth_header_template.clone())?;
    Some(render_template(&template))
}

/// Apply the documented substitutions. Public for testing — most
/// callers should use `resolve_auth_header()`.
///
/// M6.21 BUG M3: substitution is depth-capped at `MAX_DEPTH` to prevent
/// an infinite loop if env values themselves contain `{{env:...}}`
/// references that form a cycle (e.g. `A=`{{env:B}}`, `B=`{{env:A}}`).
/// 16 substitutions is generous — real templates have 1-2 — and a
/// hung worker startup would otherwise wedge the GUI.
pub fn render_template(template: &str) -> String {
    const MAX_DEPTH: usize = 16;
    let mut out = template.to_string();
    // {{env:NAME}} → process env value (or empty).
    for _ in 0..MAX_DEPTH {
        let Some(start) = out.find("{{env:") else {
            break;
        };
        let after = &out[start + 6..];
        let Some(end_offset) = after.find("}}") else {
            break;
        };
        let name = &after[..end_offset];
        let value = std::env::var(name).unwrap_or_default();
        let full_token_end = start + 6 + end_offset + 2;
        out.replace_range(start..full_token_end, &value);
    }
    // {{sso_token}} → access_token from the active SSO session, when
    // policies.sso is enabled and the user is logged in. Renders to
    // empty string when no session — the gateway will surface a 401
    // and the user is prompted to run /sso login.
    if out.contains("{{sso_token}}") {
        let token = crate::policy::active()
            .and_then(|a| a.policy.policies.sso.as_ref())
            .filter(|s| s.enabled)
            .and_then(crate::sso::current_access_token)
            .unwrap_or_default();
        out = out.replace("{{sso_token}}", &token);
    }
    out
}

/// `true` when the `fail_closed` sub-policy is set. Currently advisory —
/// see module-level docs for the security stance.
pub fn fail_closed() -> bool {
    generic()
        .map(|g| g.enabled && g.fail_closed)
        .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn render_template_substitutes_env() {
        std::env::set_var("THCLAWS_TEST_GATEWAY_TOKEN", "abc-secret-123");
        let rendered = render_template("Bearer {{env:THCLAWS_TEST_GATEWAY_TOKEN}}");
        assert_eq!(rendered, "Bearer abc-secret-123");
        std::env::remove_var("THCLAWS_TEST_GATEWAY_TOKEN");
    }

    #[test]
    fn render_template_unset_env_produces_empty() {
        let rendered = render_template("Bearer {{env:THCLAWS_NONEXISTENT_VAR_XYZ}}");
        assert_eq!(rendered, "Bearer ");
    }

    #[test]
    fn render_template_sso_placeholder_phase3_is_empty() {
        let rendered = render_template("Bearer {{sso_token}}");
        assert_eq!(rendered, "Bearer ");
    }

    #[test]
    fn render_template_combines_multiple_substitutions() {
        std::env::set_var("THCLAWS_TEST_KEY", "k1");
        let rendered = render_template("X-Org: acme; X-Token: {{env:THCLAWS_TEST_KEY}}");
        assert_eq!(rendered, "X-Org: acme; X-Token: k1");
        std::env::remove_var("THCLAWS_TEST_KEY");
    }

    #[test]
    fn render_template_literal_passthrough() {
        let rendered = render_template("Bearer static-token-12345");
        assert_eq!(rendered, "Bearer static-token-12345");
    }

    /// M6.21 BUG M3: cyclic env-var values (A → `{{env:B}}`, B → `{{env:A}}`)
    /// would loop forever pre-fix. Verify the depth cap (16 substitutions)
    /// terminates instead of hanging worker startup. We can't easily set
    /// up a true cycle (env vars referencing each other on the same name
    /// is the simplest provable cycle), but a self-referential env IS
    /// cyclic and must terminate.
    #[test]
    fn render_template_terminates_on_cyclic_env_substitution() {
        // Set env A's value to a fresh `{{env:NAME}}` reference.
        // Loop must terminate in ≤16 substitutions even if both var
        // values keep producing the same `{{env:...}}` token.
        std::env::set_var("THCLAWS_TEST_CYCLE_A", "{{env:THCLAWS_TEST_CYCLE_A}}");
        let rendered = render_template("{{env:THCLAWS_TEST_CYCLE_A}}");
        // After MAX_DEPTH=16 substitutions, the unresolved tail remains.
        // The exact tail doesn't matter — what matters is the call returns.
        assert!(
            rendered.contains("{{env:THCLAWS_TEST_CYCLE_A}}") || rendered.is_empty(),
            "expected the loop to terminate with a tail or empty string, got: {rendered:?}",
        );
        std::env::remove_var("THCLAWS_TEST_CYCLE_A");
    }

    #[test]
    fn is_local_provider_classification() {
        assert!(is_local_provider(ProviderKind::Ollama));
        assert!(is_local_provider(ProviderKind::OllamaAnthropic));
        assert!(is_local_provider(ProviderKind::LMStudio));
        assert!(is_local_provider(ProviderKind::AgentSdk));
        assert!(!is_local_provider(ProviderKind::Anthropic));
        assert!(!is_local_provider(ProviderKind::OpenAI));
        assert!(!is_local_provider(ProviderKind::Gemini));
        assert!(!is_local_provider(ProviderKind::OllamaCloud));
    }
}
