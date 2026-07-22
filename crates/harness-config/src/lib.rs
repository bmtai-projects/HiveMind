//! Config resolution: which model endpoint, which tier, and where the key
//! comes from.
//!
//! Resolution order (highest wins): CLI flag > `config.toml` >
//! `$HIVEMIND_API_KEY` environment variable > stored hosted credentials
//! (`hivemind auth login`). This intentionally mirrors grok-build's own
//! precedence (`SamplerConfig` construction in `xai-grok-sampler::config`),
//! extended with the hosted fallback. `$DEEPSEEK_API_KEY` and the legacy
//! `[deepseek]` config section are still accepted silently, underneath
//! `$HIVEMIND_API_KEY`/`[model]`, so nothing set up before this rename
//! breaks.
//!
//! Scoped to one upstream model provider for now (Flash + Pro tiers), per
//! the current product decision — but `Endpoint` is kept separate from
//! tier/pricing data so a second provider is an additive module later, not
//! a rewrite.

use std::fmt;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use thiserror::Error;

/// The two model tiers this harness ships. `Flash` is the default for
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

/// Resolved model endpoint: where to send requests and with what key.
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
    model: ModelSection,
    // Legacy alias for `[model]`, from before the section was renamed.
    // Still parsed, silently, so a config.toml written before this rename
    // keeps working; never mentioned in anything user-facing going forward.
    #[serde(default)]
    deepseek: ModelSection,
    #[serde(default)]
    agent: AgentSection,
}

#[derive(Debug, Default, Deserialize)]
struct ModelSection {
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
        "no HiveMind API key found — run `hivemind auth login`, set $HIVEMIND_API_KEY, pass \
         --api-key, or add `api_key` under [model] in your config file"
    )]
    MissingKey,
    #[error("invalid tier: {0}")]
    InvalidTier(String),
    #[error("writing {path}: {source}")]
    Write {
        path: String,
        #[source]
        source: std::io::Error,
    },
    #[error("serializing credentials: {source}")]
    Serialize {
        #[source]
        source: toml::ser::Error,
    },
}

/// Prices as of 2026-07 — approximate, and the upstream provider can change
/// them without notice. Override via `config.toml` if these drift.
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

/// A token minted by the hosted backend (`hivemind auth login`), paired
/// with the API base it's valid against. Stored at
/// `default_credentials_path()`, never sent anywhere except that backend.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HostedCredentials {
    pub api_base: String,
    pub access_token: String,
}

#[derive(Serialize, Deserialize)]
struct CredentialsFile {
    hosted: HostedCredentials,
}

/// Load and fully resolve configuration.
///
/// `config_path` is the config file location; `credentials_path` is where
/// `hivemind auth login` would have stored hosted credentials. Neither
/// existing is an error — config sections are all optional, and hosted
/// credentials are only consulted as the last-resort key source.
pub fn resolve(
    config_path: &Path,
    credentials_path: &Path,
    cli: CliOverrides,
) -> Result<Resolved, ConfigError> {
    let file = load_file(config_path)?;

    let explicit_key = cli
        .api_key
        .clone()
        .or_else(|| file.model.api_key.clone())
        .or_else(|| file.deepseek.api_key.clone())
        .or_else(|| std::env::var("HIVEMIND_API_KEY").ok())
        .or_else(|| std::env::var("DEEPSEEK_API_KEY").ok())
        .filter(|k| !k.is_empty());
    let base_url_override = cli
        .base_url
        .clone()
        .or_else(|| file.model.base_url.clone())
        .or_else(|| file.deepseek.base_url.clone());

    // A hosted token is only ever paired with its own api_base — never the
    // bare upstream default — unless an explicit override says otherwise
    // (e.g. pointing a hosted token at a local mock for testing).
    let (api_key, base_url) = match explicit_key {
        Some(key) => (
            key,
            base_url_override.unwrap_or_else(|| "https://api.deepseek.com".to_string()),
        ),
        None => {
            let creds = load_hosted_credentials(credentials_path).ok_or(ConfigError::MissingKey)?;
            (
                creds.access_token,
                base_url_override.unwrap_or(creds.api_base),
            )
        }
    };

    let flash_wire_id = file
        .model
        .flash_model
        .or(file.deepseek.flash_model)
        .unwrap_or_else(|| "deepseek-v4-flash".to_string());
    let pro_wire_id = file
        .model
        .pro_model
        .or(file.deepseek.pro_model)
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

/// Default config file location: `~/.config/hivemind/config.toml`.
pub fn default_config_path() -> std::path::PathBuf {
    if let Some(home) = dirs_home() {
        return home.join(".config").join("hivemind").join("config.toml");
    }
    std::path::PathBuf::from("hivemind.toml")
}

/// Default hosted-credentials location: `~/.config/hivemind/credentials.toml`.
pub fn default_credentials_path() -> PathBuf {
    if let Some(home) = dirs_home() {
        return home
            .join(".config")
            .join("hivemind")
            .join("credentials.toml");
    }
    PathBuf::from("hivemind-credentials.toml")
}

/// Returns `None` on any failure (missing file, bad TOML, ...) rather than
/// an error — the caller's next move either way is "fall back / tell the
/// user to run `hivemind auth login`", so a granular error isn't useful
/// here the way it is for `config.toml`.
pub fn load_hosted_credentials(path: &Path) -> Option<HostedCredentials> {
    let text = std::fs::read_to_string(path).ok()?;
    let file: CredentialsFile = toml::from_str(&text).ok()?;
    Some(file.hosted)
}

/// Writes `credentials.toml`, creating its parent directory if needed, and
/// restricting it to owner-read/write on Unix (mode `0600`) since it holds
/// a live bearer token. There's no equivalent restriction applied on
/// Windows — NTFS ACLs default to per-user already for files under the
/// user's own profile directory, which is where this lands.
pub fn save_hosted_credentials(path: &Path, creds: &HostedCredentials) -> Result<(), ConfigError> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|source| ConfigError::Write {
            path: parent.display().to_string(),
            source,
        })?;
    }
    let text = toml::to_string_pretty(&CredentialsFile {
        hosted: creds.clone(),
    })
    .map_err(|source| ConfigError::Serialize { source })?;
    std::fs::write(path, text).map_err(|source| ConfigError::Write {
        path: path.display().to_string(),
        source,
    })?;

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600)).map_err(
            |source| ConfigError::Write {
                path: path.display().to_string(),
                source,
            },
        )?;
    }
    Ok(())
}

