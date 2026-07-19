//! `hivemind` — a fast, cost-optimized DeepSeek coding agent.
//!
//! Two tiers only, for now: Flash (default, cheap) and Pro (escalated to
//! automatically when the agent looks stuck). See the workspace README for
//! the full architecture and optimization notes.

mod banner;
mod commands;
mod completion;
mod input;
mod mentions;
mod ui;

use std::path::PathBuf;
use std::sync::Arc;

use clap::{Args, Parser, Subcommand};
use reedline::Signal;

use commands::{SlashCommand, TierArg};
use harness_agent::Agent;
use harness_config::{CliOverrides, Tier};
use harness_tools::{Bash, ListDir, ReadFile, Registry, Workspace, WriteFile};
use input::HivePrompt;
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
    name = "hivemind",
    version,
    about = "A fast, cost-optimized DeepSeek coding agent"
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Start the agent — interactive REPL, or headless with --prompt.
    Activate(ActivateArgs),
}

#[derive(Args)]
struct ActivateArgs {
    /// Run one prompt headlessly (auto-approves shell), then exit.
    #[arg(short = 'p', long = "prompt")]
    prompt: Option<String>,

    /// Workspace root the agent operates in.
    #[arg(long, default_value = ".")]
    workdir: PathBuf,

    /// Path to config.toml. Defaults to ~/.config/hivemind/config.toml.
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
    let Command::Activate(args) = cli.command;
    if let Err(e) = run(args).await {
        eprintln!("\nerror: {e:#}");
        std::process::exit(1);
    }
    Ok(())
}

async fn run(args: ActivateArgs) -> anyhow::Result<()> {
    let config_path = args
        .config
        .clone()
        .unwrap_or_else(harness_config::default_config_path);
    let overrides = CliOverrides {
        api_key: args.api_key.clone(),
        base_url: args.base_url.clone(),
        tier: args
            .tier
            .as_deref()
            .map(str::parse::<Tier>)
            .transpose()
            .map_err(|e| anyhow::anyhow!("{e}"))?,
    };
    let resolved = harness_config::resolve(&config_path, overrides)?;

    let workdir = args
        .workdir
        .canonicalize()
        .map_err(|e| anyhow::anyhow!("workdir {:?}: {e}", args.workdir))?;
    let headless = args.prompt.is_some();

    let mut registry = Registry::new();
    let ws = Workspace::new(workdir.clone());
    registry.register(Arc::new(ReadFile(ws.clone())));
    registry.register(Arc::new(WriteFile(ws.clone())));
    registry.register(Arc::new(ListDir(ws.clone())));

    let mut bash = Bash::new(workdir.clone());
    if !args.yolo && !headless {
        bash = bash.with_approval(Arc::new(ui::terminal_approve));
    }
    registry.register(Arc::new(bash));
    let tool_names = registry.names().join(", ");

    let ui: Arc<TermUi> = Arc::new(TermUi::new(args.show_reasoning));
    let mut agent = Agent::new(
        resolved.clone(),
        registry,
        ui.clone(),
        SYSTEM_PROMPT.to_string(),
    );

    banner::print(&resolved, &workdir, &tool_names);

    if let Some(prompt) = &args.prompt {
        let expanded = mentions::expand_mentions(prompt, &ws);
        agent.run(&expanded).await?;
        return Ok(());
    }

    repl(&mut agent, ws, ui).await
}

async fn repl(agent: &mut Agent, ws: Workspace, ui: Arc<TermUi>) -> anyhow::Result<()> {
    println!("Type your request, or /help for commands. @path references a file. Ctrl-D to quit.");

    let history_path = harness_config::default_config_path()
        .parent()
        .map(|dir| dir.join("history.txt"));
    let mut line_editor = input::build_line_editor(&ws.root, history_path);

    loop {
        let (returned_editor, sig) = tokio::task::spawn_blocking(move || {
            let sig = line_editor.read_line(&HivePrompt);
            (line_editor, sig)
        })
        .await?;
        line_editor = returned_editor;

        let input = match sig.map_err(|e| anyhow::anyhow!("input error: {e}"))? {
            Signal::Success(line) => line,
            Signal::CtrlC => continue, // reedline already cleared the in-progress line
            Signal::CtrlD => {
                println!();
                return Ok(());
            }
        };
        let input = input.trim();
        if input.is_empty() {
            continue;
        }

        if let Some(cmd) = commands::parse(input) {
            match cmd {
                SlashCommand::Help => println!("{}", commands::HELP_TEXT),
                SlashCommand::Clear => print!("\x1b[2J\x1b[H"),
                SlashCommand::Compact => {
                    if !agent.force_compact().await {
                        println!("nothing to compact yet");
                    }
                }
                SlashCommand::Tier(TierArg::Show) => println!("tier: {}", agent.current_tier()),
                SlashCommand::Tier(TierArg::Set(tier)) => {
                    agent.set_default_tier(tier);
                    println!("tier set to {tier}");
                }
                SlashCommand::Tier(TierArg::Invalid(bad)) => {
                    println!("unknown tier {bad:?} — expected \"flash\" or \"pro\"");
                }
                SlashCommand::Cost => println!("session cost so far: ${:.6}", ui.session_cost()),
                SlashCommand::Exit => return Ok(()),
                SlashCommand::Unknown(name) => println!("unknown command /{name} — try /help"),
            }
            continue;
        }

        let expanded = mentions::expand_mentions(input, &ws);

        tokio::select! {
            result = agent.run(&expanded) => {
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
