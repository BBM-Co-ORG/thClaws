//! Usage tracking — accumulate token usage per provider+model per project.
//!
//! Stored in `.thclaws/state/usage/{provider}/{model}.json` with daily
//! breakdowns. Each file is a JSON object keyed by date (YYYY-MM-DD) with
//! token counts.
//!
//! Example: `.thclaws/state/usage/anthropic/claude-sonnet-4-5.json`
//! ```json
//! {
//!   "2026-04-13": { "input": 15230, "output": 3420, "cache_write": 500, "cache_read": 12000, "requests": 8 },
//!   "2026-04-12": { "input": 8100, "output": 1200, "cache_write": 0, "cache_read": 4000, "requests": 3 }
//! }
//! ```

use crate::providers::Usage;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::PathBuf;

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct DailyUsage {
    pub input: u64,
    pub output: u64,
    #[serde(default)]
    pub cache_write: u64,
    #[serde(default)]
    pub cache_read: u64,
    #[serde(default)]
    pub requests: u64,
}

/// Per-model usage file: date → DailyUsage.
pub type ModelUsage = BTreeMap<String, DailyUsage>;

pub struct UsageTracker {
    root: PathBuf,
}

impl UsageTracker {
    pub fn new(root: PathBuf) -> Self {
        Self { root }
    }

    /// Default: `.thclaws/state/usage/` in cwd.
    pub fn default_path() -> PathBuf {
        PathBuf::from(".thclaws/state/usage")
    }

    fn model_path(&self, provider: &str, model: &str) -> PathBuf {
        // Sanitize model name for filesystem (e.g. "claude-sonnet-4-5" is fine,
        // but "ollama/llama3.2" needs the slash replaced).
        let safe_model = model.replace('/', "_");
        self.root.join(provider).join(format!("{safe_model}.json"))
    }

    /// Record a usage event for a provider+model.
    pub fn record(&self, provider: &str, model: &str, usage: &Usage) {
        if usage.input_tokens == 0 && usage.output_tokens == 0 {
            return;
        }
        let path = self.model_path(provider, model);
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }

        let today = today_str();

        // M3: serialize the whole read→modify→write across threads AND
        // processes. The per-model file is a read-modify-write: two
        // concurrent writers would both read the pre-update contents and
        // last-writer-wins, silently dropping one subagent's tokens
        // (Task is `parallelizable` and several thClaws instances may
        // share one `.thclaws/usage/`). An advisory exclusive flock on a
        // dedicated sidecar `.lock` file closes the lost-update window;
        // writing via temp-file + atomic rename closes the torn-read
        // window for the lock-free readers (`all_models`/`total`/`today`).
        // The lock file inode is stable (never renamed), so all writers
        // contend on the same inode. Best-effort throughout: a failed
        // open/lock skips this record rather than panicking — usage
        // accounting must never take down a turn.
        use fs2::FileExt;
        let lock_path = path.with_extension("json.lock");
        let Ok(lock_file) = std::fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(&lock_path)
        else {
            return;
        };
        if lock_file.lock_exclusive().is_err() {
            return;
        }

        // Read existing data (under lock).
        let mut data: ModelUsage = if path.exists() {
            std::fs::read_to_string(&path)
                .ok()
                .and_then(|s| serde_json::from_str(&s).ok())
                .unwrap_or_default()
        } else {
            BTreeMap::new()
        };

        // Accumulate into today's entry.
        let entry = data.entry(today).or_default();
        entry.input += usage.input_tokens as u64;
        entry.output += usage.output_tokens as u64;
        entry.cache_write += usage.cache_creation_input_tokens.unwrap_or(0) as u64;
        entry.cache_read += usage.cache_read_input_tokens.unwrap_or(0) as u64;
        entry.requests += 1;

        // Write back via temp-file + atomic rename so a concurrent reader
        // never observes a half-written file.
        if let Ok(json) = serde_json::to_string_pretty(&data) {
            let tmp = path.with_extension("json.tmp");
            if std::fs::write(&tmp, &json).is_ok() {
                let _ = std::fs::rename(&tmp, &path);
            }
        }

