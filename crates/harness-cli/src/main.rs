//! `hivemind` — a fast, cost-optimized AI coding agent.
//!
//! "hivemind" (a branded DeepSeek alias) is the cheap default model; six
//! real third-party coding models are selectable alongside it in hosted
//! mode via `--model`/`/model`, and the agent escalates off "hivemind"
//! automatically when it looks stuck. See the workspace README for the
//! full architecture and optimization notes.

mod auth;
mod banner;
mod commands;
mod completion;
mod input;
mod json_ui;
mod mentions;
mod self_update;
mod ui;
mod update_check;

use std::io::Write;
use std::path::PathBuf;
use std::sync::Arc;

use clap::{Args, Parser, Subcommand};
use reedline::Signal;

use commands::{BudgetArg, ModelArg, ReasoningArg, SlashCommand, UndoArg};
use harness_agent::{Agent, Ui};
use harness_config::CliOverrides;
use harness_tools::{
    Bash, CreatePdf, CreateSpreadsheet, EditFile, ListDir, ProjectMap, ReadFile, Registry, Search,
    SemanticSearch, TodoWrite, Workspace, WriteFile,
};
use input::HivePrompt;
use json_ui::JsonUi;
use ui::TermUi;

const SYSTEM_PROMPT: &str =
    "You are a terminal-based coding agent operating inside a user's workspace.

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
// Parsed once at startup; the variant size gap costs nothing here.
#[allow(clippy::large_enum_variant)]
enum Command {
    /// Start the agent — interactive REPL, or headless with --prompt.
    Activate(ActivateArgs),
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
}

#[derive(Args)]
struct SessionsArgs {
    /// Workspace whose sessions to list (default: current directory).
    #[arg(long, default_value = ".")]
    workdir: PathBuf,
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
    let sessions = store.list_for_workspace(&workspace);

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

    let mut registry = Registry::new();
    let ws = Workspace::new(workdir.clone());
    registry.register(Arc::new(ReadFile(ws.clone())));
    registry.register(Arc::new(WriteFile(ws.clone())));
    registry.register(Arc::new(EditFile(ws.clone())));
    registry.register(Arc::new(ListDir(ws.clone())));
    registry.register(Arc::new(ProjectMap(ws.clone())));
    registry.register(Arc::new(Search(ws.clone())));
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
    registry.register(Arc::new(TodoWrite));
    registry.register(Arc::new(CreatePdf(ws.clone())));
    registry.register(Arc::new(CreateSpreadsheet(ws.clone())));

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
            SYSTEM_PROMPT.to_string(),
        );
        agent.warm_connection();
        // Protocol mode persists too: an editor window reloading is exactly
        // the kind of ordinary interruption a session must survive.
        attach_or_restore_session(&mut agent, &args, store, workspace, false)?;
        return run_json_protocol(&mut agent, ws, json_ui).await;
    }

    let ui = term_ui.expect("interactive mode always constructs a TermUi above");
    let mut agent = Agent::new(
        resolved.clone(),
        registry,
        ws.clone(),
        ui.clone(),
        SYSTEM_PROMPT.to_string(),
    );
    agent.warm_connection();

    if let Some(prompt) = &args.prompt {
        // A one-shot `-p` run has nothing worth resuming later, so it stays
        // unpersisted -- no session file, no clutter in `hivemind sessions`.
        let expanded = mentions::expand_mentions(prompt, &ws);
        agent.run(&expanded).await?;
        return Ok(());
    }

    attach_or_restore_session(&mut agent, &args, store, workspace, true)?;

    repl(&mut agent, ws, ui, args.yolo, update_check).await
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
            agent.restore(record, store, SYSTEM_PROMPT.to_string());
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

    ui.emit_ready();

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
