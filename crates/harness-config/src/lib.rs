//! Config resolution: which DeepSeek endpoint, which tier, and where the key
//! comes from.
//!
//! Resolution order (highest wins): CLI flag > `config.toml` >
//! `$DEEPSEEK_API_KEY` environment variable. This intentionally mirrors
//! grok-build's own precedence (`SamplerConfig` construction in
//! `xai-grok-sampler::config`).
//!
//! Scoped to DeepSeek only for now (Flash + Pro tiers), per the current
//! product decision — but `Endpoint` is kept separate from tier/pricing data
//! so a second provider is an additive module later, not a rewrite.

use std::fmt;
use std::path::Path;

use serde::Deserialize;
use thiserror::Error;

/// The two DeepSeek tiers this harness ships. `Flash` is the default for
/// every turn; `Pro` is used for explicit escalation (harder tasks, or
/// automatic escalation after repeated failure — see `harness-agent`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Tier {
    Flash,
    Pro,
}

impl fmt::Display for Tier {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Tier::Flash => "flash",
            Tier::Pro => "pro",
        })
    }
}

impl std::str::FromStr for Tier {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.to_ascii_lowercase().as_str() {
            "flash" => Ok(Tier::Flash),
            "pro" => Ok(Tier::Pro),
            other => Err(format!(
                "unknown tier {other:?} (expected \"flash\" or \"pro\")"
            )),
        }
    }
}

/// $/M-token pricing for one tier, used only for the live cost readout —
/// never sent to the API. Editable via `config.toml` since providers change
/// prices without notice.
#[derive(Debug, Clone, Copy, Deserialize)]
pub struct Pricing {
    pub input_cache_miss_per_m: f64,
    pub input_cache_hit_per_m: f64,
    pub output_per_m: f64,
}

/// Everything needed to sample one tier: wire model id, context window, and
/// pricing for the cost readout.
#[derive(Debug, Clone)]
pub struct ModelInfo {
    pub wire_id: String,
    pub context_window: u64,
    pub pricing: Pricing,
}

/// Resolved DeepSeek endpoint: where to send requests and with what key.
#[derive(Debug, Clone)]
pub struct Endpoint {
    pub base_url: String,
    pub api_key: String,
}

/// Agent-loop policy knobs, all with sane defaults.
#[derive(Debug, Clone, Copy)]
pub struct AgentPolicy {
    pub default_tier: Tier,
    pub max_turns: u32,
    /// Compact the conversation once usage crosses this percent of the
    /// active tier's context window.
    pub compaction_threshold_percent: u8,
    /// Escalate Flash → Pro for one retry after this many consecutive
    /// identical tool calls (a doom-loop symptom) or tool errors.
    pub auto_escalate: bool,
    pub escalate_after_repeats: u32,
}

impl Default for AgentPolicy {
    fn default() -> Self {
        Self {
            default_tier: Tier::Flash,
            max_turns: 25,
            compaction_threshold_percent: 75,
            auto_escalate: true,
            escalate_after_repeats: 2,
        }
    }
}

/// Fully resolved runtime configuration.
#[derive(Debug, Clone)]
pub struct Resolved {
    pub endpoint: Endpoint,
    pub flash: ModelInfo,
    pub pro: ModelInfo,
    pub policy: AgentPolicy,
}

impl Resolved {
    pub fn model_for(&self, tier: Tier) -> &ModelInfo {
        match tier {
            Tier::Flash => &self.flash,
            Tier::Pro => &self.pro,
        }
    }
}

// ---- on-disk config.toml shape (all fields optional) ----

#[derive(Debug, Default, Deserialize)]
struct File {
    #[serde(default)]
    deepseek: DeepSeekSection,
    #[serde(default)]
    agent: AgentSection,
}

#[derive(Debug, Default, Deserialize)]
struct DeepSeekSection {
    api_key: Option<String>,
    base_url: Option<String>,
    flash_model: Option<String>,
    pro_model: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
struct AgentSection {
    default_tier: Option<String>,
    max_turns: Option<u32>,
    compaction_threshold_percent: Option<u8>,
    auto_escalate: Option<bool>,
    escalate_after_repeats: Option<u32>,
}

#[derive(Debug, Error)]
pub enum ConfigError {
    #[error("reading {path}: {source}")]
    Read {
        path: String,
        #[source]
        source: std::io::Error,
    },
    #[error("parsing {path}: {source}")]
    Parse {
        path: String,
        #[source]
        source: toml::de::Error,
    },
    #[error(
        "no DeepSeek API key found — set $DEEPSEEK_API_KEY, pass --api-key, or add \
         `api_key` under [deepseek] in your config file"
    )]
    MissingKey,
    #[error("invalid tier: {0}")]
    InvalidTier(String),
}

