use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use thiserror::Error;
#[derive(Debug, Clone, Copy)]
pub struct Pricing {
    pub input_per_m: f64,
    pub input_cache_read_per_m: f64,
    pub output_per_m: f64,
}

/// One model HiveMind can select, whether hosted (resolved server-side to
/// a real OpenRouter slug the client never sees) or a display entry for a
/// model a BYOK user could point their own key at directly.
#[derive(Debug, Clone, Copy)]
pub struct ModelCatalogEntry {
    pub id: &'static str,
    pub display_name: &'static str,
    pub context_window: u64,
    pub wholesale_pricing: Pricing,
    pub reasoning_efforts: &'static [&'static str],
    pub needs_explicit_cache_control: bool,
}

/// What a hosted token costs the user, as a multiple of the wholesale rate
/// in [`KNOWN_MODELS`]. Only applied when `hosted` — a BYOK user pays their
/// own provider directly and sees the raw wholesale figure.
///
/// **Must equal `MARKUP_MULTIPLIER` in HiveMind-server's `src/config.ts`.**
/// That one decrements the balance; this one is what the CLI quotes in
/// `/cost`, streams in the per-turn readout, and enforces `--budget`
/// against. They are the same number in two languages in two repositories,
/// with nothing but this comment tying them together: if they drift, every
/// figure a hosted user sees is wrong by the size of the gap, and the
/// budget stops at the wrong point. The server is authoritative — it is
/// what actually moves money — so reconcile toward it.
pub const HOSTED_MARKUP_MULTIPLIER: f64 = 1.15;
pub const KNOWN_MODELS: &[ModelCatalogEntry] = &[
    ModelCatalogEntry {
        id: "hivemind",
        display_name: "HiveMind",
        context_window: 1_048_576,
        // Re-checked live against https://openrouter.ai/api/v1/models on
        // 2026-08-17 (wireId deepseek/deepseek-v4-flash, i.e. "DeepSeek V4
        // Flash 0423" -- OpenRouter's own pricing had drifted about 13.5%
        // below what this table said, which overestimated every BYOK cost
        // readout and --budget check by the same margin.
        wholesale_pricing: Pricing {
            input_per_m: 0.0826,
            input_cache_read_per_m: 0.01652,
            output_per_m: 0.1652,
        },
        reasoning_efforts: &["high", "xhigh"],
        needs_explicit_cache_control: false,
    },
    ModelCatalogEntry {
        id: "claude-sonnet-5",
        display_name: "Claude Sonnet 5",
        context_window: 1_000_000,
        wholesale_pricing: Pricing {
            input_per_m: 2.0,
            input_cache_read_per_m: 0.2,
            output_per_m: 10.0,
        },
        reasoning_efforts: &["low", "medium", "high", "xhigh", "max"],
        // Anthropic is the one provider in this catalog that caches nothing
        // without an explicit breakpoint -- and it's also the most
        // expensive input in the catalog, so the miss costs the most here.
        needs_explicit_cache_control: true,
    },
    ModelCatalogEntry {
        id: "gpt-5.3-codex",
        display_name: "GPT-5.3 Codex",
        context_window: 400_000,
        wholesale_pricing: Pricing {
            input_per_m: 1.75,
            input_cache_read_per_m: 0.175,
            output_per_m: 14.0,
        },
        reasoning_efforts: &["none", "low", "medium", "high", "xhigh"],
        needs_explicit_cache_control: false,
    },
    ModelCatalogEntry {
        id: "gemini-3.1-pro",
        display_name: "Gemini 3.1 Pro",
        context_window: 1_048_576,
        wholesale_pricing: Pricing {
            input_per_m: 2.0,
            input_cache_read_per_m: 0.2,
            output_per_m: 12.0,
        },
        // Reasoning is mandatory for this model (always on regardless of
        // this parameter) -- listing efforts still lets a user pick how
        // hard it thinks, just never lets them turn it off.
        reasoning_efforts: &["low", "medium", "high"],
        needs_explicit_cache_control: false,
    },
    ModelCatalogEntry {
        id: "gemini-3.7-flash",
        display_name: "Gemini 3.7 Flash",
        context_window: 1_048_576,
        wholesale_pricing: Pricing {
            input_per_m: 0.375,
            input_cache_read_per_m: 0.0375,
            output_per_m: 1.875,
        },
        // Unlike gemini-3.1-pro above, reasoning is a genuine on/off
        // toggle here -- Google's "hybrid" reasoning models support a
        // zero thinking budget, unlike gemini-3.1-pro's always-on
        // reasoning. "none" is how a user reaches that off state, the
        // same convention gpt-5.3-codex already uses above.
        reasoning_efforts: &["none", "low", "medium", "high"],
        needs_explicit_cache_control: false,
    },
    ModelCatalogEntry {
        id: "grok-build",
        display_name: "Grok Build 0.1",
        context_window: 256_000,
        wholesale_pricing: Pricing {
            input_per_m: 1.0,
            input_cache_read_per_m: 0.2,
            output_per_m: 2.0,
        },
        // Reasons unconditionally, but the model is cheap enough that
        reasoning_efforts: &[],
        needs_explicit_cache_control: false,
    },
    ModelCatalogEntry {
        id: "qwen3-coder-plus",
        display_name: "Qwen3 Coder Plus",
        context_window: 1_000_000,
        wholesale_pricing: Pricing {
            input_per_m: 0.65,
            input_cache_read_per_m: 0.13,
            output_per_m: 3.25,
        },
        // Genuinely no reasoning support, per OpenRouter's own metadata.
        reasoning_efforts: &[],
        needs_explicit_cache_control: false,
    },
    ModelCatalogEntry {
        id: "kimi-k2-code",
        display_name: "Kimi K2.7 Code",
        context_window: 262_144,
        wholesale_pricing: Pricing {
            input_per_m: 0.78,
            input_cache_read_per_m: 0.15,
            output_per_m: 3.5,
        },
        // Reasons unconditionally, same caveat as grok-build above.
        reasoning_efforts: &[],
        needs_explicit_cache_control: false,
    },
];

