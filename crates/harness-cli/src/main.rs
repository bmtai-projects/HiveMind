//! `hivemind` — a fast, cost-optimized AI coding agent.
//!
//! Two tiers only, for now: Flash (default, cheap) and Pro (escalated to
//! automatically when the agent looks stuck). See the workspace README for
//! the full architecture and optimization notes.

mod auth;
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

use commands::{SlashCommand, TierArg, UndoArg};
use harness_agent::Agent;
use harness_config::{CliOverrides, Tier};
use harness_tools::{
    Bash, EditFile, ListDir, ReadFile, Registry, Search, SemanticSearch, Workspace, WriteFile,
};
use input::HivePrompt;
use ui::TermUi;

const SYSTEM_PROMPT: &str =
    "You are a terminal-based coding agent operating inside a user's workspace.

You can search, read, create, and edit files, list directories, and run shell
commands via the provided tools. Work in small, verifiable steps:

- Investigate before acting: use `search` for an exact string, or
  `semantic_search` to find code by concept when you don't know the symbol
  (prefer both over shell grep); then `read_file` and `list_dir` for detail.
- To change an existing file, use `edit_file` — an exact old_string→new_string
  replacement. It is cheaper than rewriting the file and cannot corrupt the
  parts you leave untouched. Copy `old_string` verbatim from the file
  (whitespace included) and give enough context that it matches one place.
  Reserve `write_file` for creating new files.
- Make focused changes, then verify them (build/test/inspect) with run_shell.
- Prefer tools over guessing. Never claim you did something you did not do.
- When the task is complete, stop calling tools and give a short final summary
  of what you changed and how you verified it.

Be concise. Reference files by path.";

#[derive(Parser)]
#[command(
    name = "hivemind",
    version,
    about = "A fast, cost-optimized AI coding agent"
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Start the agent — interactive REPL, or headless with --prompt.
    Activate(ActivateArgs),
    /// Manage your HiveMind hosted account (sign in, sign out, check balance).
    Auth(AuthArgs),
}

#[derive(Args)]
struct AuthArgs {
    #[command(subcommand)]
    action: AuthAction,
}

#[derive(Subcommand)]
enum AuthAction {
    /// Sign in via the browser and store a hosted access token.
    Login {
        /// Override the hosted API base (mainly for testing against a non-production deployment).
        #[arg(long)]
        api_base: Option<String>,
    },
    /// Remove the stored hosted access token.
    Logout,
    /// Show whether you're signed in, and your balance.
    Status,
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

    /// Override the HiveMind API key (else $HIVEMIND_API_KEY or config.toml).
    #[arg(long)]
    api_key: Option<String>,

    /// Override the model API base URL (e.g. to point at a proxy or mock).
    #[arg(long)]
    base_url: Option<String>,

    /// Auto-approve all shell commands. Dangerous; off by default.
    #[arg(long)]
    yolo: bool,

    /// Print streamed model reasoning (Pro tier only).
    #[arg(long)]
    show_reasoning: bool,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    let result = match cli.command {
        Command::Activate(args) => run(args).await,
        Command::Auth(args) => match args.action {
            AuthAction::Login { api_base } => auth::login(api_base).await,
            AuthAction::Logout => auth::logout().await,
            AuthAction::Status => auth::status().await,
        },
    };
    if let Err(e) = result {
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
    let resolved = harness_config::resolve(
        &config_path,
        &harness_config::default_credentials_path(),
        overrides,
    )?;

    let workdir = args
        .workdir
        .canonicalize()
        .map_err(|e| anyhow::anyhow!("workdir {:?}: {e}", args.workdir))?;
    let headless = args.prompt.is_some();

    let mut registry = Registry::new();
    let ws = Workspace::new(workdir.clone());
    registry.register(Arc::new(ReadFile(ws.clone())));
    registry.register(Arc::new(WriteFile(ws.clone())));
    registry.register(Arc::new(EditFile(ws.clone())));
    registry.register(Arc::new(ListDir(ws.clone())));
    registry.register(Arc::new(Search(ws.clone())));
    registry.register(Arc::new(SemanticSearch::new(ws.clone())));

    let mut bash = Bash::new(workdir.clone());
    if !args.yolo && !headless {
        bash = bash.with_approval(Arc::new(ui::terminal_approve));
    }
    registry.register(Arc::new(bash));

    let ui: Arc<TermUi> = Arc::new(TermUi::new(args.show_reasoning));
    let mut agent = Agent::new(
        resolved.clone(),
        registry,
        ws.clone(),
        ui.clone(),
        SYSTEM_PROMPT.to_string(),
    );

    if let Some(prompt) = &args.prompt {
        let expanded = mentions::expand_mentions(prompt, &ws);
        agent.run(&expanded).await?;
        return Ok(());
    }

    repl(&mut agent, ws, ui).await
}

async fn repl(agent: &mut Agent, ws: Workspace, ui: Arc<TermUi>) -> anyhow::Result<()> {
    banner::print();
    println!("\x1b[90mTry /help for commands · @ to reference a file · Ctrl-D to quit\x1b[0m");

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
                SlashCommand::Undo(UndoArg::Count(n)) => match agent.undo(n).await {
                    Some(report) => println!(
                        "undid {} turn(s) (last: {:?}): {} file(s) restored, {} file(s) removed, {} message(s) left",
                        report.turns_undone,
                        report.label,
                        report.files_restored,
                        report.files_removed,
                        report.messages_truncated_to
                    ),
                    None => println!("nothing to undo yet"),
                },
                SlashCommand::Undo(UndoArg::Invalid(bad)) => {
                    println!("invalid /undo count {bad:?} — expected a number, e.g. /undo 2");
                }
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
