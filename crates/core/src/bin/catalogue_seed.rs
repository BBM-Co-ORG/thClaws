//! catalogue-seed — operator tool that merges provider `/v1/models`
//! output into the baseline catalogue JSON without overwriting
//! hand-curated rows.
//!
//! Providers probed (each gated on the presence of its API key so the
//! tool degrades gracefully when only some keys are configured):
//!
//!   - OpenRouter   (always, no key needed)  → long-tail filler
//!   - Anthropic    (ANTHROPIC_API_KEY)      → real dated ids, context from OpenRouter or existing curation
//!   - OpenAI       (OPENAI_API_KEY)         → real dated ids, context from OpenRouter or existing curation
//!   - Gemini       (GEMINI_API_KEY)         → real ids + inputTokenLimit
//!   - DeepSeek     (DEEPSEEK_API_KEY)       → V4 line (flash/pro), context from OpenRouter mirror or default 128K
//!   - Ollama       (if OLLAMA_HOST reachable, default http://localhost:11434)
//!
//! New ids are inserted into the appropriate `providers.<name>.models`
//! submap. Hand-curated rows are never overwritten — the `id` is the
//! map key and we only write when absent. Stale rows are left in place;
//! a vendor removing a model doesn't delete its entry automatically
//! (operator deletes manually after reviewing the diff).
//!
//! Usage:
//!   cargo run --bin catalogue-seed -- [path/to/model_catalogue.json]
//!
//! Exit non-zero on any hard failure so CI can gate a refresh PR.

use serde::Deserialize;
use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use thclaws_core::model_catalogue::{
    Catalogue, ModelEntry, ProviderCatalogue, CONTEXT_UNVERIFIED_MARK, CURRENT_SCHEMA,
};

const OPENROUTER_URL: &str = "https://openrouter.ai/api/v1/models";
const ANTHROPIC_URL: &str = "https://api.anthropic.com/v1/models";
const OPENAI_URL: &str = "https://api.openai.com/v1/models";
const GEMINI_URL: &str = "https://generativelanguage.googleapis.com/v1beta/models";
const OLLAMA_CLOUD_URL: &str = "https://ollama.com/v1/models";
const DEEPSEEK_URL: &str = "https://api.deepseek.com/v1/models";
const THAILLM_URL: &str = "http://thaillm.or.th/api/v1/models";
const NVIDIA_URL: &str = "https://integrate.api.nvidia.com/v1/models";
const MINIMAX_URL: &str = "https://api.minimax.io/v1/models";
const OPENCODE_GO_URL: &str = "https://opencode.ai/zen/go/v1/models";
// Alibaba DashScope. The compatible-mode endpoint is OpenAI-
// shape so /v1/models returns {data:[{id, …}]}. We hit two
// regions: the China-mainland default (`dashscope.aliyuncs.com`)
// and the Singapore/intl region (`dashscope-intl.aliyuncs.com`,
// our QwenCloud variant). Both can be overridden via
// DASHSCOPE_BASE_URL / QWENCLOUD_BASE_URL but the script keeps
// hard-coded defaults so it works out of the box.
const DASHSCOPE_URL: &str = "https://dashscope.aliyuncs.com/compatible-mode/v1/models";
const QWENCLOUD_URL: &str = "https://dashscope-intl.aliyuncs.com/compatible-mode/v1/models";
const DEFAULT_TARGET: &str = "crates/core/resources/model_catalogue.json";

// ── Wire types ──────────────────────────────────────────────────────

#[derive(Debug, Deserialize)]
struct OpenRouterEnvelope {
    data: Vec<OpenRouterModel>,
}

#[derive(Debug, Deserialize)]
struct OpenRouterModel {
    id: String,
    #[serde(default)]
    context_length: Option<u32>,
    #[serde(default)]
    top_provider: Option<TopProvider>,
    /// OpenRouter publishes per-million-token rates as strings
    /// (e.g. `{"prompt":"0","completion":"0"}` for free models,
    /// `{"prompt":"0.000003","completion":"0.000015"}` for paid).
    /// Models with both fields at "0" are zero-cost — surfaced
    /// in the catalogue as `free: true`.
    #[serde(default)]
    pricing: Option<OpenRouterPricing>,
    /// Input/output modalities published by OpenRouter. We use
    /// `output_modalities` to drop rows that don't emit text
    /// (Lyria → audio, Imagen → image, etc.) from the chat picker.
    #[serde(default)]
    architecture: Option<OpenRouterArchitecture>,
}

#[derive(Debug, Deserialize)]
struct OpenRouterPricing {
    #[serde(default)]
    prompt: Option<String>,
    #[serde(default)]
    completion: Option<String>,
}

#[derive(Debug, Deserialize)]
struct OpenRouterArchitecture {
    /// Short modality string like `"text->text"`, `"text->audio"`,
    /// `"text+image->text"`. Older OpenRouter responses ship this
    /// without the structured `output_modalities` array, so we keep
    /// it as a fallback signal.
    #[serde(default)]
    modality: Option<String>,
    /// Newer field: explicit list like `["text"]` or `["audio"]`.
    #[serde(default)]
    output_modalities: Option<Vec<String>>,
}

impl OpenRouterModel {
    /// True when both prompt and completion rates parse to 0.0.
    /// Missing fields or unparseable values are treated as
    /// non-free (defensive — better to undercount than overcount).
    fn is_free(&self) -> bool {
        let p = self.pricing.as_ref();
        let zero = |s: &Option<String>| {
            s.as_deref()
                .and_then(|v| v.parse::<f64>().ok())
                .map(|v| v == 0.0)
                .unwrap_or(false)
        };
        p.map(|pr| zero(&pr.prompt) && zero(&pr.completion))
            .unwrap_or(false)
    }

    /// Returns `Some(true)` when this row is a pure text-out model
    /// (suitable for the chat agent), `Some(false)` when it emits any
    /// non-text modality (audio / image / video / embeddings — even
    /// alongside text, e.g. Lyria's `["text", "audio"]`), and `None`
    /// when OpenRouter doesn't publish modality data. `None` defaults
    /// to chat-capable at filter time — better to over-include legacy
    /// rows than silently hide them.
    fn is_chat(&self) -> Option<bool> {
        const NON_CHAT: &[&str] = &["audio", "image", "video", "embedding", "embeddings"];
        let arch = self.architecture.as_ref()?;
        let outputs: Vec<String> = if let Some(list) = &arch.output_modalities {
            if list.is_empty() {
                return None;
            }
            list.iter().map(|s| s.trim().to_ascii_lowercase()).collect()
        } else {
            let modality = arch.modality.as_deref()?;
            let output = modality.split("->").nth(1)?.trim();
            if output.is_empty() {
                return None;
            }
            output
                .split('+')
                .map(|s| s.trim().to_ascii_lowercase())
                .collect()
        };
        if outputs.iter().any(|m| NON_CHAT.contains(&m.as_str())) {
            return Some(false);
        }
        if outputs.iter().any(|m| m == "text") {
            return Some(true);
        }
        None
    }
}

#[derive(Debug, Deserialize)]
struct TopProvider {
    #[serde(default)]
    max_completion_tokens: Option<u32>,
}

#[derive(Debug, Deserialize)]
struct AnthropicEnvelope {
    data: Vec<AnthropicModel>,
}
#[derive(Debug, Deserialize)]
struct AnthropicModel {
    id: String,
}

