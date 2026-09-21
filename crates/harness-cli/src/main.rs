//! `hivemind` — a fast, cost-optimized AI coding agent.
//!
//! "hivemind" is the cheap default model and a HiveMind-owned brand name,
//! not a passthrough to any vendor's model id; six real third-party coding
//! models are selectable alongside it in hosted mode via `--model`/
//! `/model`, and the agent escalates off "hivemind" automatically when it
//! looks stuck. What the alias resolves to upstream is deliberately kept
//! server-side — see `harness_config::KNOWN_MODELS` for the one place that
//! is documented, and [`IDENTITY`] for why the agent must not guess at it.
//! See the workspace README for the full architecture and optimization
//! notes.

mod auth;
mod banner;
mod commands;
mod completion;
mod conventions;
mod diff;
mod hook_presets;
mod hooks_config;
mod input;
mod json_ui;
mod mentions;
mod review;
mod self_update;
mod ui;
mod update_check;

use std::io::Write;
use std::path::PathBuf;
use std::sync::Arc;

use clap::{Args, Parser, Subcommand};
use reedline::Signal;

use commands::{BudgetArg, ModelArg, ReasoningArg, SlashCommand, UndoArg, WebArg};
use harness_agent::{Agent, Ui};
use harness_config::CliOverrides;
use harness_tools::{
    Bash, CreateDiagram, CreatePdf, CreateSpreadsheet, EditFile, HostedWebClient, ListDir,
    ProjectMap, ReadFile, ReadProgram, Registry, Search, SemanticSearch, TodoWrite, WebFetch,
    WebSearch, Workspace, WriteFile,
};
use input::HivePrompt;
use json_ui::JsonUi;
use review::ReviewArgs;
use ui::TermUi;

/// The agent's own identity paragraph.
///
/// Kept as its own constant because it fixes a correctness problem, not
/// just a branding one. With no name in the prompt the model filled the gap
/// from its training priors and answered "who created you?" with
/// "Anthropic" -- while `hivemind` actually routes to DeepSeek. It was not
/// leaking a true fact, it was inventing a false attribution, and it did
/// the same generic hand-waving for "what model are you?".
///
/// The framing is deliberately the honest one rather than a denial:
/// HiveMind is a real product bmtai builds -- this agent, its tools, its
/// behaviour -- running on models it licenses and lets the user choose
/// between. That is all true, so the model is told to say it plainly and to
/// stop guessing at infrastructure it demonstrably guesses wrong.
const IDENTITY: &str = "\
You are HiveMind, a coding agent built by bmtai.

Asked who or what you are, who made you, or what you are called: you are
HiveMind, built by bmtai. HiveMind runs on a selection of language models
that bmtai licenses and routes behind the product -- users pick one with
`/model`, and `hivemind` is the cheap default. Which model is serving any
given request is infrastructure you are not told and must not guess at; say
so plainly instead of speculating, and never attribute your own creation to
a model vendor. Do not describe your architecture, training, or weights --
you have no reliable knowledge of them. Answer briefly and get back to the
work.";

const SYSTEM_PROMPT_BODY: &str = "You work from the terminal, inside a user's workspace.

You can search, read, create, and edit files, list directories, run shell
commands, and track a plan via the provided tools. Work in small, verifiable
steps:

- For any task with 3+ distinct steps -- especially ones spanning several
  files or components (e.g. \"build a backend and a frontend and wire them
  together\") -- call `todo_write` with the full breakdown before starting.
  Keep exactly one item in_progress at a time, and mark an item completed
  immediately after finishing it, not in a batch at the end. Skip it for
  single-step or trivial requests. This tracks progress for the user; it
  does not mean one tool call per turn -- `todo_write` costs nothing extra
  when sent alongside the work it describes, so include it in the same turn
  rather than spending a turn on it by itself.
- Orient before reading. In an unfamiliar codebase, or when asked to
  understand/explain/audit one, call `project_map` FIRST: one call returns
  the whole tree plus every file's definitions with line numbers. Reading
  files one by one to discover what they contain wastes context you will
  need later for the actual work -- map first, then `read_file` only the
  few files the map showed to be relevant. Scope big repos with its `path`
  argument rather than mapping everything at once.
- Read the part of a file you need, not all of it. `project_map` already
  told you which line each definition is on, so pass `offset`/`limit` to
  `read_file` and pull those lines plus surrounding context. Every file you
  read stays in the conversation and is re-billed as input on every later
  turn, so pulling 1500 lines to use 80 is the single most expensive habit
  available to you. Read the whole file when you genuinely need all of it --
  a small file, or one you are about to restructure.
- Investigate before acting: use `search` for an exact string, or
  `semantic_search` to find code by concept when you don't know the symbol
  (prefer both over shell grep); then `read_file` and `list_dir` for detail.
- Request independent tool calls together in one turn -- they are executed
  in parallel, so several files scaffolded, or several files read before
  you plan, cost about the same wall-clock time as one. Batch only calls
  that do not depend on each other's results: reading four files, or
  creating four new files whose contents you already know, all belong in a
  single turn. Never batch a call whose arguments depend on another call's
  output (read a file, then edit it based on what it said), and never batch
  a `run_shell` whose effect the next command relies on (install, then
  build). When in doubt, split the turn -- a wrong batch costs a retry,
  which is slower than the turn it saved.
- To produce a PDF or an Excel workbook, use `create_pdf` / `create_spreadsheet`
  directly -- they need no installed runtime and always produce a valid file.
  They cover structured content (headings, paragraphs, bullets, tables /
  sheets of cells and formulas), not charts, images, or pixel-precise layout.
  For a PowerPoint deck, or anything past what those two tools can express,
  write a small script using a well-known library (e.g. python-pptx,
  reportlab, openpyxl) and run it with `run_shell` -- check the runtime/
  library is available first and install it if not, then verify the output
  file actually exists afterward.
- To visualize a flowchart, sequence, class, ER, state, or gantt diagram,
  use `create_diagram` with Mermaid syntax -- it always writes the raw
  source, and additionally renders an image if `mmdc` is available. If it
  isn't, the result says so and the source is still directly usable; do not
  treat that as a failure needing a retry.
- To change an existing file, use `edit_file` — an exact old_string→new_string
  replacement. It is cheaper than rewriting the file and cannot corrupt the
  parts you leave untouched. Copy `old_string` verbatim from the file
  (whitespace included) and give enough context that it matches one place.
  Reserve `write_file` for creating new files.
- Make focused changes, then verify them with run_shell -- and mean it: if
  you scaffolded or changed a server, script, or app, actually install
  dependencies and run it, then hit it (curl an endpoint, run the test
  suite, execute the script) and read the real output. Passing a build/typecheck
  is not the same as confirming the thing works. Never claim something runs,
  passes, or is fixed without having just observed that yourself.
- Keep each `run_shell` short -- one command, or a couple joined by `&&`.
  Verify in small steps and read each result before choosing the next one.
  Do NOT pack a whole test plan into one call (start a server, curl six
  endpoints, extract ids, clean up): a long script takes far longer to write
  than to run, one bad quote wastes all of it, and a failure tells you
  nothing about which part broke. Several focused calls finish sooner than
  one big one.
- To run anything that does not exit on its own -- a dev server, a watcher,
  `npm run dev` -- pass `background: true`, never a trailing `&`. It returns
  immediately, keeps running for the rest of the session, and writes its
  output to a log file you can read. Re-running the same background command
  restarts it, so a stale process or a port still in use is never something
  you have to hunt down. Ordinary commands are cleaned up completely when
  they finish, including anything they started, so `&` buys you nothing.
- Prefer tools over guessing. Never claim you did something you did not do.
- When the task is complete, stop calling tools and report back on it. Cover,
  in prose, only the ones that apply: what changed and where; how you verified
  it and what the check actually said; what you did NOT verify; anything left
  risky, unfinished, or worth a second look. Scale it to the work -- a question
  or a one-line fix gets a sentence or two and no structure at all; a
  multi-file change earns the full set. Never list a check you did not run,
  and never quietly drop the \"did not verify\" part: an unverified change
  presented as a finished one is the single most expensive thing you can hand
  back, because it costs the user the time to discover it themselves.

Be concise. Reference files by path.";

const READ_PROGRAM_PROMPT: &str =
    "- Use `read_program` when discovery needs either several independent read-only
  operations or a bounded `search_then_read` that should search and fetch
  context around its matches in one model round trip. Keep a single simple
  read/search/list on its ordinary tool. `read_program` is read-only: never
  use it for edits, shell commands, Git, or network work.";

/// Identity first, then the working instructions, then whatever this
/// particular repository asks for.
///
/// Order is deliberate at both ends. `IDENTITY` leads because appended last
/// it would sit behind ~90 lines of workflow rules, which is exactly where
/// a model stops treating something as defining. Project conventions go
/// last for the opposite reason: they *specialize* the general rules above
/// them ("use `just test`, not `cargo test`"), and the nearest instruction
/// wins ties.
///
/// `project` is passed in rather than read here so it is loaded exactly
/// once per process. This string is the provider's context-cache prefix —
/// re-reading `AGENTS.md` each turn would let one mid-session file save
/// silently cost every remaining cache hit in the session.
/// The two environment facts a shell-using agent cannot guess and must not
/// assume.
///
/// POSIX is the training-data default, so a model told only "you work from
/// the terminal" opens a Windows session with `cd /home/user && ls` and
/// every call fails until it works out why. The harness has always known
/// which platform it is on -- `harness_tools::shell_command` picks
/// `cmd /C` or `bash -lc` from `#[cfg(windows)]` -- it just never told the
/// model. Stated once, in the cached prefix, so it costs nothing per turn.
fn platform_note() -> &'static str {
    if cfg!(windows) {
        "Environment: Windows. `run_shell` executes through `cmd /C`, so use \
         Windows commands (`dir`, `type`, `copy`, `where`) and backslash paths. \
         POSIX tools and paths like /home or /usr do not exist here."
    } else if cfg!(target_os = "macos") {
        "Environment: macOS. `run_shell` executes through `bash -lc`. BSD \
         variants of `sed`, `find` and `date` differ from GNU ones."
    } else {
        "Environment: Linux. `run_shell` executes through `bash -lc`."
    }
}