/// Look up display metadata for a model id. `None` for anything not in
/// `KNOWN_MODELS` — a BYOK user can still send an arbitrary provider-native
/// model string, it just won't have a display name or a live cost
/// estimate; see callers in `harness-cli` for the graceful-degradation
/// behavior.
pub fn lookup_model(id: &str) -> Option<&'static ModelCatalogEntry> {
    KNOWN_MODELS.iter().find(|m| m.id == id)
}

/// Resolved model endpoint: where to send requests and with what key.
#[derive(Debug, Clone)]
pub struct Endpoint {
    pub base_url: String,
    pub api_key: String,
}

/// Agent-loop policy knobs, all with sane defaults.
#[derive(Debug, Clone)]
pub struct AgentPolicy {
    pub max_turns: u32,
    /// Compact the conversation once usage crosses this percent of the
    /// active model's context window.
    pub compaction_threshold_percent: u8,
    pub auto_escalate: bool,
    pub escalate_to_model: String,
    pub escalate_after_repeats: u32,
    /// Tool results at or above this many bytes are written to the artifact
    /// store and replaced in context by a preview plus a handle.
    ///
    /// Configurable because the right value depends on what a project's
    /// tools actually emit: the default leaves the measured median result
    /// (4,772 bytes) inline while capturing the handful that hold most of
    /// the transcript's bytes. `0` turns offloading off entirely.
    pub artifact_threshold_bytes: usize,
}