#[derive(Debug, Deserialize)]
struct OpenAIEnvelope {
    data: Vec<OpenAIModel>,
}
#[derive(Debug, Deserialize)]
struct OpenAIModel {
    id: String,
}

#[derive(Debug, Deserialize)]
struct GeminiEnvelope {
    models: Vec<GeminiModel>,
}
#[derive(Debug, Deserialize)]
struct GeminiModel {
    name: String,
    #[serde(default, rename = "inputTokenLimit")]
    input_token_limit: Option<u32>,
    #[serde(default, rename = "outputTokenLimit")]
    output_token_limit: Option<u32>,
}

// ── Main ────────────────────────────────────────────────────────────

#[tokio::main(flavor = "current_thread")]
async fn main() -> ExitCode {
    match run().await {
        Ok(summary) => {
            println!("{summary}");
            ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("catalogue-seed: {e}");
            ExitCode::FAILURE
        }
    }
}

async fn run() -> Result<String, String> {
    // Pick up API keys from a workspace-root .env, regardless of where
    // `cargo run --bin catalogue-seed` was invoked from. Standard
    // load_dotenv handles ./.env and ~/.config/thclaws/.env; the
    // walking-up pass catches the workspace .env when this binary
    // is run from a nested crate dir (the typical case in the dev
    // workspace where the public-side root Cargo.toml doesn't exist).
    thclaws_core::dotenv::load_dotenv();
    if let Ok(cwd) = std::env::current_dir() {
        thclaws_core::dotenv::load_dotenv_walking_up(&cwd);
    }

    let target: PathBuf = std::env::args()
        .nth(1)
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            if Path::new(DEFAULT_TARGET).exists() {
                DEFAULT_TARGET.into()
            } else {
                "resources/model_catalogue.json".into()
            }
        });

    let existing =
        std::fs::read_to_string(&target).map_err(|e| format!("read {}: {e}", target.display()))?;
    let mut cat: Catalogue =
        serde_json::from_str(&existing).map_err(|e| format!("parse {}: {e}", target.display()))?;
    if cat.schema != CURRENT_SCHEMA {
        return Err(format!(
            "target has schema {}, expected {CURRENT_SCHEMA}",
            cat.schema
        ));
    }

    let today = today_iso();
    let mut report = Vec::new();

    // 1. OpenRouter — public, always runs. Also gives us context data
    //    we can reuse when we later discover bare Anthropic/OpenAI ids
    //    (which OpenRouter proxies as `anthropic/<id>` / `openai/<id>`).
    let openrouter_rows = match fetch_openrouter().await {
        Ok(rows) => rows,
        Err(e) => {
            report.push(format!("  openrouter: FAILED ({e})"));
            Vec::new()
        }
    };
    let openrouter_ctx_by_bare: HashMap<String, u32> = openrouter_rows
        .iter()
        .filter_map(|m| {
            let ctx = m.context_length?;
            let bare = m.id.rsplit('/').next().unwrap_or(&m.id).to_string();
            Some((bare, ctx))
        })
        .collect();
    // Mirror map for `max_completion_tokens` so providers whose own
    // `/v1/models` endpoint doesn't return per-model output caps (OpenAI,
    // Anthropic, DashScope, …) can borrow OpenRouter's well-curated
    // values. Without this, every downstream provider row ships
    // `max_output: None`, the agent's cap-against-max-output logic
    // becomes a no-op, and `max_tokens: 32000` blows up for gpt-4o
    // (cap 16384) and similar.
    let openrouter_max_output_by_bare: HashMap<String, u32> = openrouter_rows
        .iter()
        .filter_map(|m| {
            let max = m.top_provider.as_ref()?.max_completion_tokens?;
            let bare = m.id.rsplit('/').next().unwrap_or(&m.id).to_string();
            Some((bare, max))
        })
        .collect();
    let added_or = merge_openrouter(&mut cat, openrouter_rows, &today);
    push_provider_stats(&mut report, "openrouter", &added_or, None);

    // 2. Anthropic / OpenAI — need API key, gives us canonical dated
    //    ids. Context is not returned, so we pair each id with whatever
    //    OpenRouter reported for the matching `anthropic/<id>` or
    //    `openai/<id>` row; fall back to the provider's default_context.
    if let Ok(key) = std::env::var("ANTHROPIC_API_KEY") {
        match fetch_anthropic(&key).await {
            Ok(ids) => {
                let added = merge_discovered(
                    &mut cat,
                    "anthropic",
                    ANTHROPIC_URL,
                    ids,
                    &openrouter_ctx_by_bare,
                    &openrouter_max_output_by_bare,
                    &today,
                );
                push_provider_stats(&mut report, "anthropic", &added, None);
            }
            Err(e) => report.push(format!("  anthropic:   FAILED ({e})")),
        }
    } else {
        report.push("  anthropic:   skipped (no ANTHROPIC_API_KEY)".into());
    }

    if let Ok(key) = std::env::var("OPENAI_API_KEY") {
        match fetch_openai(&key).await {
            Ok(ids) => {
                let (kept, dropped): (Vec<_>, Vec<_>) =
                    ids.into_iter().partition(|id| is_openai_chat(id));
                let added = merge_discovered(
                    &mut cat,
                    "openai",
                    OPENAI_URL,
                    kept,
                    &openrouter_ctx_by_bare,
                    &openrouter_max_output_by_bare,
                    &today,
                );
                let suffix = format!(
                    "({} filtered: fine-tunes/audio/image/embedding)",
                    dropped.len()
                );
                push_provider_stats(&mut report, "openai", &added, Some(&suffix));
            }
            Err(e) => report.push(format!("  openai:      FAILED ({e})")),
        }
    } else {
        report.push("  openai:      skipped (no OPENAI_API_KEY)".into());
    }

    // 3. Gemini — gives us context directly in the list response.
    if let Ok(key) = std::env::var("GEMINI_API_KEY") {
        match fetch_gemini(&key).await {
            Ok(rows) => {
                let before = rows.len();
                let rows: Vec<_> = rows
                    .into_iter()
                    .filter(|m| {
                        let id = m.name.strip_prefix("models/").unwrap_or(&m.name);
                        is_gemini_chat(id)
                    })
                    .collect();
                let filtered = before - rows.len();
                let added = merge_gemini(&mut cat, rows, &today);
                let suffix = format!("({filtered} filtered: imagen/veo/gemma/embedding/tts)");
                push_provider_stats(&mut report, "gemini", &added, Some(&suffix));
            }
            Err(e) => report.push(format!("  gemini:      FAILED ({e})")),
        }
    } else {
        report.push("  gemini:      skipped (no GEMINI_API_KEY)".into());
    }

    // 4. Ollama Cloud — OpenAI-compatible /v1/models lists the cloud
    //    catalog (NOT the user's local Ollama; that one needs a local
    //    daemon to be running). Each id needs the `ollama-cloud/` prefix
    //    before merging because that's how thClaws routes cloud models
    //    distinctly from local Ollama (which uses `ollama/` prefix).
    if let Ok(key) = std::env::var("OLLAMA_CLOUD_API_KEY") {
        match fetch_ollama_cloud(&key).await {
            Ok(ids) => {
                let prefixed: Vec<String> = ids
                    .into_iter()
                    .map(|id| format!("ollama-cloud/{id}"))
                    .collect();
                // Seed the provider entry with a sensible default context
                // so merge_discovered doesn't skip rows for "no context".
                // 262144 covers most current cloud models; specific rows
                // can be hand-bumped later (e.g. deepseek-v4-flash at 1M).
                let pc = cat.providers.entry("ollama-cloud".into()).or_default();
                if pc.default_context.is_none() {
                    pc.default_context = Some(262144);
                }
                let added = merge_discovered(
                    &mut cat,
                    "ollama-cloud",
                    OLLAMA_CLOUD_URL,
                    prefixed,
                    &openrouter_ctx_by_bare,
                    &openrouter_max_output_by_bare,
                    &today,
                );
                push_provider_stats(&mut report, "ollama-cloud", &added, None);
            }
            Err(e) => report.push(format!("  ollama-cloud: FAILED ({e})")),
        }
    } else {
        report.push("  ollama-cloud: skipped (no OLLAMA_CLOUD_API_KEY)".into());
    }

    // 4b. DeepSeek — OpenAI-compatible `/v1/models` lists their V4 line
    //     (deepseek-v4-flash, deepseek-v4-pro). Bare model ids — no
    //     prefix-namespacing on our side, since `deepseek-` is enough
    //     for ProviderKind::detect. Default context seeded conservatively
    //     at 128K (V4 line ships with a longer window but specific rows
    //     can be hand-bumped after operator review of the diff).
    if let Ok(key) = std::env::var("DEEPSEEK_API_KEY") {
        match fetch_deepseek(&key).await {
            Ok(ids) => {
                let pc = cat.providers.entry("deepseek".into()).or_default();
                if pc.default_context.is_none() {
                    pc.default_context = Some(131072);
                }
                let added = merge_discovered(
                    &mut cat,
                    "deepseek",
                    DEEPSEEK_URL,
                    ids,
                    &openrouter_ctx_by_bare,
                    &openrouter_max_output_by_bare,
                    &today,
                );
                push_provider_stats(&mut report, "deepseek", &added, None);
            }
            Err(e) => report.push(format!("  deepseek:    FAILED ({e})")),
        }
    } else {
        report.push("  deepseek:    skipped (no DEEPSEEK_API_KEY)".into());
    }

    // 4c. ThaiLLM — NSTDA / สวทช aggregator at thaillm.or.th. OpenAI-
    //     compatible /v1/models lists OpenThaiGPT, Typhoon-S, Pathumma,
    //     THaLLE — all 8B Thai-tuned models on Llama-3.1-8B / Qwen3-8B
    //     bases (native 128K context). Each id is namespaced with the
    //     `thaillm/` prefix so ProviderKind::detect routes correctly,
    //     mirroring the ollama-cloud pattern.
    if let Ok(key) = std::env::var("THAILLM_API_KEY") {
        match fetch_thaillm(&key).await {
            Ok(ids) => {
                let prefixed: Vec<String> =
                    ids.into_iter().map(|id| format!("thaillm/{id}")).collect();
                let pc = cat.providers.entry("thaillm".into()).or_default();
                if pc.default_context.is_none() {
                    pc.default_context = Some(131072);
                }
                let added = merge_discovered(
                    &mut cat,
                    "thaillm",
                    THAILLM_URL,
                    prefixed,
                    &openrouter_ctx_by_bare,
                    &openrouter_max_output_by_bare,
                    &today,
                );
                push_provider_stats(&mut report, "thaillm", &added, None);
            }
            Err(e) => report.push(format!("  thaillm:     FAILED ({e})")),
        }
    } else {
        report.push("  thaillm:     skipped (no THAILLM_API_KEY)".into());
    }

    // 4d. NVIDIA NIM — OpenAI-compatible `/v1/models` at
    //     integrate.api.nvidia.com. The endpoint returns ids from many
    //     vendor namespaces (`nvidia/…`, `meta/…`, `google/…`,
    //     `mistralai/…`, etc.). We prepend a uniform `nvidia/` routing
    //     prefix so a single `from_model_id` rule auto-routes everything
    //     served by NIM — same pattern Ollama Cloud uses for its mixed
    //     vendor catalog. The `nvidia/` prefix is stripped in
    //     `build_provider` before the request hits the upstream, which
    //     means NVIDIA-owned models stored as `nvidia/nvidia/<name>`
    //     restore the original `nvidia/<name>` id on the wire. Context
    //     comes from OpenRouter's mirror where available; falls back to
    //     provider default (131072).
    if let Ok(key) = std::env::var("NVIDIA_API_KEY") {
        match fetch_nvidia(&key).await {
            Ok(ids) => {
                let prefixed: Vec<String> =
                    ids.into_iter().map(|id| format!("nvidia/{id}")).collect();
                let pc = cat.providers.entry("nvidia".into()).or_default();
                if pc.default_context.is_none() {
                    pc.default_context = Some(131072);
                }
                let added = merge_discovered(
                    &mut cat,
                    "nvidia",
                    NVIDIA_URL,
                    prefixed,
                    &openrouter_ctx_by_bare,
                    &openrouter_max_output_by_bare,
                    &today,
                );
                push_provider_stats(&mut report, "nvidia", &added, None);
            }
            Err(e) => report.push(format!("  nvidia:      FAILED ({e})")),
        }
    } else {
        report.push("  nvidia:      skipped (no NVIDIA_API_KEY)".into());
    }

    // 4e. MiniMax — international api.minimax.io OpenAI-compat
    //     `/v1/models`. Returns bare model ids (e.g. `MiniMax-M2`,
    //     `MiniMax-M1`). Each id is namespaced with the `minimax/`
    //     prefix so ProviderKind::detect routes correctly. Default
    //     context seeded at 200K (M2's published window); specific
    //     rows can be hand-bumped. China-platform users on
    //     api.minimax.chat (different auth) need a separate run with
    //     MINIMAX_BASE_URL pointed there.
    if let Ok(key) = std::env::var("MINIMAX_API_KEY") {
        match fetch_minimax(&key).await {
            Ok(ids) => {
                let prefixed: Vec<String> =
                    ids.into_iter().map(|id| format!("minimax/{id}")).collect();
                let pc = cat.providers.entry("minimax".into()).or_default();
                if pc.default_context.is_none() {
                    pc.default_context = Some(200000);
                }
                let added = merge_discovered(
                    &mut cat,
                    "minimax",
                    MINIMAX_URL,
                    prefixed,
                    &openrouter_ctx_by_bare,
                    &openrouter_max_output_by_bare,
                    &today,
                );
                push_provider_stats(&mut report, "minimax", &added, None);
            }
            Err(e) => report.push(format!("  minimax:     FAILED ({e})")),
        }
    } else {
        report.push("  minimax:     skipped (no MINIMAX_API_KEY)".into());
    }

    // 4e2. OpenCodeGo (opencode.ai). Subscription gateway serving
    //      GLM / Kimi / DeepSeek / MiMo (OpenAI-compat path),
    //      MiniMax M2.5 / M2.7 (Anthropic-compat path), and
    //      Qwen3.5 / 3.6-plus (Alibaba-compat path) through one
    //      base URL — `/v1/models` returns the full catalog as an
    //      OpenAI envelope. Bare ids — the `opencode-go/` prefix
    //      is added by the seed so ProviderKind::detect routes
    //      correctly. Default context conservative at 128K; the
    //      catalog mixes 200K (Kimi/MiniMax) and 1M (Qwen) windows
    //      so per-row hand-bumps cover the long-context variants.
    //      OPENCODE_GO_BASE_URL overrides for self-hosted proxies.
    let opencode_go_url = std::env::var("OPENCODE_GO_BASE_URL")
        .map(|b| format!("{}/models", b.trim_end_matches('/')))
        .unwrap_or_else(|_| OPENCODE_GO_URL.to_string());
    if let Ok(key) = std::env::var("OPENCODE_GO_API_KEY") {
        match fetch_opencode_go(&opencode_go_url, &key).await {
            Ok(ids) => {
                let prefixed: Vec<String> = ids
                    .into_iter()
                    .map(|id| format!("opencode-go/{id}"))
                    .collect();
                let pc = cat.providers.entry("opencode-go".into()).or_default();
                if pc.default_context.is_none() {
                    pc.default_context = Some(131072);
                }
                let added = merge_discovered(
                    &mut cat,
                    "opencode-go",
                    &opencode_go_url,
                    prefixed,
                    &openrouter_ctx_by_bare,
                    &openrouter_max_output_by_bare,
                    &today,
                );
                push_provider_stats(&mut report, "opencode-go", &added, None);
            }
            Err(e) => report.push(format!("  opencode-go: FAILED ({e})")),
        }
    } else {
        report.push("  opencode-go: skipped (no OPENCODE_GO_API_KEY)".into());
    }

    // 4f. DashScope (Alibaba, mainland-China region). OpenAI-compat
    //     `/v1/models` at dashscope.aliyuncs.com lists the qwen-*
    //     family (qwen-max, qwen-plus, qwen3-coder-plus, etc.).
    //     Bare ids — the `dashscope/` prefix is added by the seed
    //     so ProviderKind::detect routes correctly. Default
    //     context conservative at 32K; specific rows hand-bumped
    //     for the long-context variants (qwen3-coder is 1M).
    let dashscope_url = std::env::var("DASHSCOPE_BASE_URL")
        .map(|b| format!("{}/models", b.trim_end_matches('/')))
        .unwrap_or_else(|_| DASHSCOPE_URL.to_string());
    if let Ok(key) = std::env::var("DASHSCOPE_API_KEY") {
        match fetch_dashscope(&dashscope_url, &key).await {
            Ok(ids) => {
                let prefixed: Vec<String> = ids
                    .into_iter()
                    .map(|id| format!("dashscope/{id}"))
                    .collect();
                let pc = cat.providers.entry("dashscope".into()).or_default();
                if pc.default_context.is_none() {
                    pc.default_context = Some(32768);
                }
                let added = merge_discovered(
                    &mut cat,
                    "dashscope",
                    &dashscope_url,
                    prefixed,
                    &openrouter_ctx_by_bare,
                    &openrouter_max_output_by_bare,
                    &today,
                );
                push_provider_stats(&mut report, "dashscope", &added, None);
            }
            Err(e) => report.push(format!("  dashscope:   FAILED ({e})")),
        }
    } else {
        report.push("  dashscope:   skipped (no DASHSCOPE_API_KEY)".into());
    }

    // 4g. QwenCloud — Alibaba's Singapore/intl region of DashScope
    //     at dashscope-intl.aliyuncs.com. Same wire shape as
    //     mainland DashScope but a different account, key, and
    //     model availability set (intl region typically lags
    //     mainland by a release or two). The `qc/` prefix
    //     namespace separates QwenCloud rows from DashScope so
    //     a single workspace can have both keys configured and
    //     ProviderKind::detect picks the right one. The `qc/`
    //     prefix is stripped before the request hits the
    //     upstream (which expects bare `qwen-max`, etc.).
    let qwencloud_url = std::env::var("QWENCLOUD_BASE_URL")
        .map(|b| format!("{}/models", b.trim_end_matches('/')))
        .unwrap_or_else(|_| QWENCLOUD_URL.to_string());
    if let Ok(key) = std::env::var("QWENCLOUD_API_KEY") {
        match fetch_dashscope(&qwencloud_url, &key).await {
            Ok(ids) => {
                let prefixed: Vec<String> = ids.into_iter().map(|id| format!("qc/{id}")).collect();
                let pc = cat.providers.entry("qwen-cloud".into()).or_default();
                if pc.default_context.is_none() {
                    pc.default_context = Some(32768);
                }
                let added = merge_discovered(
                    &mut cat,
                    "qwen-cloud",
                    &qwencloud_url,
                    prefixed,
                    &openrouter_ctx_by_bare,
                    &openrouter_max_output_by_bare,
                    &today,
                );
                push_provider_stats(&mut report, "qwen-cloud", &added, None);
            }
            Err(e) => report.push(format!("  qwen-cloud:  FAILED ({e})")),
        }
    } else {
        report.push("  qwen-cloud:  skipped (no QWENCLOUD_API_KEY)".into());
    }

    // 5. Derive agent-sdk rows from anthropic. The Claude CLI subprocess
    //    (ProviderKind::AgentSdk) accepts any claude-* model id; thClaws
    //    routes it as `agent/<id>`. So for every claude-* row in the
    //    anthropic catalogue, mirror an `agent/<id>` row into agent-sdk
    //    with the same context. Skips ids already present so hand-curated
    //    overrides win on metadata. Closes the lag pattern from
    //    thclaws/thclaws#26 — Anthropic ships a new variant, native picks
    //    it up via /v1/models, this step propagates it to agent-sdk.
    let claude_ids: Vec<(String, Option<u32>)> = cat
        .providers
        .get("anthropic")
        .map(|p| {
            p.models
                .iter()
                .map(|(id, e)| (id.clone(), e.context))
                .collect()
        })
        .unwrap_or_default();
    if !claude_ids.is_empty() {
        let agent_pc = cat.providers.entry("anthropic-agent".into()).or_default();
        if agent_pc.default_context.is_none() {
            agent_pc.default_context = Some(200000);
        }
        let mut stats = MergeStats::default();
        for (claude_id, ctx) in claude_ids {
            let agent_id = format!("agent/{claude_id}");
            if agent_pc.models.contains_key(&agent_id) {
                stats.unchanged += 1;
                continue;
            }
            agent_pc.models.insert(
                agent_id.clone(),
                ModelEntry {
                    context: ctx,
                    max_output: None,
                    source: Some(format!("derived:{claude_id}")),
                    verified_at: Some(today.clone()),
                    free: None,
                    chat: None,
                    ..Default::default()
                },
            );
            stats.added.push(agent_id);
        }
        push_provider_stats(
            &mut report,
            "anthropic-agent",
            &stats,
            Some("(derived from anthropic)"),
        );
    }

    cat.source = format!("baseline {today}");
    cat.fetched_at = format!("{today}T00:00:00Z");

    let out = serde_json::to_string_pretty(&cat).map_err(|e| format!("serialize: {e}"))?;
    std::fs::write(&target, out).map_err(|e| format!("write {}: {e}", target.display()))?;

    let total: usize = cat.providers.values().map(|p| p.models.len()).sum();
    Ok(format!(
        "wrote {} ({total} total rows):\n{}",
        target.display(),
        report.join("\n")
    ))
}