/// Removes `credentials.toml` if present. Returns whether a file was
/// actually there to remove, so `hivemind auth logout` can say "signed
/// out" vs "wasn't signed in" without a separate existence check.
pub fn delete_hosted_credentials(path: &Path) -> std::io::Result<bool> {
    if path.exists() {
        std::fs::remove_file(path)?;
        Ok(true)
    } else {
        Ok(false)
    }
}

/// Minimal `$HOME` lookup so this crate doesn't need the `dirs` dependency
/// for one call site.
fn dirs_home() -> Option<std::path::PathBuf> {
    std::env::var_os("HOME").map(std::path::PathBuf::from)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    // `cargo test` runs tests in this file concurrently by default, but
    // HIVEMIND_API_KEY/DEEPSEEK_API_KEY are process-wide state -- two tests
    // mutating them at once produces exactly the kind of intermittent
    // failure that's easy to dismiss as flaky and hard to reproduce later.
    // Every test below that touches either var locks this first, for its
    // whole duration (guard held until scope end), so they're serialized
    // against each other without slowing down or affecting unrelated tests.
    static ENV_LOCK: Mutex<()> = Mutex::new(());

    #[test]
    fn tier_parses_case_insensitively() {
        assert_eq!("Flash".parse::<Tier>().unwrap(), Tier::Flash);
        assert_eq!("PRO".parse::<Tier>().unwrap(), Tier::Pro);
        assert!("nope".parse::<Tier>().is_err());
    }

    #[test]
    fn missing_key_is_an_error_when_env_and_credentials_unset() {
        let _guard = ENV_LOCK.lock().unwrap();
        // SAFETY: test-only env mutation, serialized via ENV_LOCK above.
        unsafe {
            std::env::remove_var("HIVEMIND_API_KEY");
            std::env::remove_var("DEEPSEEK_API_KEY");
        };
        let result = resolve(
            Path::new("/nonexistent/config.toml"),
            Path::new("/nonexistent/credentials.toml"),
            CliOverrides::default(),
        );
        assert!(matches!(result, Err(ConfigError::MissingKey)));
    }

    #[test]
    fn cli_key_wins_without_env() {
        let _guard = ENV_LOCK.lock().unwrap();
        unsafe {
            std::env::remove_var("HIVEMIND_API_KEY");
            std::env::remove_var("DEEPSEEK_API_KEY");
        };
        let cli = CliOverrides {
            api_key: Some("sk-test".into()),
            ..Default::default()
        };
        let resolved = resolve(
            Path::new("/nonexistent/config.toml"),
            Path::new("/nonexistent/credentials.toml"),
            cli,
        )
        .unwrap();
        assert_eq!(resolved.endpoint.api_key, "sk-test");
        assert_eq!(resolved.flash.wire_id, "deepseek-v4-flash");
        assert_eq!(resolved.policy.default_tier, Tier::Flash);
    }

    #[test]
    fn hivemind_api_key_wins_over_legacy_deepseek_api_key() {
        let _guard = ENV_LOCK.lock().unwrap();
        unsafe {
            std::env::set_var("HIVEMIND_API_KEY", "hm-key");
            std::env::set_var("DEEPSEEK_API_KEY", "ds-key");
        };
        let resolved = resolve(
            Path::new("/nonexistent/config.toml"),
            Path::new("/nonexistent/credentials.toml"),
            CliOverrides::default(),
        )
        .unwrap();
        assert_eq!(resolved.endpoint.api_key, "hm-key");
        unsafe {
            std::env::remove_var("HIVEMIND_API_KEY");
            std::env::remove_var("DEEPSEEK_API_KEY");
        };
    }

    #[test]
    fn legacy_deepseek_api_key_still_works_alone() {
        let _guard = ENV_LOCK.lock().unwrap();
        unsafe {
            std::env::remove_var("HIVEMIND_API_KEY");
            std::env::set_var("DEEPSEEK_API_KEY", "ds-key");
        };
        let resolved = resolve(
            Path::new("/nonexistent/config.toml"),
            Path::new("/nonexistent/credentials.toml"),
            CliOverrides::default(),
        )
        .unwrap();
        assert_eq!(resolved.endpoint.api_key, "ds-key");
        unsafe { std::env::remove_var("DEEPSEEK_API_KEY") };
    }

    #[test]
    fn model_section_wins_over_legacy_deepseek_section() {
        let _guard = ENV_LOCK.lock().unwrap();
        unsafe {
            std::env::remove_var("HIVEMIND_API_KEY");
            std::env::remove_var("DEEPSEEK_API_KEY");
        };
        let dir =
            std::env::temp_dir().join(format!("hivemind-test-section-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let config_path = dir.join("config.toml");
        std::fs::write(
            &config_path,
            r#"
            [model]
            api_key = "new-section-key"

            [deepseek]
            api_key = "legacy-section-key"
            "#,
        )
        .unwrap();

        let resolved = resolve(
            &config_path,
            Path::new("/nonexistent/credentials.toml"),
            CliOverrides::default(),
        )
        .unwrap();
        assert_eq!(resolved.endpoint.api_key, "new-section-key");

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn hosted_credentials_round_trip_through_save_and_load() {
        let dir = std::env::temp_dir().join(format!("hivemind-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("credentials.toml");

        let creds = HostedCredentials {
            api_base: "https://hivemind-server.example/v1".to_string(),
            access_token: "hvm_live_abc123".to_string(),
        };
        save_hosted_credentials(&path, &creds).unwrap();

        let loaded = load_hosted_credentials(&path).unwrap();
        assert_eq!(loaded.api_base, creds.api_base);
        assert_eq!(loaded.access_token, creds.access_token);

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o600);
        }

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn resolve_falls_back_to_hosted_credentials_when_no_explicit_key() {
        let _guard = ENV_LOCK.lock().unwrap();
        unsafe {
            std::env::remove_var("HIVEMIND_API_KEY");
            std::env::remove_var("DEEPSEEK_API_KEY");
        };
        let dir =
            std::env::temp_dir().join(format!("hivemind-test-fallback-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let creds_path = dir.join("credentials.toml");
        save_hosted_credentials(
            &creds_path,
            &HostedCredentials {
                api_base: "https://hivemind-server.example/v1".to_string(),
                access_token: "hvm_live_abc123".to_string(),
            },
        )
        .unwrap();

        let resolved = resolve(
            Path::new("/nonexistent/config.toml"),
            &creds_path,
            CliOverrides::default(),
        )
        .unwrap();
        assert_eq!(resolved.endpoint.api_key, "hvm_live_abc123");
        assert_eq!(
            resolved.endpoint.base_url,
            "https://hivemind-server.example/v1"
        );

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn explicit_base_url_override_wins_even_with_hosted_credentials() {
        let _guard = ENV_LOCK.lock().unwrap();
        unsafe {
            std::env::remove_var("HIVEMIND_API_KEY");
            std::env::remove_var("DEEPSEEK_API_KEY");
        };
        let dir =
            std::env::temp_dir().join(format!("hivemind-test-override-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let creds_path = dir.join("credentials.toml");
        save_hosted_credentials(
            &creds_path,
            &HostedCredentials {
                api_base: "https://hivemind-server.example/v1".to_string(),
                access_token: "hvm_live_abc123".to_string(),
            },
        )
        .unwrap();

        let cli = CliOverrides {
            base_url: Some("http://localhost:9999".to_string()),
            ..Default::default()
        };
        let resolved = resolve(Path::new("/nonexistent/config.toml"), &creds_path, cli).unwrap();
        assert_eq!(resolved.endpoint.api_key, "hvm_live_abc123");
        assert_eq!(resolved.endpoint.base_url, "http://localhost:9999");

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn delete_hosted_credentials_reports_whether_a_file_existed() {
        let dir = std::env::temp_dir().join(format!("hivemind-test-delete-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("credentials.toml");

        assert!(!delete_hosted_credentials(&path).unwrap());
        save_hosted_credentials(
            &path,
            &HostedCredentials {
                api_base: "https://hivemind-server.example/v1".to_string(),
                access_token: "hvm_live_abc123".to_string(),
            },
        )
        .unwrap();
        assert!(delete_hosted_credentials(&path).unwrap());
        assert!(!path.exists());

        std::fs::remove_dir_all(&dir).ok();
    }
}