impl Default for AgentPolicy {
    fn default() -> Self {
        Self {
            max_turns: 60,
            compaction_threshold_percent: 75,
            auto_escalate: true,
            escalate_to_model: "claude-sonnet-5".to_string(),
            escalate_after_repeats: 2,
            // Kept in step with harness_tools::DEFAULT_ARTIFACT_THRESHOLD_BYTES,
            // which this crate cannot import without depending on the tool
            // layer. harness-agent depends on both and asserts they agree
            // (`the_artifact_threshold_default_matches_the_tool_layer`).
            artifact_threshold_bytes: 10_000,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HookEvent {
    PreToolUse,
    PostToolUse,
}

fn default_hook_timeout_ms() -> u64 {
    5_000
}

/// One `[[hooks]]` entry from `config.toml`. Execution (spawning, the
/// stdin envelope, exit-code/JSON decision parsing) lives in
/// `harness_agent::hooks`, not here — this crate only owns the declarative
/// shape, matching how `AgentPolicy` is data and its enforcement lives in
/// `harness_agent`.
#[derive(Debug, Clone, Deserialize)]
pub struct HookSpec {
    pub name: String,
    pub event: HookEvent,
    /// Tool names this hook applies to. `None` (the field omitted) means
    /// every tool.
    #[serde(default)]
    pub matcher: Option<Vec<String>>,
    /// Spawned the same way `run_shell` spawns (`bash -lc` on Unix,
    /// `cmd /C` on Windows) in the workspace root, with the event envelope
    /// written to stdin.
    pub command: String,
    #[serde(default = "default_hook_timeout_ms")]
    pub timeout_ms: u64,
    #[serde(default)]
    pub enforcement: bool,
}

/// Fully resolved runtime configuration.
#[derive(Debug, Clone)]
pub struct Resolved {
    pub endpoint: Endpoint,
    pub default_model: String,
    pub mode: Mode,
    pub hosted: bool,
    pub reasoning_effort: Option<String>,
    pub budget_usd: Option<f64>,
    pub policy: AgentPolicy,
    pub hooks: Vec<HookSpec>,
}

// ---- on-disk config.toml shape (all fields optional) ----

#[derive(Debug, Default, Deserialize)]
struct File {
    #[serde(default)]
    model: ModelSection,
    #[serde(default)]
    deepseek: ModelSection,
    #[serde(default)]
    agent: AgentSection,
    #[serde(default)]
    hooks: Vec<HookSpec>,
}

#[derive(Debug, Default, Deserialize)]
struct ModelSection {
    api_key: Option<String>,
    base_url: Option<String>,
    model: Option<String>,
    reasoning_effort: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Mode {
    /// Everything local and free. Works offline, costs nothing.
    #[default]
    Standard,
    /// Hosted code-aware embeddings, billed against the account balance.
    Pro,
}

impl Mode {
    pub fn as_str(self) -> &'static str {
        match self {
            Mode::Standard => "standard",
            Mode::Pro => "pro",
        }
    }

    /// Accepts the spellings a user might reasonably type or a client might
    /// send. Anything unrecognized is `None` so the caller can fall through
    /// to the next precedence tier rather than silently guessing a tier the
    /// user did not ask (and may be billed) for.
    pub fn parse(s: &str) -> Option<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "standard" | "default" | "free" | "local" => Some(Mode::Standard),
            "pro" | "remote" | "cloud" => Some(Mode::Pro),
            _ => None,
        }
    }
}

#[derive(Debug, Default, Deserialize)]
struct AgentSection {
    mode: Option<String>,
    max_turns: Option<u32>,
    compaction_threshold_percent: Option<u8>,
    auto_escalate: Option<bool>,
    escalate_to_model: Option<String>,
    escalate_after_repeats: Option<u32>,
    budget_usd: Option<f64>,
    artifact_threshold_bytes: Option<usize>,
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

/// Overrides collected from CLI flags — anything `Some` here wins over the
/// config file and environment.
#[derive(Debug, Default)]
pub struct CliOverrides {
    pub api_key: Option<String>,
    pub base_url: Option<String>,
    pub model: Option<String>,
    pub reasoning_effort: Option<String>,
    pub budget_usd: Option<f64>,
    pub mode: Option<Mode>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HostedCredentials {
    pub api_base: String,
    pub access_token: String,
}

#[derive(Serialize, Deserialize)]
struct CredentialsFile {
    hosted: HostedCredentials,
}

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