// ── Merge helpers ───────────────────────────────────────────────────

/// Per-provider seed result. Captures everything the operator might want
/// to see in the report: which ids were inserted, which were already
/// present (so they don't wonder "did the seed see it?"), and which the
/// seed had to drop for lack of usable metadata.
#[derive(Default)]
pub struct MergeStats {
    pub added: Vec<String>,
    pub unchanged: usize,
    pub skipped_no_context: usize,
    /// Rows whose guessed context was replaced by a real one this run.
    /// Reported because it silently changes a model's usable window.
    pub context_corrected: Vec<String>,
}

/// Format the per-provider report lines. Header always shows added +
/// unchanged counts; appends "skipped (no context)" only when nonzero so
/// the common case stays terse. Each new id is listed on its own bullet
/// (capped at MAX_LIST_IDS to keep an unusually large refresh from
/// dumping hundreds of lines). `suffix` carries provider-specific extras
/// (e.g. OpenAI's "X filtered: fine-tunes/audio/image/embedding").
const MAX_LIST_IDS: usize = 30;

/// Does this row's context come from the blanket provider default rather
/// than a published number? Delegates so the marker string has one
/// definition — the pickers read the same signal to render `200k?`.
fn is_unverified_context(e: &ModelEntry) -> bool {
    e.context_unverified()
}

