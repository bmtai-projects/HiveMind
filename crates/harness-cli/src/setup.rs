//! First-run provider picker for `hivemind activate`, triggered only when
//! no backend is configured at all (`ConfigError::MissingKey`) and the
//! session is interactive -- never under `--prompt` or `--protocol json`,
//! where there is no human to answer a prompt.
//!
//! Plain numbered stdin prompts, matching `auth.rs`'s own println!-driven
//! style, rather than the REPL's `reedline` editor: this runs once, before
//! any session exists, and needs none of that machinery.

use std::io::{self, Write};
use std::path::Path;

use harness_config::Dialect;

struct ProviderPreset {
    label: &'static str,
    base_url: &'static str,
    /// `None` when there's nothing safe to default -- the wizard asks.
    default_model: Option<&'static str>,
    dialect: Dialect,
}

/// Anthropic has no default: a HiveMind model alias doesn't apply to a
/// direct key, and its own model ids change over time (see
/// `ConfigError::MissingAnthropicModel`).
const PRESETS: &[ProviderPreset] = &[
    ProviderPreset {
        label: "OpenRouter",
        base_url: "https://openrouter.ai/api/v1",
        default_model: Some("deepseek/deepseek-v4-flash"),
        dialect: Dialect::OpenAiCompatible,
    },
    ProviderPreset {
        label: "OpenAI",
        base_url: "https://api.openai.com/v1",
        default_model: None,
        dialect: Dialect::OpenAiCompatible,
    },
    ProviderPreset {
        label: "Anthropic (Claude)",
        base_url: "https://api.anthropic.com",
        default_model: None,
        dialect: Dialect::Anthropic,
    },
    ProviderPreset {
        label: "DeepSeek",
        base_url: "https://api.deepseek.com",
        default_model: Some("deepseek-v4-flash"),
        dialect: Dialect::OpenAiCompatible,
    },
];

/// Runs the picker and writes the result to `config_path` (or, for
/// "HiveMind", runs the existing device-flow login instead -- the same
/// code path `hivemind auth login` uses, not a separate one). Returns once
/// a backend is configured; the caller re-resolves afterward.
pub async fn run(config_path: &Path) -> anyhow::Result<()> {
    println!("No backend configured yet. Pick one:\n");
    println!("  1) HiveMind (hosted) — sign in, pay-as-you-go, no key needed");
    for (i, p) in PRESETS.iter().enumerate() {
        println!("  {}) {}", i + 2, p.label);
    }
    let other_choice = PRESETS.len() + 2;
    println!("  {other_choice}) Other (any OpenAI-compatible endpoint)");
    print!("\n> ");
    io::stdout().flush().ok();

    let choice: usize = read_line()?.trim().parse().map_err(|_| {
        anyhow::anyhow!("not a number — run `hivemind activate` again and pick one listed above")
    })?;

    if choice == 1 {
        return crate::auth::login(None).await;
    }
    if choice == other_choice {
        return configure_other(config_path);
    }
    if let Some(preset) = choice.checked_sub(2).and_then(|i| PRESETS.get(i)) {
        return configure_byok(config_path, preset);
    }
    anyhow::bail!("{choice} isn't one of the options above — run `hivemind activate` again")
}

fn configure_byok(config_path: &Path, preset: &ProviderPreset) -> anyhow::Result<()> {
    let api_key = read_line_prompt(&format!("\nPaste your {} API key: ", preset.label))?;
    if api_key.is_empty() {
        anyhow::bail!("no key entered — run `hivemind activate` again");
    }
    let model = match preset.default_model {
        Some(m) => m.to_string(),
        None => {
            let m = read_line_prompt(&format!(
                "Model id for {} (check their docs for current names): ",
                preset.label
            ))?;
            if m.is_empty() {
                anyhow::bail!("a model id is required for {}", preset.label);
            }
            m
        }
    };
    harness_config::save_model_section(
        config_path,
        &api_key,
        preset.base_url,
        &model,
        preset.dialect,
    )?;
    println!(
        "\nSaved. `hivemind activate` will use {} now.",
        preset.label
    );
    Ok(())
}

fn configure_other(config_path: &Path) -> anyhow::Result<()> {
    let base_url =
        read_line_prompt("\nBase URL (an OpenAI-compatible /chat/completions endpoint): ")?;
    if base_url.is_empty() {
        anyhow::bail!("a base URL is required — run `hivemind activate` again");
    }
    let api_key = read_line_prompt("API key (leave blank for a keyless local server): ")?;
    let model = read_line_prompt("Model id: ")?;
    if model.is_empty() {
        anyhow::bail!("a model id is required");
    }
    harness_config::save_model_section(
        config_path,
        &api_key,
        &base_url,
        &model,
        Dialect::OpenAiCompatible,
    )?;
    println!("\nSaved. `hivemind activate` will use this endpoint now.");
    Ok(())
}

fn read_line_prompt(prompt: &str) -> anyhow::Result<String> {
    print!("{prompt}");
    io::stdout().flush().ok();
    read_line()
}

fn read_line() -> anyhow::Result<String> {
    let mut buf = String::new();
    io::stdin().read_line(&mut buf)?;
    Ok(buf.trim().to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Anthropic model ids change over time and HiveMind's own aliases
    /// don't apply to a direct key, so it must never guess -- the wizard
    /// has to ask (see `ConfigError::MissingAnthropicModel`).
    #[test]
    fn the_anthropic_preset_has_no_default_model_to_guess() {
        let anthropic = PRESETS
            .iter()
            .find(|p| p.dialect == Dialect::Anthropic)
            .unwrap();
        assert!(anthropic.default_model.is_none());
    }

    #[test]
    fn preset_labels_are_unique() {
        let mut labels: Vec<&str> = PRESETS.iter().map(|p| p.label).collect();
        let before = labels.len();
        labels.sort_unstable();
        labels.dedup();
        assert_eq!(labels.len(), before);
    }
}