    let (api_key, base_url, hosted) = match explicit_key {
        Some(key) => (
            key,
            base_url_override.unwrap_or_else(|| "https://api.deepseek.com".to_string()),
            false,
        ),
        None => {
            let creds = load_hosted_credentials(credentials_path).ok_or(ConfigError::MissingKey)?;
            (
                creds.access_token,
                base_url_override.unwrap_or(creds.api_base),
                true,
            )
        }
    };

    // Hosted mode defaults to the branded "hivemind" alias, which resolves
    // to the full 7-model catalog server-side. BYOK talks to the upstream
    // provider directly, so it needs a real provider-native id instead.
    let default_model = cli
        .model
        .or_else(|| file.model.model.clone())
        .or_else(|| file.deepseek.model.clone())
        .unwrap_or_else(|| {
            if hosted {
                "hivemind".to_string()
            } else {
                "deepseek-v4-flash".to_string()
            }
        });

    let reasoning_effort = cli
        .reasoning_effort
        .or_else(|| file.model.reasoning_effort.clone());
    let budget_usd = cli.budget_usd.or(file.agent.budget_usd);

    // Highest wins: --mode > $HIVEMIND_MODE > [agent] mode > Standard.
    // An unparseable value at any tier falls through to the next rather
    // than defaulting to Pro -- nobody gets billed because of a typo.
    let mode = cli
        .mode
        .or_else(|| {
            std::env::var("HIVEMIND_MODE")
                .ok()
                .as_deref()
                .and_then(Mode::parse)
        })
        .or_else(|| file.agent.mode.as_deref().and_then(Mode::parse))
        .unwrap_or_default();

    let mut policy = AgentPolicy::default();
    if let Some(v) = file.agent.max_turns {
        policy.max_turns = v;
    }
    if let Some(v) = file.agent.compaction_threshold_percent {
        policy.compaction_threshold_percent = v;
    }
    if let Some(v) = file.agent.auto_escalate {
        policy.auto_escalate = v;
    }
    if let Some(v) = file.agent.escalate_to_model {
        policy.escalate_to_model = v;
    }
    if let Some(v) = file.agent.artifact_threshold_bytes {
        policy.artifact_threshold_bytes = v;
    }
    if let Some(v) = file.agent.escalate_after_repeats {
        policy.escalate_after_repeats = v;
    }