fn push_provider_stats(
    report: &mut Vec<String>,
    provider: &str,
    stats: &MergeStats,
    suffix: Option<&str>,
) {
    let count = stats.added.len();
    let label = format!("{provider}:");
    let mut header = format!("  {label:12} +{count} new, {} unchanged", stats.unchanged);
    if stats.skipped_no_context > 0 {
        header.push_str(&format!(
            ", {} skipped (no context)",
            stats.skipped_no_context
        ));
    }
    if !stats.context_corrected.is_empty() {
        header.push_str(&format!(
            ", {} context corrected",
            stats.context_corrected.len()
        ));
    }
    if let Some(s) = suffix {
        header.push(' ');
        header.push_str(s);
    }
    report.push(header);
    if count == 0 {
        return;
    }
    let mut sorted = stats.added.clone();
    sorted.sort();
    let shown = sorted.iter().take(MAX_LIST_IDS);
    for id in shown {
        report.push(format!("                 · {id}"));
    }
    if count > MAX_LIST_IDS {
        report.push(format!(
            "                 … (+{} more)",
            count - MAX_LIST_IDS
        ));
    }
}

fn merge_openrouter(cat: &mut Catalogue, rows: Vec<OpenRouterModel>, today: &str) -> MergeStats {
    let pc = cat
        .providers
        .entry("openrouter".into())
        .or_insert_with(|| ProviderCatalogue {
            list_url: Some(OPENROUTER_URL.into()),
            default_context: Some(128_000),
            models: BTreeMap::new(),
        });
    let mut stats = MergeStats::default();
    for m in rows {
        let Some(ctx) = m.context_length else {
            stats.skipped_no_context += 1;
            continue;
        };
        let is_free = m.is_free();
        let chat = m.is_chat();
        let max_output = m
            .top_provider
            .as_ref()
            .and_then(|p| p.max_completion_tokens);
        // Even when the entry already exists, refresh the `free`
        // and `chat` flags — OpenRouter occasionally flips models
        // (free ↔ paid; preview modality reclassified) and we want
        // the Settings filters to reflect current upstream state
        // without forcing operators to delete-and-reseed.
        if let Some(existing) = pc.models.get_mut(&m.id) {
            if existing.free != Some(is_free) {
                existing.free = Some(is_free);
            }
            if chat.is_some() && existing.chat != chat {
                existing.chat = chat;
            }
            stats.unchanged += 1;
            continue;
        }
        pc.models.insert(
            m.id.clone(),
            ModelEntry {
                context: Some(ctx),
                max_output,
                source: Some(OPENROUTER_URL.into()),
                verified_at: Some(today.into()),
                free: Some(is_free),
                chat,
                ..Default::default()
            },
        );
        stats.added.push(m.id);
    }
    stats
}