fn system_prompt(project: Option<&str>, read_program_available: bool) -> String {
    let mut prompt = format!("{IDENTITY}\n\n{SYSTEM_PROMPT_BODY}\n\n{}", platform_note());
    if read_program_available {
        prompt.push_str("\n\n");
        prompt.push_str(READ_PROGRAM_PROMPT);
    }
    if let Some(project) = project {
        prompt.push_str("\n\n");
        prompt.push_str(project);
    }
    prompt
}

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
// Parsed once at startup; the variant size gap costs nothing here.
#[allow(clippy::large_enum_variant)]
enum Command {
    /// Start the agent — interactive REPL, or headless with --prompt.
    Activate(ActivateArgs),
    /// Review a local Git change without modifying the repository.
    Review(ReviewArgs),
    /// Manage your HiveMind hosted account (sign in, sign out, check balance).
    Auth(AuthArgs),
    /// List models selectable with --model (hosted mode: "hivemind" plus 6
    /// real third-party coding models; BYOK: whatever your provider key
    /// itself supports).
    Models,
    /// List saved sessions for a workspace, newest first, for `--resume`.
    Sessions(SessionsArgs),
    /// Download the latest release for this platform and replace the
    /// running binary with it. Never runs on its own -- see
    /// `crate::self_update` for the full explanation of why this is
    /// explicit-only, not automatic.
    Update,
    /// Turn built-in safety rules on or off (e.g. "never write outside
    /// src/") without hand-writing shell into config.toml yourself. Run
    /// with no subcommand to list what's available.
    Hooks(HooksArgs),
}

#[derive(Args)]
struct HooksArgs {
    #[command(subcommand)]
    action: Option<HooksAction>,
}

#[derive(Subcommand)]
enum HooksAction {
    /// List available presets and whether each is currently enabled.
    List,
    /// Turn a preset on, writing it into config.toml.
    Enable {
        preset: String,
        /// Required by presets that need one (e.g. a directory); omit for
        /// ones that don't -- `hivemind hooks list` shows which is which.
        arg: Option<String>,
    },
    /// Turn a preset off, removing it from config.toml.
    Disable { preset: String, arg: Option<String> },
    /// Internal: evaluate one preset against the hook JSON envelope on
    /// stdin. This is what a preset's generated hook actually runs when
    /// the agent is about to call a tool -- not meant to be run by hand.
    #[command(hide = true)]
    Check { preset: String, arg: Option<String> },
}

#[derive(Args)]
struct SessionsArgs {
    /// Workspace whose sessions to list (default: current directory).
    #[arg(long, default_value = ".")]
    workdir: PathBuf,

    /// Emit the listing as JSON instead of a human table, for editors and
    /// scripts. One object per session, newest first.
    #[arg(long)]
    json: bool,

    /// Delete sessions across *all* workspaces last updated more than this
    /// many days ago, then continue with the listing. 0 disables pruning.
    /// Deliberately opt-in: nothing deletes a saved conversation unless
    /// asked to.
    #[arg(long, value_name = "DAYS")]
    prune_older_than: Option<u64>,

    /// With --prune-older-than, report what would be deleted without
    /// deleting anything.
    #[arg(long, requires = "prune_older_than")]
    dry_run: bool,

    /// Session ids never to prune, even when older than the cutoff -- for a
    /// host that has these open right now and would otherwise delete a
    /// conversation out from under a live window. Repeatable.
    #[arg(long = "keep", value_name = "ID")]
    keep: Vec<String>,

    /// Delete one session by id and exit.
    #[arg(long, value_name = "ID", conflicts_with = "prune_older_than")]
    delete: Option<String>,
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

/// Machine-readable protocol `activate` can speak instead of the
/// interactive terminal REPL -- for driving `hivemind` as a subprocess
/// (e.g. from an editor extension) rather than a human typing at a
/// terminal. Only one value exists so far; anything else is a clap parse
/// error, not a silent no-op, since `--protocol` is otherwise unset (`None`)
/// and silently ignored.
#[derive(Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
enum Protocol {
    /// Newline-delimited JSON events on stdout, newline-delimited JSON
    /// commands on stdin. See `json_ui::JsonUi` for the event shapes and
    /// `run_json_protocol` for the dispatch loop.
    Json,
}

#[derive(Args)]
struct ActivateArgs {
    /// Run one prompt headlessly (auto-approves shell), then exit.
    #[arg(short = 'p', long = "prompt")]
    prompt: Option<String>,