    Ok(Resolved {
        endpoint: Endpoint { base_url, api_key },
        default_model,
        mode,
        hosted,
        reasoning_effort,
        budget_usd,
        policy,
        hooks: file.hooks,
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

/// Default embedding-cache location: `~/.config/hivemind/embeddings/`.
/// Vectors keyed by chunk content, so an unchanged chunk is embedded once
/// ever rather than once per session — see `harness_tools::EmbedCache`.
pub fn default_embeddings_cache_dir() -> PathBuf {
    if let Some(home) = dirs_home() {
        return home.join(".config").join("hivemind").join("embeddings");
    }
    PathBuf::from("hivemind-embeddings")
}

/// Default artifact-store location: `~/.config/hivemind/artifacts/`.
///
/// A sibling of the session store rather than a directory inside it, so
/// each stays what it is: a flat directory of files a human can read, diff,
/// or `rm`. Artifacts are pruned with the session that produced them — see
/// `harness_tools::ArtifactStore::remove_sessions`.
pub fn default_artifacts_dir() -> PathBuf {
    if let Some(home) = dirs_home() {
        return home.join(".config").join("hivemind").join("artifacts");
    }
    PathBuf::from("hivemind-artifacts")
}

/// Default session-store location: `~/.config/hivemind/sessions/`. One JSON
/// file per saved conversation — see `harness_agent::SessionStore`.
pub fn default_sessions_dir() -> PathBuf {
    if let Some(home) = dirs_home() {
        return home.join(".config").join("hivemind").join("sessions");
    }
    PathBuf::from("hivemind-sessions")
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

/// Minimal home-directory lookup so this crate doesn't need the `dirs`
/// dependency for one call site.
///
/// Windows does not set `HOME`. Without the `USERPROFILE` fallback every
/// path above silently degraded to a *relative* one, resolved against the
/// process's cwd — so credentials written by `hivemind auth login` in one
/// directory were invisible to a `hivemind activate` launched from another
/// (which is exactly what the VS Code extension does: it spawns with the
/// workspace root as cwd).
fn dirs_home() -> Option<std::path::PathBuf> {
    fn var(key: &str) -> Option<std::path::PathBuf> {
        std::env::var_os(key)
            .filter(|v| !v.is_empty())
            .map(std::path::PathBuf::from)
    }

    #[cfg(windows)]
    {
        // `HOME` last: only MSYS/Git-Bash sets it, and when it does it can
        // hold a POSIX-style path the Windows APIs can't open.
        var("USERPROFILE")
            .or_else(|| Some(var("HOMEDRIVE")?.join(var("HOMEPATH")?)))
            .or_else(|| var("HOME"))
    }

    #[cfg(not(windows))]
    {
        var("HOME")
    }
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
    fn known_models_lookup_finds_hivemind_and_rejects_unknown() {
        assert_eq!(lookup_model("hivemind").unwrap().display_name, "HiveMind");
        assert!(lookup_model("not-a-real-model").is_none());
    }

    /// A relative path here means the file is looked up against whatever cwd
    /// the process happened to launch with -- the Windows bug where the VS
    /// Code extension (cwd = workspace root) could not see credentials that
    /// `auth login` had written elsewhere.
    #[test]
    fn default_paths_are_absolute_not_cwd_relative() {
        let _guard = ENV_LOCK.lock().unwrap();
        for path in [
            default_config_path(),
            default_credentials_path(),
            default_sessions_dir(),
        ] {
            assert!(
                path.is_absolute(),
                "{path:?} is relative, so it resolves against the process cwd"
            );
        }
    }

    #[test]
    fn empty_home_is_treated_as_unset() {
        let _guard = ENV_LOCK.lock().unwrap();
        let key = if cfg!(windows) { "USERPROFILE" } else { "HOME" };
        let saved = std::env::var_os(key);
        // SAFETY: test-only env mutation, serialized via ENV_LOCK above.
        unsafe { std::env::set_var(key, "") };
        let got = dirs_home();
        unsafe {
            match saved {
                Some(v) => std::env::set_var(key, v),
                None => std::env::remove_var(key),
            }
        };
        assert_ne!(got, Some(std::path::PathBuf::from("")));
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
        assert_eq!(resolved.default_model, "deepseek-v4-flash");
        assert!(!resolved.hosted);
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
        assert_eq!(resolved.default_model, "hivemind");
        assert!(resolved.hosted);

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn explicit_model_override_wins_even_when_hosted() {
        let _guard = ENV_LOCK.lock().unwrap();
        unsafe {
            std::env::remove_var("HIVEMIND_API_KEY");
            std::env::remove_var("DEEPSEEK_API_KEY");
        };
        let dir = std::env::temp_dir().join(format!(
            "hivemind-test-model-override-{}",
            std::process::id()
        ));
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
            model: Some("claude-sonnet-5".to_string()),
            ..Default::default()
        };
        let resolved = resolve(Path::new("/nonexistent/config.toml"), &creds_path, cli).unwrap();
        assert_eq!(resolved.default_model, "claude-sonnet-5");

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