/// Ids came from the provider's `/v1/models` (so they're real). Context
/// is not in that response, so we look up each id's bare form in the
/// `openrouter_ctx_by_bare` map (OpenRouter usually proxies the same
/// model and publishes its context). When OpenRouter doesn't know
/// either, we still insert the id with the provider's default context
/// so the user can at least pick it — the `source` flag says it's
/// unverified context.
fn merge_discovered(
    cat: &mut Catalogue,
    provider: &str,
    list_url: &str,
    ids: Vec<String>,
    openrouter_ctx_by_bare: &HashMap<String, u32>,
    openrouter_max_output_by_bare: &HashMap<String, u32>,
    today: &str,
) -> MergeStats {
    let pc = cat.providers.entry(provider.into()).or_default();
    if pc.list_url.is_none() {
        pc.list_url = Some(list_url.into());
    }
    let default_ctx = pc.default_context;
    let mut stats = MergeStats::default();
    for id in ids {
        let mirrored_max_output = openrouter_max_output_by_bare.get(&id).copied();
        if let Some(existing) = pc.models.get_mut(&id) {
            // Backfill max_output on rows that pre-date the cap-tracking
            // changes (existing seed runs left it None for OpenAI etc.).
            // Treat OpenRouter's mirror value as authoritative — that's
            // where every other path resolves the cap from.
            if existing.max_output.is_none() {
                if let Some(max) = mirrored_max_output {
                    existing.max_output = Some(max);
                }
            }
            // Upgrade a GUESSED context once a real one shows up. A row
            // seeded while OpenRouter didn't know the model got the
            // provider's blanket `default_context` and an "(context
            // unverified)" stamp — and, before this, kept it forever, because
            // an existing row was never re-examined. That froze
            // opencode-go/deepseek-v4-* at 131072 for a 1M model (#190): the
            // same id reads 1048576 on every provider that publishes its own
            // number, so users were being compacted 8× too early.
            //
            // Only rows still carrying the unverified stamp are touched — a
            // context the provider itself reported stays authoritative.
            if is_unverified_context(existing) {
                if let Some(real) = openrouter_ctx_by_bare.get(&id).copied() {
                    if existing.context != Some(real) {
                        existing.context = Some(real);
                        existing.source = Some(format!("{OPENROUTER_URL} via bare id"));
                        existing.verified_at = Some(today.into());
                        stats.context_corrected.push(id.clone());
                    }
                }
            }
            stats.unchanged += 1;
            continue;
        }
        let (ctx, source) = match openrouter_ctx_by_bare.get(&id).copied() {
            Some(n) => (n, format!("{OPENROUTER_URL} via bare id")),
            None => match default_ctx {
                Some(n) => (n, format!("{list_url} {CONTEXT_UNVERIFIED_MARK}")),
                None => {
                    stats.skipped_no_context += 1;
                    continue;
                }
            },
        };
        pc.models.insert(
            id.clone(),
            ModelEntry {
                context: Some(ctx),
                max_output: mirrored_max_output,
                source: Some(source),
                verified_at: Some(today.into()),
                free: None,
                chat: None,
                ..Default::default()
            },
        );
        stats.added.push(id);
    }
    stats
}

fn merge_gemini(cat: &mut Catalogue, rows: Vec<GeminiModel>, today: &str) -> MergeStats {
    let pc = cat
        .providers
        .entry("gemini".into())
        .or_insert_with(|| ProviderCatalogue {
            list_url: Some(GEMINI_URL.into()),
            default_context: Some(1_000_000),
            models: BTreeMap::new(),
        });
    let mut stats = MergeStats::default();
    for m in rows {
        // Gemini returns ids like `models/gemini-1.5-pro` — strip the
        // leading `models/` to match the rest of the codebase.
        let id = m
            .name
            .strip_prefix("models/")
            .unwrap_or(&m.name)
            .to_string();
        let Some(ctx) = m.input_token_limit else {
            stats.skipped_no_context += 1;
            continue;
        };
        if pc.models.contains_key(&id) {
            stats.unchanged += 1;
            continue;
        }
        pc.models.insert(
            id.clone(),
            ModelEntry {
                context: Some(ctx),
                max_output: m.output_token_limit,
                source: Some(GEMINI_URL.into()),
                verified_at: Some(today.into()),
                free: None,
                chat: None,
                ..Default::default()
            },
        );
        stats.added.push(id);
    }
    stats
}