        // Best-effort unlock; Drop closes the fd which also releases the
        // flock per POSIX semantics, so a failed unlock can't deadlock
        // the next writer.
        let _ = lock_file.unlock();
    }

    /// Get total usage across all providers and models.
    pub fn total(&self) -> DailyUsage {
        let mut total = DailyUsage::default();
        for (_, _, data) in self.all_models() {
            for day in data.values() {
                total.input += day.input;
                total.output += day.output;
                total.cache_write += day.cache_write;
                total.cache_read += day.cache_read;
                total.requests += day.requests;
            }
        }
        total
    }

    /// Get today's usage across all providers and models.
    pub fn today(&self) -> DailyUsage {
        let today = today_str();
        let mut total = DailyUsage::default();
        for (_, _, data) in self.all_models() {
            if let Some(day) = data.get(&today) {
                total.input += day.input;
                total.output += day.output;
                total.cache_write += day.cache_write;
                total.cache_read += day.cache_read;
                total.requests += day.requests;
            }
        }
        total
    }

    /// List all (provider, model, data) tuples.
    pub fn all_models(&self) -> Vec<(String, String, ModelUsage)> {
        let mut out = Vec::new();
        let Ok(providers) = std::fs::read_dir(&self.root) else {
            return out;
        };
        for prov_entry in providers.flatten() {
            if !prov_entry.file_type().map(|t| t.is_dir()).unwrap_or(false) {
                continue;
            }
            let provider = prov_entry.file_name().to_string_lossy().into_owned();
            let Ok(models) = std::fs::read_dir(prov_entry.path()) else {
                continue;
            };
            for model_entry in models.flatten() {
                let path = model_entry.path();
                if path.extension().and_then(|e| e.to_str()) != Some("json") {
                    continue;
                }
                let model = path
                    .file_stem()
                    .and_then(|s| s.to_str())
                    .unwrap_or("unknown")
                    .replace('_', "/");
                if let Ok(contents) = std::fs::read_to_string(&path) {
                    if let Ok(data) = serde_json::from_str::<ModelUsage>(&contents) {
                        out.push((provider.clone(), model, data));
                    }
                }
            }
        }
        out.sort_by(|a, b| a.0.cmp(&b.0).then(a.1.cmp(&b.1)));
        out
    }

    /// Format a summary for display.
    pub fn summary(&self) -> String {
        let mut parts = Vec::new();
        let today_key = today_str();

        let models = self.all_models();
        if models.is_empty() {
            return "No usage recorded.".into();
        }

        parts.push("## Usage by model".to_string());
        for (provider, model, data) in &models {
            let mut total = DailyUsage::default();
            for day in data.values() {
                total.input += day.input;
                total.output += day.output;
                total.cache_write += day.cache_write;
                total.cache_read += day.cache_read;
                total.requests += day.requests;
            }
            let today = data.get(&today_key);
            let today_str = today
                .map(|d| format!("today: {}in/{}out ({} req)", d.input, d.output, d.requests))
                .unwrap_or_else(|| "today: 0".into());
            parts.push(format!(
                "  {}/{}: {}in/{}out ({} req) — {}",
                provider, model, total.input, total.output, total.requests, today_str
            ));
        }

        let grand = self.total();
        let grand_today = self.today();
        parts.push(format!(
            "\n## Total: {}in/{}out ({} req) — today: {}in/{}out ({} req)",
            grand.input,
            grand.output,
            grand.requests,
            grand_today.input,
            grand_today.output,
            grand_today.requests,
        ));

        parts.join("\n")
    }
}

pub(crate) fn today_str() -> String {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    // Simple date calculation (no chrono dependency).
    let days = secs / 86400;
    let (y, m, d) = days_to_ymd(days);
    format!("{y:04}-{m:02}-{d:02}")
}

fn epoch_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Best-effort per-workspace usage ledger.
///
/// Appends one JSON line per *finalized agent turn* — the main loop OR a
/// Task subagent — to `<cwd>/.thclaws/state/usage.jsonl`. A reader can sum the
/// lines to get the true token + USD cost of a whole task **including
/// subagent work**, which the per-model [`UsageTracker`] aggregates by
/// day (hard to isolate one run) and the session JSONL omits entirely.
///
/// `role` is `"main"` for the orchestrator or the subagent's name. The
/// cost is computed from the model catalogue (`null` for unknown / tier-
/// billed models). Never returns an error: usage telemetry must not be
/// able to break a turn.
pub fn append_usage_ledger(
    cwd: &std::path::Path,
    role: &str,
    provider: &str,
    model: &str,
    usage: &Usage,
) {
    if usage.input_tokens == 0 && usage.output_tokens == 0 {
        return;
    }
    let dir = cwd.join(".thclaws").join("state");
    if std::fs::create_dir_all(&dir).is_err() {
        return;
    }
    let cost = crate::model_catalogue::EffectiveCatalogue::load().compute_cost_usd(
        model,
        &crate::model_catalogue::TokenUsage {
            prompt_tokens: usage.input_tokens,
            completion_tokens: usage.output_tokens,
            cached_input_tokens: usage.cache_read_input_tokens.unwrap_or(0),
            cache_creation_tokens: usage.cache_creation_input_tokens.unwrap_or(0),
            reasoning_tokens: usage.reasoning_output_tokens.unwrap_or(0),
        },
    );
    let line = serde_json::json!({
        "type": "usage",
        "ts": epoch_secs(),
        "role": role,
        "provider": provider,
        "model": model,
        "input": usage.input_tokens,
        "output": usage.output_tokens,
        "cache_read": usage.cache_read_input_tokens.unwrap_or(0),
        "cache_write": usage.cache_creation_input_tokens.unwrap_or(0),
        "reasoning": usage.reasoning_output_tokens.unwrap_or(0),
        "cost_usd": cost,
    })
    .to_string();
    use std::io::Write;
    if let Ok(mut f) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(dir.join("usage.jsonl"))
    {
        let _ = writeln!(f, "{line}");
    }
}