    /// Speak a machine-readable protocol on stdin/stdout instead of the
    /// interactive terminal REPL. Only "json" is implemented -- see
    /// `json_ui` for the wire format.
    #[arg(long, value_enum)]
    protocol: Option<Protocol>,

    /// Workspace root the agent operates in.
    #[arg(long, default_value = ".")]
    workdir: PathBuf,

    /// Path to config.toml. Defaults to ~/.config/hivemind/config.toml.
    #[arg(long)]
    config: Option<PathBuf>,

    /// Model to start on. Hosted mode: "hivemind" (default) or one of the 6
    /// real coding models (run `hivemind models` to list them). BYOK: a
    /// model id your own provider key recognizes.
    #[arg(long)]
    model: Option<String>,

    /// Override the HiveMind API key (else $HIVEMIND_API_KEY or config.toml).
    #[arg(long)]
    api_key: Option<String>,

    /// Override the model API base URL (e.g. to point at a proxy or mock).
    #[arg(long)]
    base_url: Option<String>,

    /// Auto-approve all shell commands. Dangerous; off by default.
    #[arg(long)]
    yolo: bool,

    /// Reasoning effort for models that support it (e.g. "high", "medium";
    /// valid values vary per model -- run `hivemind models` or `/reasoning`
    /// with no argument to check). Off by default: reasoning-capable
    /// models run slower and cost more, so it's opt-in, not assumed.
    #[arg(long)]
    reasoning_effort: Option<String>,

    /// Print streamed model reasoning (reasoning-capable models only). A
    /// lightweight "thinking..." indicator shows either way -- this flag
    /// only controls whether the full raw text is also dumped.
    #[arg(long)]
    show_reasoning: bool,

    /// Hard cap on cumulative estimated USD spend for the session. Stops
    /// cleanly (not an error) once reached, at the next turn boundary --
    /// an in-flight turn always finishes first. Unbounded by default.
    #[arg(long)]
    budget: Option<f64>,

    /// "standard" (default) runs everything locally and free. "pro" adds
    /// hosted code-aware embeddings for sharper semantic_search, billed
    /// against your balance. Also settable via `[agent] mode` in
    /// config.toml or $HIVEMIND_MODE.
    #[arg(long)]
    mode: Option<String>,

    /// Enable hosted public web search for this session. Off by default and
    /// available only with `hivemind auth login` credentials.
    #[arg(long)]
    web: bool,

    /// Resume the most recent session for this workspace. Restores the
    /// conversation, model, and accumulated spend -- but not `/undo`
    /// history, which is deliberately never carried across processes (the
    /// files it would restore may have changed since).
    #[arg(long = "continue", conflicts_with = "resume")]
    continue_session: bool,