// ── HTTP ────────────────────────────────────────────────────────────

fn client() -> Result<reqwest::Client, String> {
    reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(30))
        .build()
        .map_err(|e| format!("http client: {e}"))
}

async fn fetch_openrouter() -> Result<Vec<OpenRouterModel>, String> {
    let resp = client()?
        .get(OPENROUTER_URL)
        .send()
        .await
        .map_err(|e| format!("GET {OPENROUTER_URL}: {e}"))?;
    if !resp.status().is_success() {
        return Err(format!("openrouter HTTP {}", resp.status()));
    }
    let env: OpenRouterEnvelope = resp.json().await.map_err(|e| format!("json: {e}"))?;
    Ok(env.data)
}

async fn fetch_anthropic(key: &str) -> Result<Vec<String>, String> {
    let resp = client()?
        .get(ANTHROPIC_URL)
        .header("x-api-key", key)
        .header("anthropic-version", "2023-06-01")
        .send()
        .await
        .map_err(|e| format!("GET {ANTHROPIC_URL}: {e}"))?;
    if !resp.status().is_success() {
        return Err(format!("anthropic HTTP {}", resp.status()));
    }
    let env: AnthropicEnvelope = resp.json().await.map_err(|e| format!("json: {e}"))?;
    Ok(env.data.into_iter().map(|m| m.id).collect())
}

async fn fetch_openai(key: &str) -> Result<Vec<String>, String> {
    let resp = client()?
        .get(OPENAI_URL)
        .bearer_auth(key)
        .send()
        .await
        .map_err(|e| format!("GET {OPENAI_URL}: {e}"))?;
    if !resp.status().is_success() {
        return Err(format!("openai HTTP {}", resp.status()));
    }
    let env: OpenAIEnvelope = resp.json().await.map_err(|e| format!("json: {e}"))?;
    Ok(env.data.into_iter().map(|m| m.id).collect())
}

/// Fetch DeepSeek's `/v1/models` (OpenAI-compatible). At the time of
/// writing this returns just the V4 line (`deepseek-v4-flash`,
/// `deepseek-v4-pro`); older aliases like `deepseek-chat` and
/// `deepseek-reasoner` still work on the chat completions endpoint as
/// wire-level aliases but aren't listed by `/v1/models`, so they don't
/// land in the catalogue automatically. Operators can hand-add them.
async fn fetch_deepseek(key: &str) -> Result<Vec<String>, String> {
    let resp = client()?
        .get(DEEPSEEK_URL)
        .bearer_auth(key)
        .send()
        .await
        .map_err(|e| format!("GET {DEEPSEEK_URL}: {e}"))?;
    if !resp.status().is_success() {
        return Err(format!("deepseek HTTP {}", resp.status()));
    }
    let env: OpenAIEnvelope = resp.json().await.map_err(|e| format!("json: {e}"))?;
    Ok(env.data.into_iter().map(|m| m.id).collect())
}

/// Fetch the model list from Alibaba DashScope's OpenAI-compatible
/// endpoint. Both `dashscope` (mainland) and `qwen-cloud`
/// (Singapore / intl) speak the same wire shape — the same
/// helper handles both, parameterised on URL + key. `base_url`
/// is the override env var if set; otherwise the hard-coded
/// default. Returns bare model ids (`qwen-max`, `qwen-plus`,
/// `qwen3-coder-plus`, …); caller adds the provider-specific
/// `dashscope/` or `qc/` prefix.
async fn fetch_dashscope(url: &str, key: &str) -> Result<Vec<String>, String> {
    let resp = client()?
        .get(url)
        .bearer_auth(key)
        .send()
        .await
        .map_err(|e| format!("GET {url}: {e}"))?;
    if !resp.status().is_success() {
        return Err(format!("dashscope HTTP {}", resp.status()));
    }
    let env: OpenAIEnvelope = resp.json().await.map_err(|e| format!("json: {e}"))?;
    // Some DashScope responses include non-chat models (text-
    // embedding-*, qwen-vl-*, reranker, etc.). Catalogue is for
    // chat; filter to the `qwen-*` family minus embeddings /
    // vision-only / audio variants. Power users can hand-edit
    // `model_catalogue.json` if they want a specialised model.
    Ok(env
        .data
        .into_iter()
        .map(|m| m.id)
        .filter(|id| {
            // Chat-only roster. DashScope's /v1/models surfaces a
            // lot of specialised endpoints — image generation
            // (`qwen-image-*`), machine translation
            // (`qwen-mt-*`), embeddings, rerankers, ASR, TTS,
            // audio understanding. None of them speak the chat
            // protocol we route through. Filter aggressively to
            // keep the catalogue useful in the model picker.
            id.starts_with("qwen")
                && !id.contains("embedding")
                && !id.contains("rerank")
                && !id.contains("-audio")
                && !id.contains("-tts")
                && !id.contains("-asr")
                && !id.contains("-image")
                && !id.contains("-mt-")
        })
        .collect())
}

/// Fetch the model list from NSTDA's Thai LLM aggregator. The endpoint
/// is OpenAI-compatible — `/v1/models` returns `{data:[{id, object,
/// owned_by}]}` for each Thai model hosted (OpenThaiGPT, Typhoon-S,
/// Pathumma, THaLLE, etc.). Returns bare ids; caller adds the
/// `thaillm/` prefix to namespace them.
async fn fetch_thaillm(key: &str) -> Result<Vec<String>, String> {
    let resp = client()?
        .get(THAILLM_URL)
        .bearer_auth(key)
        .send()
        .await
        .map_err(|e| format!("GET {THAILLM_URL}: {e}"))?;
    if !resp.status().is_success() {
        return Err(format!("thaillm HTTP {}", resp.status()));
    }
    let env: OpenAIEnvelope = resp.json().await.map_err(|e| format!("json: {e}"))?;
    Ok(env.data.into_iter().map(|m| m.id).collect())
}

/// Fetch the NVIDIA NIM model list. The endpoint is OpenAI-compatible —
/// `/v1/models` returns `{data:[{id,...}]}`. Model IDs already include
/// the `nvidia/` owner prefix (e.g. `nvidia/nemotron-3-super-120b-a12b`),
/// so no additional prefix is applied by the caller.
async fn fetch_nvidia(key: &str) -> Result<Vec<String>, String> {
    let resp = client()?
        .get(NVIDIA_URL)
        .bearer_auth(key)
        .send()
        .await
        .map_err(|e| format!("GET {NVIDIA_URL}: {e}"))?;
    if !resp.status().is_success() {
        return Err(format!("nvidia HTTP {}", resp.status()));
    }
    let env: OpenAIEnvelope = resp.json().await.map_err(|e| format!("json: {e}"))?;
    Ok(env.data.into_iter().map(|m| m.id).collect())
}