/// Prices as of 2026-07 — approximate, and DeepSeek can change them without
/// notice. Override via `config.toml` if these drift.
fn default_flash_pricing() -> Pricing {
    Pricing {
        input_cache_miss_per_m: 0.14,
        input_cache_hit_per_m: 0.0028,
        output_per_m: 0.28,
    }
}
fn default_pro_pricing() -> Pricing {
    Pricing {
        input_cache_miss_per_m: 0.435,
        input_cache_hit_per_m: 0.003625,
        output_per_m: 0.87,
    }
}

/// Overrides collected from CLI flags — anything `Some` here wins over the
/// config file and environment.
#[derive(Debug, Default)]
pub struct CliOverrides {
    pub api_key: Option<String>,
    pub base_url: Option<String>,
    pub tier: Option<Tier>,
}

/// Load and fully resolve configuration.
///
/// `path` is the config file location; a missing file is not an error (all
/// sections are optional and fall back to defaults + environment).
pub fn resolve(path: &Path, cli: CliOverrides) -> Result<Resolved, ConfigError> {
    let file = load_file(path)?;

    let base_url = cli
        .base_url
        .or(file.deepseek.base_url)
        .unwrap_or_else(|| "https://api.deepseek.com".to_string());

    let api_key = cli
        .api_key
        .or(file.deepseek.api_key)
        .or_else(|| std::env::var("DEEPSEEK_API_KEY").ok())
        .filter(|k| !k.is_empty())
        .ok_or(ConfigError::MissingKey)?;

    let flash_wire_id = file
        .deepseek
        .flash_model
        .unwrap_or_else(|| "deepseek-v4-flash".to_string());
    let pro_wire_id = file
        .deepseek
        .pro_model
        .unwrap_or_else(|| "deepseek-v4-pro".to_string());

    let mut policy = AgentPolicy::default();
    if let Some(t) = &file.agent.default_tier {
        policy.default_tier = t.parse().map_err(ConfigError::InvalidTier)?;
    }
    if let Some(v) = file.agent.max_turns {
        policy.max_turns = v;
    }
    if let Some(v) = file.agent.compaction_threshold_percent {
        policy.compaction_threshold_percent = v;
    }
    if let Some(v) = file.agent.auto_escalate {
        policy.auto_escalate = v;
    }
    if let Some(v) = file.agent.escalate_after_repeats {
        policy.escalate_after_repeats = v;
    }
    if let Some(t) = cli.tier {
        policy.default_tier = t;
    }

    Ok(Resolved {
        endpoint: Endpoint { base_url, api_key },
        flash: ModelInfo {
            wire_id: flash_wire_id,
            context_window: 128_000,
            pricing: default_flash_pricing(),
        },
        pro: ModelInfo {
            wire_id: pro_wire_id,
            context_window: 128_000,
            pricing: default_pro_pricing(),
        },
        policy,
    })
}

fn load_file(path: &Path) -> Result<File, ConfigError> {
    if !path.exists() {
        return Ok(File::default());
    }
    let text = std::fs::read_to_string(path).map_err(|source| ConfigError::Read {
        path: path.display().to_string(),
        source,
    })?;
    toml::from_str(&text).map_err(|source| ConfigError::Parse {
        path: path.display().to_string(),
        source,
    })
}

/// Default config file location: `~/.config/harness/config.toml`.
pub fn default_config_path() -> std::path::PathBuf {
    if let Some(home) = dirs_home() {
        return home.join(".config").join("harness").join("config.toml");
    }
    std::path::PathBuf::from("harness.toml")
}

/// Minimal `$HOME` lookup so this crate doesn't need the `dirs` dependency
/// for one call site.
fn dirs_home() -> Option<std::path::PathBuf> {
    std::env::var_os("HOME").map(std::path::PathBuf::from)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tier_parses_case_insensitively() {
        assert_eq!("Flash".parse::<Tier>().unwrap(), Tier::Flash);
        assert_eq!("PRO".parse::<Tier>().unwrap(), Tier::Pro);
        assert!("nope".parse::<Tier>().is_err());
    }

    #[test]
    fn missing_key_is_an_error_when_env_unset() {
        // SAFETY: test-only env mutation, single-threaded within this test.
        unsafe { std::env::remove_var("DEEPSEEK_API_KEY") };
        let result = resolve(
            Path::new("/nonexistent/config.toml"),
            CliOverrides::default(),
        );
        assert!(matches!(result, Err(ConfigError::MissingKey)));
    }

    #[test]
    fn cli_key_wins_without_env() {
        unsafe { std::env::remove_var("DEEPSEEK_API_KEY") };
        let cli = CliOverrides {
            api_key: Some("sk-test".into()),
            ..Default::default()
        };
        let resolved = resolve(Path::new("/nonexistent/config.toml"), cli).unwrap();
        assert_eq!(resolved.endpoint.api_key, "sk-test");
        assert_eq!(resolved.flash.wire_id, "deepseek-v4-flash");
        assert_eq!(resolved.policy.default_tier, Tier::Flash);
    }
}