    /// Resume one specific session by id (see `hivemind sessions`).
    #[arg(long, value_name = "ID")]
    resume: Option<String>,
}

fn print_model_catalog() {
    println!("Available models:");
    for m in harness_config::KNOWN_MODELS {
        let reasoning = if m.reasoning_efforts.is_empty() {
            String::new()
        } else {
            format!(" · reasoning: {}", m.reasoning_efforts.join("/"))
        };
        println!(
            "  {:<18} {:<18} ${:.2} in / ${:.2} out per M tok · {}K context{reasoning}",
            m.id,
            m.display_name,
            m.wholesale_pricing.input_per_m,
            m.wholesale_pricing.output_per_m,
            m.context_window / 1000,
        );
    }
    println!("Pick with --model <id>, or /model <id> in the REPL.");
}

/// Roughly how long ago, in the coarsest unit that's still informative --
/// a session list is scanned, not studied, so "3h ago" beats a timestamp.
fn humanize_age(secs_ago: u64) -> String {
    match secs_ago {
        0..=59 => "just now".to_string(),
        60..=3599 => format!("{}m ago", secs_ago / 60),
        3600..=86_399 => format!("{}h ago", secs_ago / 3600),
        _ => format!("{}d ago", secs_ago / 86_400),
    }
}

fn list_sessions(args: SessionsArgs) -> anyhow::Result<()> {
    let workdir = args
        .workdir
        .canonicalize()
        .map_err(|e| anyhow::anyhow!("workdir {:?}: {e}", args.workdir))?;
    let workspace = workdir.to_string_lossy().to_string();
    let store = harness_agent::SessionStore::new(harness_config::default_sessions_dir());

    if let Some(id) = &args.delete {
        let existed = store.delete(id)?;
        if args.json {
            println!(
                "{}",
                serde_json::json!({"deleted": if existed { vec![id.clone()] } else { vec![] }})
            );
        } else if existed {
            println!("Deleted session {id}");
        } else {
            println!("No session {id}");
        }
        return Ok(());
    }

    let mut pruned: Vec<String> = Vec::new();
    if let Some(days) = args.prune_older_than.filter(|d| *d > 0) {
        let max_age = days.saturating_mul(86_400);
        if args.dry_run {
            let now = harness_agent::unix_now();
            pruned = store
                .list_all()
                .into_iter()
                .filter(|s| {
                    now.saturating_sub(s.updated_at) > max_age && !args.keep.contains(&s.id)
                })
                .map(|s| s.id)
                .collect();
        } else {
            // Re-adding a kept id is not possible after the fact, so the
            // filter has to happen inside the store, not on its result.
            pruned = store.prune_older_than_except(max_age, &args.keep);
            // A pruned session's artifacts must go with it. Left behind they
            // would accumulate forever with nothing referencing them -- a
            // worse bug than the token cost artifacts exist to fix.
            let artifacts =
                harness_tools::ArtifactStore::new(harness_config::default_artifacts_dir());
            artifacts.remove_sessions(&pruned);
            // And sweep what no session will ever claim: unpersisted `-p`
            // runs store under a per-process id, and a crash can orphan a
            // directory the same way.
            let live: Vec<String> = store.list_all().into_iter().map(|s| s.id).collect();
            artifacts.prune_orphans(max_age, &live);
        }
    }

    let sessions = store.list_for_workspace(&workspace);

    if args.json {
        let items: Vec<_> = sessions
            .iter()
            .map(|s| {
                serde_json::json!({
                    "id": s.id,
                    "title": s.title,
                    "updated_at": s.updated_at,
                    "turns": s.turns,
                    "cost_usd": s.cost_usd,
                })
            })
            .collect();
        println!(
            "{}",
            serde_json::json!({
                "workspace": workspace,
                "sessions": items,
                "pruned": pruned,
            })
        );
        return Ok(());
    }

    if !pruned.is_empty() {
        let verb = if args.dry_run {
            "Would delete"
        } else {
            "Deleted"
        };
        println!("{verb} {} session(s) past the age cutoff.", pruned.len());
    }

    if sessions.is_empty() {
        println!("No saved sessions for {workspace}");
        println!(
            "Sessions are recorded automatically; resume the latest with `hivemind activate --continue`."
        );
        return Ok(());
    }

    println!("Sessions for {workspace}:");
    let now = harness_agent::unix_now();
    for s in &sessions {
        let title = if s.title.is_empty() {
            "(no prompt yet)"
        } else {
            &s.title
        };
        println!(
            "  {:<22} {:>9}  {:>3} turns  ${:.4}  {}",
            s.id,
            humanize_age(now.saturating_sub(s.updated_at)),
            s.turns,
            s.cost_usd,
            title,
        );
    }
    println!("\nResume with `hivemind activate --resume <ID>`, or `--continue` for the newest.");
    Ok(())
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    let result = match cli.command {
        Command::Activate(args) => run(args).await,
        Command::Review(args) => review::run(args).await,
        Command::Auth(args) => match args.action {
            AuthAction::Login { api_base } => auth::login(api_base).await,
            AuthAction::Logout => auth::logout().await,
            AuthAction::Status => auth::status().await,
        },
        Command::Models => {
            print_model_catalog();
            Ok(())
        }
        Command::Sessions(args) => list_sessions(args),
        Command::Update => self_update::run().await,
        Command::Hooks(args) => match args.action {
            None | Some(HooksAction::List) => {
                hooks_config::list();
                Ok(())
            }
            Some(HooksAction::Enable { preset, arg }) => {
                hooks_config::enable(&preset, arg.as_deref())
            }
            Some(HooksAction::Disable { preset, arg }) => {
                hooks_config::disable(&preset, arg.as_deref())
            }
            Some(HooksAction::Check { preset, arg }) => {
                hooks_config::check(&preset, arg.as_deref())
            }
        },
    };
    if let Err(e) = result {
        eprintln!("\nerror: {e:#}");
        std::process::exit(1);
    }
    Ok(())
}

async fn run(args: ActivateArgs) -> anyhow::Result<()> {
    // Fired before any of the startup I/O below so its round trip overlaps
    // config resolution, workspace canonicalization, and session loading
    // rather than being serialized in front of the first prompt.
    let update_check = banner::start_update_check();

    let config_path = args
        .config
        .clone()
        .unwrap_or_else(harness_config::default_config_path);
    let overrides = CliOverrides {
        api_key: args.api_key.clone(),
        base_url: args.base_url.clone(),
        model: args.model.clone(),
        reasoning_effort: args.reasoning_effort.clone(),
        budget_usd: args.budget,
        mode: args.mode.as_deref().and_then(harness_config::Mode::parse),
    };
    let resolved = harness_config::resolve(
        &config_path,
        &harness_config::default_credentials_path(),
        overrides,
    )?;
    if args.web && !resolved.hosted {
        anyhow::bail!("--web requires HiveMind hosted sign-in; run `hivemind auth login`");
    }

    let workdir = args
        .workdir
        .canonicalize()
        .map_err(|e| anyhow::anyhow!("workdir {:?}: {e}", args.workdir))?;
    let headless = args.prompt.is_some();
    let protocol_json = matches!(args.protocol, Some(Protocol::Json));

    // Constructed up front, before any tool is registered, so a tool that
    // needs to report progress (below) can route through whichever Ui this
    // session actually uses -- rather than writing to a terminal directly,
    // bypassing the one interface a host (a TUI, or the VS Code extension
    // via --protocol json) is meant to go through. Exactly one of these is
    // ever `Some`; `TermUi::new`'s inputs are both already available here.
    let json_ui: Option<Arc<JsonUi>> = protocol_json.then(|| Arc::new(JsonUi::new()));
    let term_ui: Option<Arc<TermUi>> =
        (!protocol_json).then(|| Arc::new(TermUi::new(args.show_reasoning, resolved.budget_usd)));

    // Read once, here, and hold it for the process's lifetime -- see
    // `system_prompt`'s doc comment for why re-reading would be expensive.
    let project_conventions = conventions::load(&workdir);

    let mut registry = Registry::new();
    let ws = Workspace::new(workdir.clone());
    // Hooks match concrete tool names. Until composite sub-operations can be
    // surfaced to them individually, enabling read_program in a hooked
    // session could bypass a policy targeting read_file/search/list_dir.
    // Keeping the existing tools only preserves every configured hook's
    // enforcement and observability semantics.
    let read_program_available = resolved.hooks.is_empty();
    let artifact_store = Arc::new(harness_tools::ArtifactStore::new(
        harness_config::default_artifacts_dir(),
    ));
    registry.register(Arc::new(ReadFile(ws.clone())));
    registry.register(Arc::new(WriteFile(ws.clone())));
    registry.register(Arc::new(EditFile(ws.clone())));
    registry.register(Arc::new(ListDir(ws.clone())));
    registry.register(Arc::new(ProjectMap(ws.clone())));
    registry.register(Arc::new(Search(ws.clone())));
    if read_program_available {
        registry.register(Arc::new(ReadProgram::new(ws.clone())));
    }
    if resolved.hosted
        && let Some(web_client) =
            HostedWebClient::new(&resolved.endpoint.base_url, &resolved.endpoint.api_key)
    {
        let web_client = Arc::new(web_client);
        registry.register_disabled(Arc::new(WebSearch(web_client.clone())));
        registry.register_disabled(Arc::new(WebFetch(web_client)));
    }
    // Pro mode swaps semantic_search onto hosted embeddings. Standard mode
    // -- and any Pro session that can't reach them -- keeps the local
    // embedder, which is also retained inside SemanticSearch as the runtime
    // fallback, so a network failure degrades quality instead of failing
    // the task.
    let hosted_token = resolved
        .hosted
        .then_some(resolved.endpoint.api_key.as_str());
    let pro = resolved.mode == harness_config::Mode::Pro;
    let progress_ui: harness_tools::ProgressSink = {
        let json_ui = json_ui.clone();
        let term_ui = term_ui.clone();
        Arc::new(move |msg: &str| {
            if let Some(j) = &json_ui {
                j.tool_progress("semantic_search", msg);
            } else if let Some(t) = &term_ui {
                t.tool_progress("semantic_search", msg);
            }
        })
    };
    registry.register(Arc::new(
        match harness_tools::RemoteEmbedder::for_pro_mode(pro, hosted_token) {
            Some(remote) => SemanticSearch::new(ws.clone())
                .with_reranker(Arc::new(remote))
                .with_cache_dir(harness_config::default_embeddings_cache_dir())
                .with_progress(progress_ui),
            None => {
                if pro {
                    eprintln!(
                        "pro mode: hosted embeddings need a signed-in account (`hivemind auth login`); using local search"
                    );
                }
                SemanticSearch::new(ws.clone()).with_progress(progress_ui)
            }
        },
    ));
    // Registered unconditionally: a resumed session can carry handles from
    // an earlier run, so the tool has to exist even before this process
    // stores anything of its own.
    registry.register(Arc::new(harness_tools::ReadArtifact(
        artifact_store.clone(),
    )));
    registry.register(Arc::new(TodoWrite));
    registry.register(Arc::new(CreatePdf(ws.clone())));
    registry.register(Arc::new(CreateSpreadsheet(ws.clone())));
    registry.register(Arc::new(CreateDiagram(ws.clone())));

    // Shell approval has three shapes:
    // - `--yolo` (either mode): auto-approved, no round-trip at all.
    // - `--protocol json`, not yolo: round-trips through JsonUi's
    //   approval_request/approve handshake (see `json_ui::JsonUi::request_shell_approval`).
    // - interactive terminal, not headless, not yolo: the existing stdin
    //   y/N prompt. Headless `-p` (no `--protocol`) keeps its pre-existing
    //   behavior of auto-approving, unchanged.
    let mut bash = Bash::new(workdir.clone());
    if let Some(json_ui) = &json_ui {
        if !args.yolo {
            let json_ui = json_ui.clone();
            bash = bash.with_approval(Arc::new(move |cmd: &str| {
                json_ui.request_shell_approval(cmd)
            }));
        }
    } else if !args.yolo && !headless {
        bash = bash.with_approval(Arc::new(ui::terminal_approve));
    }
    registry.register(Arc::new(bash));

    let workspace = workdir.to_string_lossy().to_string();
    let store = harness_agent::SessionStore::new(harness_config::default_sessions_dir());

    if let Some(json_ui) = json_ui {
        let mut agent = Agent::new(
            resolved.clone(),
            registry,
            ws.clone(),
            json_ui.clone(),
            system_prompt(project_conventions.as_deref(), read_program_available),
        );
        agent.warm_connection();
        // Off when the threshold is 0, which is how a user turns offloading
        // off entirely without the harness needing a second switch.
        if resolved.policy.artifact_threshold_bytes > 0 {
            agent.enable_artifacts(
                (*artifact_store).clone(),
                resolved.policy.artifact_threshold_bytes,
            );
        }
        // Protocol mode persists too: an editor window reloading is exactly
        // the kind of ordinary interruption a session must survive.
        attach_or_restore_session(
            &mut agent,
            &args,
            store,
            workspace,
            false,
            project_conventions.as_deref(),
            read_program_available,
        )?;
        if args.web {
            agent.set_web_enabled(true).map_err(anyhow::Error::msg)?;
        }
        return run_json_protocol(&mut agent, ws, json_ui).await;
    }

    let ui = term_ui.expect("interactive mode always constructs a TermUi above");
    let mut agent = Agent::new(
        resolved.clone(),
        registry,
        ws.clone(),
        ui.clone(),
        system_prompt(project_conventions.as_deref(), read_program_available),
    );
    agent.warm_connection();
    // Off when the threshold is 0, which is how a user turns offloading
    // off entirely without the harness needing a second switch.
    if resolved.policy.artifact_threshold_bytes > 0 {
        agent.enable_artifacts(
            (*artifact_store).clone(),
            resolved.policy.artifact_threshold_bytes,
        );
    }

    if let Some(prompt) = &args.prompt {
        // A one-shot `-p` run has nothing worth resuming later, so it stays
        // unpersisted -- no session file, no clutter in `hivemind sessions`.
        if args.web {
            agent.set_web_enabled(true).map_err(anyhow::Error::msg)?;
        }
        let expanded = mentions::expand_mentions(prompt, &ws);
        agent.run(&expanded).await?;
        return Ok(());
    }

    attach_or_restore_session(
        &mut agent,
        &args,
        store,
        workspace,
        true,
        project_conventions.as_deref(),
        read_program_available,
    )?;
    if args.web {
        agent.set_web_enabled(true).map_err(anyhow::Error::msg)?;
    }

    repl(&mut agent, ws, ui, args.yolo, update_check, resolved.mode).await
}

/// Resolve `--continue` / `--resume` into either a restored session or a
/// fresh one, and turn persistence on either way.
/// `announce` is off in protocol mode: stdout there carries ndjson only, so
/// a human-readable banner would be an unparseable line to the client.
fn attach_or_restore_session(
    agent: &mut Agent,
    args: &ActivateArgs,
    store: harness_agent::SessionStore,
    workspace: String,
    announce: bool,
    project_conventions: Option<&str>,
    read_program_available: bool,
) -> anyhow::Result<()> {
    let restored = if let Some(id) = &args.resume {
        // An explicit id that doesn't exist is a real error: silently
        // starting fresh would look like the resume worked and quietly
        // strand the session the user asked for.
        Some(store.load(id)?)
    } else if args.continue_session {
        match store.latest_for_workspace(&workspace) {
            Some(rec) => Some(rec),
            None => {
                if announce {
                    println!(
                        "\x1b[90mno previous session for this workspace — starting a new one\x1b[0m"
                    );
                }
                None
            }
        }
    } else {
        None
    };

    match restored {
        Some(record) => {
            let turns = record.turn_count();
            let title = record.title.clone();
            let cost = record.session_cost_usd;
            agent.restore(
                record,
                store,
                system_prompt(project_conventions, read_program_available),
            );
            if announce {
                println!(
                    "\x1b[90m⟲ resumed session {} — {turns} turns, ${cost:.4} spent{}\x1b[0m",
                    agent.session_id().unwrap_or("?"),
                    if title.is_empty() {
                        String::new()
                    } else {
                        format!(" · {title}")
                    }
                );
            }
        }
        None => {
            agent.enable_persistence(store, workspace);
        }
    }
    Ok(())
}

/// Headless dispatch loop for `--protocol json`: emits `ready`, then reads
/// ndjson commands from stdin and drives the same `Agent` the terminal REPL
/// uses, dispatching to the exact same public methods (`run`, `set_model`,
/// `set_reasoning_effort`, `set_budget_usd`, `undo`, `force_compact`) the
/// REPL's slash-command match arms already call. Exits with `Ok(())`
/// (process code 0) on stdin EOF.
///
/// Split into two concurrent halves, not one straight-line loop, because
/// shell-command approval genuinely needs it: `agent.run()` can block deep
/// inside `spawn_blocking` (see `harness_tools::bash::Bash::approved`)
/// waiting on an `approve` reply for a request it just emitted. If reading
/// stdin and calling `agent.run()` happened in the same loop iteration,
/// that `.await` would starve the very stdin read that could unblock it --
/// a real deadlock (caught by this implementation's own smoke test, not
/// just a theoretical concern). So a dedicated reader task owns stdin for
/// the whole process lifetime and resolves `approve` replies immediately,
/// off to the side; every other command is forwarded over a channel to
/// this function's main loop, which is the sole owner of `&mut Agent` and
/// processes them strictly one at a time, in arrival order.
async fn run_json_protocol(
    agent: &mut Agent,
    ws: Workspace,
    ui: Arc<JsonUi>,
) -> anyhow::Result<()> {
    use tokio::io::AsyncBufReadExt;

    ui.emit_ready(
        agent.session_id(),
        agent.web_available(),
        agent.web_enabled(),
    );
    // A fresh session's history is exactly the one system message;
    // emit_history treats that as "nothing to replay" and stays silent, so
    // this is safe to call unconditionally rather than threading a
    // "was this actually resumed" flag through from `attach_or_restore_session`.
    ui.emit_history(agent.history());

    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<json_ui::Command>();
    let reader_ui = ui.clone();
    let reader_queue = agent.interjections();
    let reader = tokio::spawn(async move {
        let stdin = tokio::io::stdin();
        let mut lines = tokio::io::BufReader::new(stdin).lines();
        loop {
            let line = match lines.next_line().await {
                Ok(Some(line)) => line,
                Ok(None) => break, // stdin EOF -- unblocks the main loop below via tx's drop.
                Err(e) => {
                    reader_ui.emit_error(&format!("stdin read error: {e}"));
                    break;
                }
            };
            let line = line.trim();
            if line.is_empty() {
                continue;
            }
            let cmd: json_ui::Command = match serde_json::from_str(line) {
                Ok(c) => c,
                Err(e) => {
                    reader_ui.emit_error(&format!("invalid command: {e}"));
                    continue;
                }
            };
            match cmd {
                // Both of these are resolved right here rather than
                // forwarded: they are the commands that exist *because* a
                // turn is already in flight, so queueing them behind the
                // main loop -- which is busy awaiting that very turn --
                // would deadlock the one thing they're for.
                json_ui::Command::Approve {
                    request_id,
                    approved,
                } => reader_ui.resolve_approval(&request_id, approved),
                json_ui::Command::Interject { text } => {
                    reader_queue.push(text);
                }
                other => {
                    if tx.send(other).is_err() {
                        break; // main loop already gone
                    }
                }
            }
        }
        // `tx` drops here at task exit either way, which ends the main
        // loop's `rx.recv()` below -- the single EOF signal both error
        // exits and a clean stdin close funnel through.
    });

    while let Some(cmd) = rx.recv().await {
        match cmd {
            json_ui::Command::UserMessage { text } => {
                let expanded = mentions::expand_mentions(&text, &ws);
                if let Err(e) = agent.run(&expanded).await {
                    ui.emit_error(&format!("{e:#}"));
                }
                // Always emitted after a user_message's run() settles,
                // success or error -- signals the extension may send the
                // next line.
                ui.emit_turn_done();
            }
            json_ui::Command::SetModel { model } => {
                agent.set_model(model);
                ui.emit_turn_done();
            }
            json_ui::Command::SetReasoningEffort { effort } => {
                agent.set_reasoning_effort(effort);
                ui.emit_turn_done();
            }
            json_ui::Command::SetBudget { budget_usd } => {
                agent.set_budget_usd(budget_usd);
                ui.emit_turn_done();
            }
            json_ui::Command::SetWebEnabled { enabled } => {
                if let Err(message) = agent.set_web_enabled(enabled) {
                    ui.emit_error(message);
                }
                ui.emit_web_mode(agent.web_available(), agent.web_enabled());
                ui.emit_turn_done();
            }
            json_ui::Command::Undo { n } => {
                let report = agent.undo(n).await;
                ui.emit_undo_result(report.as_ref());
                ui.emit_turn_done();
            }
            json_ui::Command::ForceCompact => {
                // force_compact() itself fires Ui::compacted() when it did
                // anything; nothing else to report on the no-op path.
                agent.force_compact().await;
                ui.emit_turn_done();
            }
            json_ui::Command::Approve { .. } | json_ui::Command::Interject { .. } => {
                unreachable!("filtered out and resolved directly by the reader task above")
            }
        }
    }

    let _ = reader.await;
    Ok(())
}

async fn repl(
    agent: &mut Agent,
    ws: Workspace,
    ui: Arc<TermUi>,
    yolo: bool,
    update_check: tokio::task::JoinHandle<Option<String>>,
    mode: harness_config::Mode,
) -> anyhow::Result<()> {
    banner::print(update_check).await;
    println!("\x1b[90m@ to reference a file\x1b[0m");

    let history_path = harness_config::default_config_path()
        .parent()
        .map(|dir| dir.join("history.txt"));
    let mut line_editor = input::build_line_editor(&ws.root, history_path);

    loop {
        let prompt = HivePrompt {
            model: agent.current_model().to_string(),
            yolo,
        };
        let (returned_editor, sig) = tokio::task::spawn_blocking(move || {
            let sig = line_editor.read_line(&prompt);
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
                SlashCommand::Status => print_status(agent, mode),
                SlashCommand::Context => print_context(agent),
                SlashCommand::Diff(filter) => print_diff(agent, filter.as_deref()),
                SlashCommand::Clear => print!("\x1b[2J\x1b[H"),
                SlashCommand::Compact => {
                    if !agent.force_compact().await {
                        println!("nothing to compact yet");
                    }
                }
                SlashCommand::Model(ModelArg::Show) => {
                    println!("model: {}", agent.current_model());
                    print_model_catalog();
                }
                SlashCommand::Model(ModelArg::Set(id)) => {
                    agent.set_model(id.clone());
                    println!("model set to {id}");
                }
                SlashCommand::Reasoning(ReasoningArg::Show) => {
                    match agent.reasoning_effort() {
                        Some(level) => println!("reasoning: {level}"),
                        None => println!("reasoning: off"),
                    }
                    let valid = agent.reasoning_efforts_for_current_model();
                    if valid.is_empty() {
                        println!(
                            "  {} doesn't support adjustable reasoning effort",
                            agent.current_model()
                        );
                    } else {
                        println!(
                            "  valid for {}: {} (or \"off\")",
                            agent.current_model(),
                            valid.join(", ")
                        );
                    }
                }
                SlashCommand::Reasoning(ReasoningArg::Off) => {
                    agent.set_reasoning_effort(None);
                    println!("reasoning off");
                }
                SlashCommand::Reasoning(ReasoningArg::Set(level)) => {
                    let valid = agent.reasoning_efforts_for_current_model();
                    if valid.is_empty() {
                        println!(
                            "{} doesn't support adjustable reasoning effort",
                            agent.current_model()
                        );
                    } else if valid.contains(&level.as_str()) {
                        agent.set_reasoning_effort(Some(level.clone()));
                        println!("reasoning set to {level}");
                    } else {
                        println!(
                            "invalid reasoning level {level:?} for {} -- valid: {}",
                            agent.current_model(),
                            valid.join(", ")
                        );
                    }
                }
                SlashCommand::Budget(BudgetArg::Show) => match agent.budget_usd() {
                    Some(b) => println!(
                        "budget: ${b:.2} (spent ${:.6} so far)",
                        agent.session_cost_usd()
                    ),
                    None => println!("budget: off (unbounded)"),
                },
                SlashCommand::Budget(BudgetArg::Off) => {
                    agent.set_budget_usd(None);
                    ui.set_budget_display(None);
                    println!("budget off");
                }
                SlashCommand::Budget(BudgetArg::Set(amount)) => {
                    if amount <= 0.0 {
                        println!("budget must be a positive number of dollars, e.g. /budget 0.50");
                    } else {
                        agent.set_budget_usd(Some(amount));
                        ui.set_budget_display(Some(amount));
                        println!("budget set to ${amount:.2}");
                    }
                }
                SlashCommand::Budget(BudgetArg::Invalid(bad)) => {
                    println!(
                        "invalid budget {bad:?} — expected a number, e.g. /budget 0.50, or /budget off"
                    );
                }
                SlashCommand::Web(WebArg::Show) => print_web_status(agent),
                SlashCommand::Web(WebArg::On) => match agent.set_web_enabled(true) {
                    Ok(()) => println!("web: on (up to 3 search/fetch operations per request)"),
                    Err(message) => println!("{message}"),
                },
                SlashCommand::Web(WebArg::Off) => {
                    let _ = agent.set_web_enabled(false);
                    println!("web: off");
                }
                SlashCommand::Web(WebArg::Invalid(bad)) => {
                    println!(
                        "invalid web mode {bad:?} — expected /web on, /web off, or /web status"
                    );
                }
                SlashCommand::Cost => {
                    println!("session cost so far: ${:.6}", agent.session_cost_usd())
                }
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

        run_steerable(agent, &expanded).await;
    }
}

/// Run one input to completion, with Ctrl+C repurposed from "abort" to
/// "steer".
///
/// The run future is pinned and re-polled after the prompt instead of being
/// dropped, so a long multi-step task doesn't throw away everything it has
/// already done just because the user wants to redirect it. Pressing Enter
/// on an empty prompt still aborts -- the historical behaviour is preserved
/// as the deliberate choice rather than the only option.
///
/// While the prompt is awaiting input the run future isn't polled at all, so
/// the turn is genuinely paused and streamed output can't interleave with
/// what the user is typing.
async fn run_steerable(agent: &mut Agent, input: &str) {
    let queue = agent.interjections();
    let mut running = Box::pin(agent.run(input));

    loop {
        tokio::select! {
            result = &mut running => {
                if let Err(e) = result {
                    eprintln!("\nerror: {e:#}");
                }
                return;
            }
            _ = tokio::signal::ctrl_c() => {
                match read_interjection().await {
                    Some(text) => {
                        queue.push(text);
                        println!(
                            "\x1b[36m  ↩ queued — delivered at the next step\x1b[0m"
                        );
                    }
                    None => {
                        // Dropping `running` here is what makes the repair
                        // below necessary; see `repair_after_interrupt`.
                        drop(running);
                        println!("\x1b[33m^C aborted\x1b[0m");
                        if agent.repair_after_interrupt() {
                            println!(
                                "\x1b[90m  (dropped an incomplete tool call from the transcript)\x1b[0m"
                            );
                        }
                        return;
                    }
                }
            }
        }
    }
}

/// Prompt for a mid-turn steer. `None` means abort (empty line, or EOF).
async fn read_interjection() -> Option<String> {
    print!("\n\x1b[36m↩ steer the agent (empty = abort): \x1b[0m");
    let _ = std::io::stdout().flush();
    let line = tokio::task::spawn_blocking(|| {
        let mut buf = String::new();
        // `Ok(0)` is EOF (Ctrl+D) -- an empty string, which reads as abort.
        std::io::stdin().read_line(&mut buf).ok().map(|_| buf)
    })
    .await
    .ok()
    .flatten()?;
    let trimmed = line.trim();
    (!trimmed.is_empty()).then(|| trimmed.to_string())
}

/// One line of `/status`'s change list, or `None` for a file that neither
/// existed before nor exists now (nothing meaningful to say about it).
///
/// Split out from the printing so the classification is testable: the four
/// cases — created, deleted, modified, and *reverted back to its original
/// contents* — are easy to get subtly wrong, and the last one especially
/// matters, since reporting "modified" for a file the agent put back
/// exactly as it found it would send the user hunting for a change that
/// isn't there.
fn change_line(label: &str, before: Option<&str>, now: Option<&str>) -> Option<String> {
    match (before, now) {
        (None, Some(_)) => Some(format!("  \x1b[32m+\x1b[0m {label} \x1b[90m(new)\x1b[0m")),
        (Some(_), None) => Some(format!(
            "  \x1b[31m-\x1b[0m {label} \x1b[90m(deleted)\x1b[0m"
        )),
        (Some(b), Some(n)) => {
            let s = diff::stats(&diff::diff(b, n));
            Some(if s.is_empty() {
                format!("  \x1b[90m·\x1b[0m {label} \x1b[90m(reverted to original)\x1b[0m")
            } else {
                format!(
                    "  \x1b[90m~\x1b[0m {label} \x1b[32m+{}\x1b[0m \x1b[31m-{}\x1b[0m",
                    s.added, s.removed
                )
            })
        }
        (None, None) => None,
    }
}

/// Percentage of the context window in use, and how many cells of a
/// `width`-wide bar that fills. Saturates at 100% rather than overflowing
/// the bar: an over-window request is already handled by the send guard,
/// and a bar longer than its brackets just looks broken.
fn context_gauge(used: u64, window: u64, width: usize) -> (u64, usize) {
    let percent = (used * 100).checked_div(window).unwrap_or(0).min(100);
    (percent, (percent as usize * width) / 100)
}

/// `/status` — one screen answering "where am I and what has this done to
/// my workspace". Deliberately includes the change list: cost and model are
/// easy to remember, but "which files has it touched" is the thing a user
/// actually loses track of during a long session.
fn print_status(agent: &Agent, mode: harness_config::Mode) {
    println!("\x1b[1mmodel\x1b[0m      {}", agent.current_model());
    println!(
        "\x1b[1mmode\x1b[0m       {}",
        match mode {
            harness_config::Mode::Pro => "Pro (hosted reranking for semantic_search)",
            harness_config::Mode::Standard => "Standard (local embeddings)",
        }
    );
    println!(
        "\x1b[1mreasoning\x1b[0m  {}",
        agent.reasoning_effort().unwrap_or("off")
    );
    println!(
        "\x1b[1mweb\x1b[0m        {}",
        if !agent.web_available() {
            "unavailable (hosted sign-in required)"
        } else if agent.web_enabled() {
            "on (3 operations per request)"
        } else {
            "off"
        }
    );
    match agent.budget_usd() {
        Some(b) => println!(
            "\x1b[1mcost\x1b[0m       ${:.6} of ${b:.2} budget",
            agent.session_cost_usd()
        ),
        None => println!(
            "\x1b[1mcost\x1b[0m       ${:.6} (no budget set)",
            agent.session_cost_usd()
        ),
    }
    println!(
        "\x1b[1mturns\x1b[0m      {} · context ~{} of {} tokens",
        agent.turn_count(),
        agent.estimated_tokens(),
        agent.context_window()
    );
    if let Some(id) = agent.session_id() {
        println!("\x1b[1msession\x1b[0m    {id}");
    }

    let changed = agent.changed_files();
    if changed.is_empty() {
        println!("\x1b[1mchanged\x1b[0m    nothing yet");
        return;
    }
    println!("\x1b[1mchanged\x1b[0m    {} file(s):", changed.len());
    for f in &changed {
        let now = std::fs::read_to_string(&f.path).ok();
        if let Some(line) = change_line(&display_path(&f.path), f.before.as_deref(), now.as_deref())
        {
            println!("{line}");
        }
    }
    if agent.change_history_truncated() {
        println!(
            "\x1b[90m  (only the most recent turns are tracked -- earlier changes may be missing)\x1b[0m"
        );
    }
    println!("\x1b[90m  run_shell changes are not tracked; /diff shows the detail\x1b[0m");
}

fn print_web_status(agent: &Agent) {
    if !agent.web_available() {
        println!("web: unavailable — run `hivemind auth login` to use hosted web search");
    } else if agent.web_enabled() {
        println!("web: on (up to 3 search/fetch operations per request)");
    } else {
        println!("web: off (enable with /web on)");
    }
}

/// `/context` — the numbers that decide when trimming, compaction, and the
/// send guard fire, taken from the same estimator those passes use.
fn print_context(agent: &Agent) {
    // A 40-cell bar is readable at any terminal width worth supporting.
    const BAR_WIDTH: usize = 40;
    let used = agent.estimated_tokens();
    let window = agent.context_window();
    let (percent, filled) = context_gauge(used, window, BAR_WIDTH);

    println!(
        "context: ~{used} of {window} tokens ({percent}%) across {} messages",
        agent.history().len()
    );
    println!(
        "  [\x1b[36m{}\x1b[0m{}]",
        "█".repeat(filled),
        "·".repeat(BAR_WIDTH - filled)
    );
    println!(
        "\x1b[90m  estimated, not tokenized -- the harness brokers several providers with\n  \
         different tokenizers, so this deliberately over-counts rather than risk\n  \
         under-counting into a rejected request.\x1b[0m"
    );
    println!(
        "\x1b[90m  old tool results are dropped automatically above ~25k tokens; older turns\n  \
         are folded into a summary near {}% of the window.\x1b[0m",
        agent.compaction_threshold_percent()
    );
}

/// `/diff` — what actually changed on disk this session.
fn print_diff(agent: &Agent, filter: Option<&str>) {
    /// Enough to read a real change without scrolling a whole file past.
    const MAX_LINES_PER_FILE: usize = 120;

    let changed = agent.changed_files();
    let matching: Vec<_> = changed
        .iter()
        .filter(|f| match filter {
            None => true,
            Some(want) => f.path.to_string_lossy().ends_with(want),
        })
        .collect();

    if matching.is_empty() {
        match filter {
            Some(want) if !changed.is_empty() => {
                println!("no changed file matches {want:?} -- /status lists them")
            }
            Some(want) => println!("nothing changed this session (and nothing matches {want:?})"),
            None => println!("nothing changed this session"),
        }
        return;
    }

    for f in matching {
        let before = f.before.clone().unwrap_or_default();
        let Ok(after) = std::fs::read_to_string(&f.path) else {
            println!(
                "\x1b[1m{}\x1b[0m \x1b[31m(deleted, or no longer readable)\x1b[0m",
                display_path(&f.path)
            );
            continue;
        };
        let lines = diff::diff(&before, &after);
        let s = diff::stats(&lines);
        if s.is_empty() {
            continue;
        }
        println!(
            "\n\x1b[1m{}\x1b[0m \x1b[32m+{}\x1b[0m \x1b[31m-{}\x1b[0m{}",
            display_path(&f.path),
            s.added,
            s.removed,
            if f.before.is_none() {
                " \x1b[90m(new file)\x1b[0m"
            } else {
                ""
            }
        );
        print!("{}", diff::render(&lines, MAX_LINES_PER_FILE));
    }

    if agent.change_history_truncated() {
        println!(
            "\n\x1b[90m(only the most recent turns are tracked -- earlier changes may be missing)\x1b[0m"
        );
    }
}

/// Shorten an absolute path against the current directory, so a diff header
/// reads `src/main.rs` rather than 90 characters of machine-specific prefix.
fn display_path(p: &std::path::Path) -> String {
    std::env::current_dir()
        .ok()
        .and_then(|cwd| p.strip_prefix(cwd).ok())
        .unwrap_or(p)
        .display()
        .to_string()
}

#[cfg(test)]
mod system_prompt_tests {
    use super::*;

    #[test]
    fn identity_leads_the_prompt() {
        let p = system_prompt(None, false);
        assert!(p.starts_with("You are HiveMind, a coding agent built by bmtai."));
    }

    #[test]
    fn a_workspace_with_no_conventions_changes_nothing() {
        assert_eq!(
            system_prompt(None, false),
            format!("{IDENTITY}\n\n{SYSTEM_PROMPT_BODY}\n\n{}", platform_note())
        );
    }

    #[test]
    fn the_prompt_names_the_platform_and_the_shell_it_will_get() {
        // A model that is not told this defaults to POSIX, and on Windows
        // every shell call fails until it infers otherwise -- which is what
        // fed the escalation counter that spent a month's budget in one
        // task. The specific strings matter: "terminal" alone is what the
        // prompt said before, and it is not enough.
        let p = system_prompt(None, false);
        assert!(p.contains("Environment: "), "{p}");
        assert!(p.contains("run_shell` executes through"), "{p}");
        if cfg!(windows) {
            assert!(p.contains("cmd /C"));
            assert!(p.contains("do not exist here"));
        } else {
            assert!(p.contains("bash -lc"));
        }
    }

    #[test]
    fn project_conventions_land_last_so_they_win_ties() {
        let p = system_prompt(
            Some("<project-instructions>use just</project-instructions>"),
            false,
        );
        assert!(p.contains("use just"));
        // Specializations have to sit *after* the general rules they
        // override, or the nearest-instruction-wins heuristic works against
        // the project instead of for it.
        let conventions_at = p.find("use just").unwrap();
        let workflow_at = p.find("Prefer tools over guessing").unwrap();
        assert!(conventions_at > workflow_at);
    }

    #[test]
    fn the_final_answer_contract_survives_in_the_prompt() {
        let p = system_prompt(None, false);
        // The "did not verify" clause is the load-bearing half of the
        // contract -- a summary that only lists successes is the failure
        // mode this exists to prevent.
        assert!(p.contains("what you did NOT verify"));
        assert!(p.contains("Scale it to the work"));
    }

    #[test]
    fn read_program_guidance_is_narrow_and_preserves_simple_tools() {
        let p = system_prompt(None, true);
        assert!(p.contains("bounded `search_then_read`"));
        assert!(p.contains("Keep a single simple"));
        assert!(p.contains("`read_program` is read-only"));
    }

    #[test]
    fn hooked_sessions_do_not_advertise_the_composite_tool() {
        let p = system_prompt(None, false);
        assert!(!p.contains("`read_program`"));
        assert!(!p.contains("`search_then_read`"));
    }
}

#[cfg(test)]
mod status_tests {
    use super::*;

    fn plain(s: Option<String>) -> String {
        // Strip SGR sequences so assertions are about content, not colour.
        let s = s.unwrap_or_default();
        let mut out = String::new();
        let mut chars = s.chars();
        while let Some(c) = chars.next() {
            if c == '\x1b' {
                for c in chars.by_ref() {
                    if c == 'm' {
                        break;
                    }
                }
            } else {
                out.push(c);
            }
        }
        out.trim().to_string()
    }

    #[test]
    fn a_file_the_session_created_reads_as_new() {
        assert_eq!(plain(change_line("a.rs", None, Some("x"))), "+ a.rs (new)");
    }

    #[test]
    fn a_file_the_session_deleted_reads_as_deleted() {
        assert_eq!(
            plain(change_line("a.rs", Some("x"), None)),
            "- a.rs (deleted)"
        );
    }

    #[test]
    fn a_modified_file_reports_its_line_counts() {
        assert_eq!(
            plain(change_line(
                "a.rs",
                Some("one\ntwo"),
                Some("one\nTWO\nthree")
            )),
            "~ a.rs +2 -1"
        );
    }

    #[test]
    fn a_file_put_back_exactly_as_found_is_not_reported_as_modified() {
        // The agent edited this and then undid it. Saying "modified" would
        // send the user looking for a change that no longer exists.
        assert_eq!(
            plain(change_line("a.rs", Some("same"), Some("same"))),
            "· a.rs (reverted to original)"
        );
    }

    #[test]
    fn a_file_that_never_existed_either_side_says_nothing() {
        assert!(change_line("a.rs", None, None).is_none());
    }

    #[test]
    fn the_context_gauge_tracks_usage() {
        assert_eq!(context_gauge(0, 1000, 40), (0, 0));
        assert_eq!(context_gauge(500, 1000, 40), (50, 20));
        assert_eq!(context_gauge(1000, 1000, 40), (100, 40));
    }

    #[test]
    fn the_gauge_saturates_rather_than_overflowing_its_bar() {
        // The send guard already stops an over-window request; the bar must
        // not print more cells than it has brackets for.
        let (percent, filled) = context_gauge(5_000, 1_000, 40);
        assert_eq!(percent, 100);
        assert_eq!(filled, 40);
    }

    #[test]
    fn an_unknown_context_window_does_not_divide_by_zero() {
        assert_eq!(context_gauge(1_000, 0, 40), (0, 0));
    }
}