/// Fetch the MiniMax model list. The international endpoint at
/// api.minimax.io advertises an OpenAI-compatible /v1/models route
/// but actually responds with `{"object":"","data":null}` (no model
/// enumeration exposed). We tolerate that by treating null/empty as
/// "no rows discovered" — hand-curated catalogue entries (M2 / M1 /
/// abab7-chat-preview) carry the metadata. If MiniMax later starts
/// returning real rows the same call will pick them up.
async fn fetch_minimax(key: &str) -> Result<Vec<String>, String> {
    #[derive(serde::Deserialize)]
    struct MinimaxEnvelope {
        #[serde(default)]
        data: Option<Vec<OpenAIModel>>,
    }
    let resp = client()?
        .get(MINIMAX_URL)
        .bearer_auth(key)
        .send()
        .await
        .map_err(|e| format!("GET {MINIMAX_URL}: {e}"))?;
    if !resp.status().is_success() {
        return Err(format!("minimax HTTP {}", resp.status()));
    }
    let env: MinimaxEnvelope = resp.json().await.map_err(|e| format!("json: {e}"))?;
    Ok(env
        .data
        .unwrap_or_default()
        .into_iter()
        .map(|m| m.id)
        .collect())
}

/// OpenCodeGo (opencode.ai) — same OpenAI-compat envelope shape as
/// Minimax / Ollama Cloud. Takes a `url` parameter so the seeder can
/// honor `OPENCODE_GO_BASE_URL` for staging / self-hosted proxies
/// without forking the function. Returns bare ids — caller adds the
/// `opencode-go/` prefix at merge time. Some upstream responses already
/// include the prefix on the id (`opencode-go/kimi-k2.6`); strip it
/// here so the merge layer's prefix-prepend doesn't produce
/// `opencode-go/opencode-go/...`.
async fn fetch_opencode_go(url: &str, key: &str) -> Result<Vec<String>, String> {
    let resp = client()?
        .get(url)
        .bearer_auth(key)
        .send()
        .await
        .map_err(|e| format!("GET {url}: {e}"))?;
    if !resp.status().is_success() {
        return Err(format!("opencode-go HTTP {}", resp.status()));
    }
    let env: OpenAIEnvelope = resp.json().await.map_err(|e| format!("json: {e}"))?;
    Ok(env
        .data
        .into_iter()
        .map(|m| {
            m.id.strip_prefix("opencode-go/")
                .map(str::to_string)
                .unwrap_or(m.id)
        })
        .collect())
}

/// Fetch the cloud catalog from Ollama Cloud's OpenAI-compatible
/// `/v1/models` endpoint. Returns bare model ids (e.g. `kimi-k2.5`,
/// `gpt-oss:120b`) — caller adds the `ollama-cloud/` prefix to namespace
/// them in the catalogue. The same key works against `/api/tags` for
/// richer metadata (size, modified_at) but we don't currently consume
/// those fields, and the OpenAI-compatible shape lets us reuse
/// OpenAIEnvelope without a new struct.
async fn fetch_ollama_cloud(key: &str) -> Result<Vec<String>, String> {
    let resp = client()?
        .get(OLLAMA_CLOUD_URL)
        .bearer_auth(key)
        .send()
        .await
        .map_err(|e| format!("GET {OLLAMA_CLOUD_URL}: {e}"))?;
    if !resp.status().is_success() {
        return Err(format!("ollama-cloud HTTP {}", resp.status()));
    }
    let env: OpenAIEnvelope = resp.json().await.map_err(|e| format!("json: {e}"))?;
    Ok(env.data.into_iter().map(|m| m.id).collect())
}

async fn fetch_gemini(key: &str) -> Result<Vec<GeminiModel>, String> {
    let url = format!("{GEMINI_URL}?key={key}");
    let resp = client()?
        .get(&url)
        .send()
        .await
        .map_err(|e| format!("GET gemini: {e}"))?;
    if !resp.status().is_success() {
        return Err(format!("gemini HTTP {}", resp.status()));
    }
    let env: GeminiEnvelope = resp.json().await.map_err(|e| format!("json: {e}"))?;
    Ok(env.models)
}

// ── Filters ─────────────────────────────────────────────────────────
//
// Provider `/v1/models` endpoints dump everything they serve: image
// gen, audio, embeddings, fine-tunes. For a chat/reasoning catalogue
// we only want text-in/text-out models. Filters are conservative —
// prefix allowlist + substring denylist — and easy to audit.

fn is_openai_chat(id: &str) -> bool {
    // User-specific fine-tunes look like `ft:base:org::suffix` —
    // never belong in a shipped baseline.
    if id.starts_with("ft:") {
        return false;
    }
    // Allowlist: only keep ids from known chat / reasoning families.
    let ok_prefix = ["gpt-", "o1", "o3", "o4", "o5", "chatgpt-"]
        .iter()
        .any(|p| id.starts_with(p));
    if !ok_prefix {
        return false;
    }
    // Denylist: modality-specific variants within those families.
    let skip = [
        "image",
        "-transcribe",
        "-realtime",
        "-audio",
        "-tts",
        "-search-preview",
    ];
    !skip.iter().any(|s| id.contains(s))
}

fn is_gemini_chat(id: &str) -> bool {
    // Google's catalogue includes imagen/veo/lyria/gemma/robotics/
    // embeddings/TTS alongside chat. Allow only `gemini-*`, then
    // deny modality-specific members of that family.
    if !id.starts_with("gemini-") {
        return false;
    }
    let skip = [
        "embedding",
        "-tts",
        "robotics",
        "-image",
        "-audio",
        "computer-use",
    ];
    if skip.iter().any(|s| id.contains(s)) {
        return false;
    }
    // Drop Gemini IDs Google has deprecated or shut down. Google
    // sometimes keeps them in the public list for "existing customer"
    // backward compat even after new-customer 404s start, which leads
    // to misleading entries in our catalogue (issue #32: user calls
    // gemini-2.0-flash → 404). Source of truth:
    //
    //     https://ai.google.dev/gemini-api/docs/deprecations
    //
    // Update this list when Google retires more IDs. Track the next
    // upcoming shutdown via the official deprecations page; the 2.5
    // family is on the clock for 2026-06-17.
    if is_retired_gemini(id) {
        return false;
    }
    true
}

/// Hard-list of Gemini model IDs we refuse to import even when the
/// upstream `/v1beta/models` endpoint still returns them. Sources:
/// <https://ai.google.dev/gemini-api/docs/deprecations>.
fn is_retired_gemini(id: &str) -> bool {
    // 1.x family — fully shut down (2025).
    if id.starts_with("gemini-1.") || id == "gemini-pro" || id == "gemini-pro-vision" {
        return true;
    }
    // 2.0 family — existing-customer-only since 2026-03-06; hard
    // shutdown 2026-06-01. Already 404s for new API keys, which is
    // exactly issue #32's symptom.
    if id.starts_with("gemini-2.0-flash") {
        return true;
    }
    // 3-pro-preview — already shut down 2026-03-09 (replaced by
    // gemini-3.1-pro-preview).
    if id == "gemini-3-pro-preview" {
        return true;
    }
    false
}

// ── Date stamp ──────────────────────────────────────────────────────

fn today_iso() -> String {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let days = secs / 86_400;
    let (y, m, d) = civil_from_days(days as i64);
    format!("{y:04}-{m:02}-{d:02}")
}