fn days_to_ymd(days: u64) -> (u64, u64, u64) {
    // Algorithm from http://howardhinnant.github.io/date_algorithms.html
    let z = days + 719468;
    let era = z / 146097;
    let doe = z - era * 146097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    (y, m, d)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn record_and_read() {
        let dir = tempdir().unwrap();
        let tracker = UsageTracker::new(dir.path().to_path_buf());

        let usage = Usage {
            input_tokens: 1000,
            output_tokens: 200,
            cache_creation_input_tokens: Some(50),
            cache_read_input_tokens: Some(300),
            reasoning_output_tokens: None,
        };
        tracker.record("anthropic", "claude-sonnet-4-5", &usage);
        tracker.record("anthropic", "claude-sonnet-4-5", &usage);

        let today = tracker.today();
        assert_eq!(today.input, 2000);
        assert_eq!(today.output, 400);
        assert_eq!(today.cache_write, 100);
        assert_eq!(today.cache_read, 600);
        assert_eq!(today.requests, 2);
    }

    #[test]
    fn ledger_appends_parseable_lines_with_roles() {
        let dir = tempdir().unwrap();
        let usage = Usage {
            input_tokens: 1000,
            output_tokens: 200,
            cache_creation_input_tokens: Some(50),
            cache_read_input_tokens: Some(300),
            reasoning_output_tokens: None,
        };
        // One main turn + two subagent turns into the same workspace.
        append_usage_ledger(dir.path(), "main", "anthropic", "claude-sonnet-4-6", &usage);
        append_usage_ledger(
            dir.path(),
            "planner",
            "anthropic",
            "claude-sonnet-4-6",
            &usage,
        );
        append_usage_ledger(
            dir.path(),
            "verifier",
            "anthropic",
            "claude-sonnet-4-6",
            &usage,
        );

        let body = std::fs::read_to_string(dir.path().join(".thclaws/state/usage.jsonl")).unwrap();
        let lines: Vec<_> = body.lines().collect();
        assert_eq!(lines.len(), 3, "one ledger line per finalized turn");

        let mut total_in = 0u64;
        let mut roles = Vec::new();
        for l in &lines {
            let v: serde_json::Value = serde_json::from_str(l).unwrap();
            assert_eq!(v["type"], "usage");
            total_in += v["input"].as_u64().unwrap();
            roles.push(v["role"].as_str().unwrap().to_string());
        }
        // Summing the ledger captures parent + subagent token usage.
        assert_eq!(total_in, 3000);
        assert!(roles.contains(&"main".to_string()));
        assert!(roles.contains(&"planner".to_string()));
        assert!(roles.contains(&"verifier".to_string()));
    }

    #[test]
    fn ledger_skips_zero_usage() {
        let dir = tempdir().unwrap();
        append_usage_ledger(dir.path(), "main", "anthropic", "m", &Usage::default());
        assert!(!dir.path().join(".thclaws/state/usage.jsonl").exists());
    }

    #[test]
    fn multiple_models() {
        let dir = tempdir().unwrap();
        let tracker = UsageTracker::new(dir.path().to_path_buf());

        tracker.record(
            "anthropic",
            "claude-sonnet-4-5",
            &Usage {
                input_tokens: 100,
                output_tokens: 50,
                cache_creation_input_tokens: None,
                cache_read_input_tokens: None,
                reasoning_output_tokens: None,
            },
        );
        tracker.record(
            "openai",
            "gpt-4o",
            &Usage {
                input_tokens: 200,
                output_tokens: 100,
                cache_creation_input_tokens: None,
                cache_read_input_tokens: None,
                reasoning_output_tokens: None,
            },
        );

        let models = tracker.all_models();
        assert_eq!(models.len(), 2);

        let total = tracker.total();
        assert_eq!(total.input, 300);
        assert_eq!(total.output, 150);
        assert_eq!(total.requests, 2);
    }

    #[test]
    fn zero_usage_not_recorded() {
        let dir = tempdir().unwrap();
        let tracker = UsageTracker::new(dir.path().to_path_buf());

        tracker.record("anthropic", "claude-sonnet-4-5", &Usage::default());
        assert!(tracker.all_models().is_empty());
    }

    #[test]
    fn summary_format() {
        let dir = tempdir().unwrap();
        let tracker = UsageTracker::new(dir.path().to_path_buf());

        tracker.record(
            "anthropic",
            "claude-sonnet-4-5",
            &Usage {
                input_tokens: 1000,
                output_tokens: 200,
                cache_creation_input_tokens: None,
                cache_read_input_tokens: None,
                reasoning_output_tokens: None,
            },
        );

        let summary = tracker.summary();
        assert!(summary.contains("anthropic/claude-sonnet-4-5"));
        assert!(summary.contains("1000in/200out"));
    }

    #[test]
    fn today_str_valid_format() {
        let d = today_str();
        assert_eq!(d.len(), 10); // YYYY-MM-DD
        assert_eq!(&d[4..5], "-");
        assert_eq!(&d[7..8], "-");
    }
}
