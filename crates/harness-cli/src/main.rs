//! `harness` — a fast, cost-optimized DeepSeek coding agent.
//!
//! Two tiers only, for now: Flash (default, cheap) and Pro (escalated to
//! automatically when the agent looks stuck). See the workspace README for
//! the full architecture and optimization notes.

mod ui;

use std::path::PathBuf;
use std::sync::Arc;

use clap::Parser;

use harness_agent::Agent;
use harness_config::{CliOverrides, Tier};
use harness_tools::{Bash, ListDir, ReadFile, Registry, Workspace, WriteFile};

use ui::TermUi;

const SYSTEM_PROMPT: &str =
    "You are a terminal-based coding agent operating inside a user's workspace.

You can read and write files, list directories, and run shell commands via the
provided tools. Work in small, verifiable steps:

- Investigate before acting: read files and list directories to build context.
- Make focused changes, then verify them (build/test/inspect) with run_shell.
- Prefer tools over guessing. Never claim you did something you did not do.
- When the task is complete, stop calling tools and give a short final summary
  of what you changed and how you verified it.

Be concise. Reference files by path.";

#[derive(Parser)]
#[command(
    name = "harness",
    version,
    about = "A fast, cost-optimized DeepSeek coding agent"
)]
struct Cli {
    /// Run one prompt headlessly (auto-approves shell), then exit.
    #[arg(short = 'p', long = "prompt")]
    prompt: Option<String>,

    /// Workspace root the agent operates in.
    #[arg(long, default_value = ".")]
    workdir: PathBuf,

    /// Path to config.toml. Defaults to ~/.config/harness/config.toml.
    #[arg(long)]
    config: Option<PathBuf>,

    /// Tier to start on: "flash" (default) or "pro".
    #[arg(long)]
    tier: Option<String>,

    /// Override the DeepSeek API key (else $DEEPSEEK_API_KEY or config.toml).
    #[arg(long)]
    api_key: Option<String>,

    /// Override the DeepSeek base URL (e.g. to point at a proxy or mock).
    #[arg(long)]
    base_url: Option<String>,

    /// Auto-approve all shell commands. Dangerous; off by default.
    #[arg(long)]
    yolo: bool,

    /// Print streamed model reasoning (deepseek-v4-pro).
    #[arg(long)]
    show_reasoning: bool,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    if let Err(e) = run(cli).await {
        eprintln!("\nerror: {e:#}");
        std::process::exit(1);
    }
    Ok(())
}

async fn run(cli: Cli) -> anyhow::Result<()> {
    let config_path = cli
        .config
        .clone()
        .unwrap_or_else(harness_config::default_config_path);
    let overrides = CliOverrides {
        api_key: cli.api_key.clone(),
        base_url: cli.base_url.clone(),
        tier: cli
            .tier
            .as_deref()
            .map(str::parse::<Tier>)
            .transpose()
            .map_err(|e| anyhow::anyhow!("{e}"))?,
    };
    let resolved = harness_config::resolve(&config_path, overrides)?;

    let workdir = cli
        .workdir
        .canonicalize()
        .map_err(|e| anyhow::anyhow!("workdir {:?}: {e}", cli.workdir))?;
    let headless = cli.prompt.is_some();

    let mut registry = Registry::new();
    let ws = Workspace::new(workdir.clone());
    registry.register(Arc::new(ReadFile(ws.clone())));
    registry.register(Arc::new(WriteFile(ws.clone())));
    registry.register(Arc::new(ListDir(ws.clone())));

    let mut bash = Bash::new(workdir.clone());
    if !cli.yolo && !headless {
        bash = bash.with_approval(Arc::new(ui::terminal_approve));
    }
    registry.register(Arc::new(bash));
    let tool_names = registry.names().join(", ");

    let ui: Arc<TermUi> = Arc::new(TermUi::new(cli.show_reasoning));
    let mut agent = Agent::new(resolved.clone(), registry, ui, SYSTEM_PROMPT.to_string());

    println!(
        "harness · tier={} · flash={} · pro={} · workdir={} · tools=[{tool_names}]",
        resolved.policy.default_tier,
        resolved.flash.wire_id,
        resolved.pro.wire_id,
        workdir.display(),
    );

    if let Some(prompt) = &cli.prompt {
        agent.run(prompt).await?;
        return Ok(());
    }

    repl(&mut agent).await
}

async fn repl(agent: &mut Agent) -> anyhow::Result<()> {
    use tokio::io::{AsyncBufReadExt, BufReader};

    println!("Type your request. Ctrl-D or /exit to quit.");
    let mut reader = BufReader::new(tokio::io::stdin());
    let mut line = String::new();

    loop {
        print!("\n\x1b[1m› \x1b[0m");
        ui::flush_stdout();

        line.clear();
        let bytes_read = reader.read_line(&mut line).await?;
        if bytes_read == 0 {
            println!();
            return Ok(()); // EOF (Ctrl-D)
        }
        let input = line.trim();
        if input.is_empty() {
            continue;
        }
        if input == "/exit" || input == "/quit" {
            return Ok(());
        }

        tokio::select! {
            result = agent.run(input) => {
                if let Err(e) = result {
                    eprintln!("\nerror: {e:#}");
                }
            }
            _ = tokio::signal::ctrl_c() => {
                println!("\n^C interrupted");
            }
        }
    }
}