fn civil_from_days(z: i64) -> (i32, u32, u32) {
    let z = z + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as u64;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = (if mp < 10 { mp + 3 } else { mp - 9 }) as u32;
    let y = if m <= 2 { y + 1 } else { y } as i32;
    (y, m, d)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn civil_from_days_known_dates() {
        assert_eq!(civil_from_days(20_567), (2026, 4, 24));
        assert_eq!(civil_from_days(0), (1970, 1, 1));
    }

    #[test]
    fn guessed_context_is_upgraded_once_a_real_one_exists() {
        // #190: a row seeded while OpenRouter didn't know the model got the
        // provider's blanket default plus an "(context unverified)" stamp,
        // and — before this — kept it forever, because an existing row was
        // never re-examined. opencode-go/deepseek-v4-* sat at 131072 for a
        // 1M model as a result.
        let mut cat = Catalogue {
            schema: CURRENT_SCHEMA,
            source: String::new(),
            fetched_at: String::new(),
            providers: BTreeMap::new(),
            aliases: Default::default(),
            fallback: Default::default(),
        };
        let pc = cat
            .providers
            .entry("opencode-go".into())
            .or_insert_with(ProviderCatalogue::default);
        pc.default_context = Some(131_072);
        pc.models.insert(
            "deepseek-v4-flash".into(),
            ModelEntry {
                context: Some(131_072),
                source: Some(format!("{OPENCODE_GO_URL} {CONTEXT_UNVERIFIED_MARK}")),
                verified_at: Some("2026-06-22".into()),
                ..Default::default()
            },
        );
        // A row whose context the provider itself published must NOT move,
        // even when OpenRouter disagrees — its number is authoritative.
        pc.models.insert(
            "capped-model".into(),
            ModelEntry {
                context: Some(32_768),
                source: Some("https://opencode.ai/zen/go/v1/models".into()),
                verified_at: Some("2026-06-22".into()),
                ..Default::default()
            },
        );

        let ctx_by_bare = HashMap::from([
            ("deepseek-v4-flash".to_string(), 1_048_576u32),
            ("capped-model".to_string(), 1_048_576u32),
        ]);
        let stats = merge_discovered(
            &mut cat,
            "opencode-go",
            "https://opencode.ai/zen/go/v1/models",
            vec!["deepseek-v4-flash".into(), "capped-model".into()],
            &ctx_by_bare,
            &HashMap::new(),
            "2026-08-09",
        );

        let models = &cat.providers["opencode-go"].models;
        assert_eq!(models["deepseek-v4-flash"].context, Some(1_048_576));
        assert_eq!(
            models["deepseek-v4-flash"].verified_at.as_deref(),
            Some("2026-08-09"),
            "an upgraded row is re-dated"
        );
        assert!(
            !is_unverified_context(&models["deepseek-v4-flash"]),
            "the unverified stamp is cleared once a real number lands"
        );
        assert_eq!(
            models["capped-model"].context,
            Some(32_768),
            "a provider-published context is never overwritten"
        );
        assert_eq!(
            stats.context_corrected,
            vec!["deepseek-v4-flash".to_string()]
        );
    }

    #[test]
    fn openai_filter_keeps_chat_drops_noise() {
        // Keep chat / reasoning.
        assert!(is_openai_chat("gpt-4o"));
        assert!(is_openai_chat("gpt-4o-mini"));
        assert!(is_openai_chat("gpt-4.1-2025-04-14"));
        assert!(is_openai_chat("o3"));
        assert!(is_openai_chat("o3-mini"));
        assert!(is_openai_chat("o4-mini"));
        assert!(is_openai_chat("chatgpt-4o-latest"));
        // Drop user fine-tunes and non-chat families.
        assert!(!is_openai_chat("ft:gpt-3.5-turbo-0613:org::abc"));
        assert!(!is_openai_chat("dall-e-3"));
        assert!(!is_openai_chat("davinci-002"));
        assert!(!is_openai_chat("babbage-002"));
        assert!(!is_openai_chat("whisper-1"));
        assert!(!is_openai_chat("tts-1"));
        assert!(!is_openai_chat("text-embedding-3-small"));
        assert!(!is_openai_chat("computer-use-preview"));
        // Drop audio / image / realtime variants of chat families.
        assert!(!is_openai_chat("gpt-image-1"));
        assert!(!is_openai_chat("chatgpt-image-latest"));
        assert!(!is_openai_chat("gpt-4o-audio-preview"));
        assert!(!is_openai_chat("gpt-4o-realtime-preview"));
        assert!(!is_openai_chat("gpt-4o-transcribe"));
        assert!(!is_openai_chat("gpt-4o-search-preview"));
        assert!(!is_openai_chat("gpt-4o-mini-tts"));
    }

    #[test]
    fn gemini_filter_keeps_chat_drops_noise() {
        // Currently-shipping chat IDs.
        assert!(is_gemini_chat("gemini-2.5-pro"));
        assert!(is_gemini_chat("gemini-2.5-flash"));
        assert!(is_gemini_chat("gemini-3.1-pro-preview"));
        assert!(is_gemini_chat("gemini-3-flash-preview"));
        assert!(is_gemini_chat("gemini-flash-latest"));
        // Non-gemini families dropped outright.
        assert!(!is_gemini_chat("imagen-4.0-generate-001"));
        assert!(!is_gemini_chat("veo-3.0-generate-001"));
        assert!(!is_gemini_chat("lyria-3-pro-preview"));
        assert!(!is_gemini_chat("gemma-3-27b-it"));
        assert!(!is_gemini_chat("aqa"));
        assert!(!is_gemini_chat("nano-banana-pro-preview"));
        // Gemini-prefixed but modality-specific → dropped.
        assert!(!is_gemini_chat("gemini-embedding-001"));
        assert!(!is_gemini_chat("gemini-2.5-flash-image"));
        assert!(!is_gemini_chat("gemini-3-pro-image-preview"));
        assert!(!is_gemini_chat("gemini-2.5-flash-preview-tts"));
        assert!(!is_gemini_chat("gemini-robotics-er-1.5-preview"));
        assert!(!is_gemini_chat("gemini-2.5-computer-use-preview-10-2025"));
    }

    #[test]
    fn gemini_filter_drops_retired_models() {
        // Issue #32: keep retired Gemini IDs out of the catalogue so
        // `make catalogue` runs against a still-listing upstream don't
        // re-add them. Source: ai.google.dev/gemini-api/docs/deprecations.
        // 1.x family — fully shut down 2025.
        assert!(is_retired_gemini("gemini-1.5-flash"));
        assert!(is_retired_gemini("gemini-1.5-pro"));
        assert!(is_retired_gemini("gemini-1.0-pro"));
        assert!(is_retired_gemini("gemini-pro"));
        assert!(is_retired_gemini("gemini-pro-vision"));
        // 2.0 family — existing-customer-only 2026-03-06; full shutdown
        // 2026-06-01. Issue #32's specific symptom (404 for new keys).
        assert!(is_retired_gemini("gemini-2.0-flash"));
        assert!(is_retired_gemini("gemini-2.0-flash-001"));
        assert!(is_retired_gemini("gemini-2.0-flash-lite"));
        assert!(is_retired_gemini("gemini-2.0-flash-lite-001"));
        // 3-pro-preview — already shut down 2026-03-09.
        assert!(is_retired_gemini("gemini-3-pro-preview"));
        // Currently-shipping IDs must not match the retirement filter.
        assert!(!is_retired_gemini("gemini-2.5-flash"));
        assert!(!is_retired_gemini("gemini-2.5-pro"));
        assert!(!is_retired_gemini("gemini-3.1-pro-preview"));
        assert!(!is_retired_gemini("gemini-3-flash-preview"));
        assert!(!is_retired_gemini("gemini-flash-latest"));
        // is_gemini_chat composes both filters — retired IDs drop out.
        assert!(!is_gemini_chat("gemini-2.0-flash"));
        assert!(!is_gemini_chat("gemini-3-pro-preview"));
        assert!(!is_gemini_chat("gemini-1.5-flash"));
    }
}
