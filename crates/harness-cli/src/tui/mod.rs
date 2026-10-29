//! Default Ratatui interface for the interactive agent. The plain Reedline
//! interface (`--ui plain`) and JSON protocol remain separate; Crossterm owns
//! input only while this module is active.

mod app;
mod detail;
mod events;
mod listview;
mod markdown;
mod render;

use std::io::{self, Stdout};
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::Once;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use anyhow::Context;
use crossterm::event::{
    self, DisableMouseCapture, EnableMouseCapture, Event, KeyCode, KeyEvent, KeyEventKind,
    KeyModifiers, MouseButton, MouseEventKind,
};
use crossterm::execute;
use crossterm::terminal::{
    EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
};
use harness_agent::Agent;
use harness_tools::Workspace;
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;

use self::app::{App, Panel, View};

pub(crate) use events::TuiBridge;

pub(crate) struct TuiConfig {
    pub(crate) workdir: PathBuf,
    pub(crate) workspace_label: String,
    /// Shown in the header; see [`connection_label`].
    pub(crate) connection: String,
}

/// Where requests go, without paths or secrets: "hosted", the provider's
/// host, or "local · host:port".
pub(crate) fn connection_label(backend: harness_config::Backend, base_url: &str) -> String {
    let host = base_url
        .split_once("://")
        .map_or(base_url, |(_, rest)| rest)
        .split('/')
        .next()
        .unwrap_or_default();
    match backend {
        harness_config::Backend::Hosted => "hosted".to_string(),
        harness_config::Backend::Byok => host.to_string(),
        harness_config::Backend::Local => format!("local · {host}"),
    }
}

/// OSC 52 asks the terminal itself to set the clipboard, so it works over SSH too.
fn osc52(text: &str) -> String {
    format!("\x1b]52;c;{}\x07", base64(text.as_bytes()))
}

fn base64(bytes: &[u8]) -> String {
    const TABLE: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let n = chunk
            .iter()
            .enumerate()
            .fold(0u32, |acc, (i, b)| acc | (u32::from(*b) << (16 - 8 * i)));
        for i in 0..4 {
            out.push(if i <= chunk.len() {
                TABLE[(n >> (18 - 6 * i) & 63) as usize] as char
            } else {
                '='
            });
        }
    }
    out
}

fn copy_last_reply(app: &mut App) {
    use std::io::Write;
    let reply = app.last_reply.trim();
    if reply.is_empty() {
        app.apply(events::UiEvent::Notice("Nothing to copy yet".into()));
        return;
    }
    let chars = reply.chars().count();
    let sent = {
        let mut out = io::stdout();
        out.write_all(osc52(reply).as_bytes())
            .and_then(|()| out.flush())
    };
    app.apply(events::UiEvent::Notice(match sent {
        Ok(()) => format!(
            "Copied the last reply ({chars} characters). Needs a terminal that allows clipboard access (OSC 52)."
        ),
        Err(error) => format!("Could not copy: {error}"),
    }));
}

enum InputEvent {
    Key(KeyEvent),
    Paste(String),
    Wheel { column: u16, row: u16, down: bool },
    Click { column: u16, row: u16 },
    Resize,
}

/// Changed files as the agent first saw them, and as they are now on disk.
struct WorkspaceFiles<'a> {
    bridge: &'a TuiBridge,
    workspace: &'a Workspace,
}

impl WorkspaceFiles<'_> {
    fn absolute(&self, path: &str) -> PathBuf {
        // `resolve` matches how the agent keyed its snapshot; a deleted
        // file's parent may be gone too, hence the plain join fallback.
        self.workspace
            .resolve(path)
            .unwrap_or_else(|_| self.workspace.root.join(path))
    }
}

impl app::FileSource for WorkspaceFiles<'_> {
    fn before(&self, path: &str) -> Option<Option<String>> {
        self.bridge.baseline(&self.absolute(path))
    }

    fn now(&self, path: &str) -> Option<String> {
        std::fs::read_to_string(self.absolute(path)).ok()
    }
}

type TuiTerminal = Terminal<CrosstermBackend<Stdout>>;

static PANIC_HOOK: Once = Once::new();

enum ControllerCommand {
    Run(String),
    Interrupt,
    SetModel(String),
    SetReasoning(Option<String>),
    SetBudget(Option<f64>),
    SetWeb(bool),
    ClearSkill,
    Compact,
    Undo(usize),
    Status,
    Shutdown,
}

/// Run the interactive TUI. The application loop is the only terminal input
/// owner; provider calls and tools continue on Tokio tasks through `Agent`.
pub(crate) async fn run(
    agent: Agent,
    workspace: Workspace,
    bridge: Arc<TuiBridge>,
    config: TuiConfig,
) -> anyhow::Result<()> {
    let sessions = harness_agent::SessionStore::new(harness_config::default_sessions_dir())
        .list_for_workspace(&config.workspace_label);
    let reviews = harness_review::ReviewStore::new(harness_config::default_reviews_dir())
        .list_for_workspace(&config.workdir)
        .unwrap_or_default();
    let mut app = App::new(
        config.workspace_label,
        agent.current_model().to_string(),
        agent.history(),
        sessions,
        reviews,
    );
    app.connection = config.connection;
    let interjections = agent.interjections();
    let (controller_tx, controller_rx) = tokio::sync::mpsc::channel(32);
    let controller = tokio::spawn(agent_controller(agent, bridge.clone(), controller_rx));
    let mut terminal = TerminalSession::enter()?;
    let (input_tx, mut input_rx) = tokio::sync::mpsc::channel(128);
    let input_stop = Arc::new(AtomicBool::new(false));
    let input_task = spawn_input_pump(input_tx, input_stop.clone());
    let mut redraw = true;
    let mut ticks: u64 = 0;
    let mut frame_clock = tokio::time::interval(Duration::from_millis(33));
    frame_clock.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

    let result = async {
        loop {
            for event in bridge.drain() {
                app.apply(event);
                redraw = true;
            }
            app.refresh_file_stats(&WorkspaceFiles {
                bridge: &bridge,
                workspace: &workspace,
            });
            if redraw {
                terminal.draw(|frame| render::draw(frame, &app))?;
                redraw = false;
            }

            tokio::select! {
                _ = frame_clock.tick() => {
                    // Drains coalesced agent events; while running, ~10 redraws a
                    // second keep the spinner and timer alive.
                    ticks += 1;
                    if app.run_state != app::RunState::Idle && ticks.is_multiple_of(3) {
                        redraw = true;
                    }
                }
                maybe_input = input_rx.recv() => {
                    let Some(input) = maybe_input else { break; };
                    let outcome = handle_input(
                        input,
                        &mut app,
                        &bridge,
                        &controller_tx,
                        &workspace,
                        &interjections,
                    ).await;
                    redraw = true;
                    if outcome == InputOutcome::Quit {
                        break;
                    }
                }
            }
        }
        Ok(())
    }
    .await;

    if let Some(request_id) = app.resolve_approval(false) {
        bridge.resolve_approval(&request_id, false);
    }
    let _ = controller_tx.send(ControllerCommand::Shutdown).await;
    let _ = controller.await;
    input_stop.store(true, Ordering::Relaxed);
    let _ = input_task.await;
    result
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum InputOutcome {
    Continue,
    Quit,
}

async fn handle_input(
    input: InputEvent,
    app: &mut App,
    bridge: &TuiBridge,
    controller: &tokio::sync::mpsc::Sender<ControllerCommand>,
    workspace: &Workspace,
    interjections: &harness_agent::InterjectionQueue,
) -> InputOutcome {
    let files = WorkspaceFiles { bridge, workspace };
    match input {
        InputEvent::Resize => return InputOutcome::Continue,
        InputEvent::Wheel { column, row, down } => {
            app.scroll_at(column, row, down);
            return InputOutcome::Continue;
        }
        InputEvent::Click { column, row } => {
            if app.approval.is_none() && !app.palette_visible && app.click_at(column, row) {
                app.open_focused(&files);
            }
            return InputOutcome::Continue;
        }
        InputEvent::Paste(text) => {
            if app.approval.is_none() && !app.palette_visible {
                app.composer.push_str(&text);
            }
            return InputOutcome::Continue;
        }
        InputEvent::Key(key) if key.kind != KeyEventKind::Press => return InputOutcome::Continue,
        InputEvent::Key(key) => {
            if let Some(approval) = app.approval.as_ref() {
                match key.code {
                    KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                        let request_id = approval.request_id.clone();
                        app.resolve_approval(false);
                        bridge.resolve_approval(&request_id, false);
                        let _ = controller.send(ControllerCommand::Interrupt).await;
                    }
                    KeyCode::Char('y') | KeyCode::Char('Y') => {
                        let request_id = approval.request_id.clone();
                        app.resolve_approval(true);
                        bridge.resolve_approval(&request_id, true);
                    }
                    KeyCode::Char('n') | KeyCode::Char('N') | KeyCode::Esc => {
                        let request_id = approval.request_id.clone();
                        app.resolve_approval(false);
                        bridge.resolve_approval(&request_id, false);
                    }
                    _ => {}
                }
                return InputOutcome::Continue;
            }

            if app.palette_visible {
                match key.code {
                    KeyCode::Esc => app.palette_visible = false,
                    KeyCode::Char('p') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                        app.palette_visible = false;
                    }
                    KeyCode::F(1) => {
                        app.view = View::Work;
                        app.palette_visible = false;
                    }
                    KeyCode::F(2) => {
                        app.view = View::Sessions;
                        app.palette_visible = false;
                    }
                    KeyCode::F(3) => {
                        app.view = View::Reviews;
                        app.palette_visible = false;
                    }
                    _ => {}
                }
                return InputOutcome::Continue;
            }

            let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
            if app.detail.is_some() {
                match key.code {
                    KeyCode::Esc | KeyCode::Char('q') => app.detail = None,
                    KeyCode::Up => app.scroll_detail(-1),
                    KeyCode::Down => app.scroll_detail(1),
                    KeyCode::PageUp => app.page_detail(false),
                    KeyCode::PageDown => app.page_detail(true),
                    KeyCode::Home => app.scroll_detail(isize::MIN / 2),
                    KeyCode::End => app.scroll_detail(isize::MAX / 2),
                    KeyCode::Left | KeyCode::BackTab => app.step_detail(false, &files),
                    KeyCode::Right | KeyCode::Tab => app.step_detail(true, &files),
                    // Ctrl+C still stops a run: close the view and fall through.
                    KeyCode::Char('c') if ctrl => app.detail = None,
                    _ => {}
                }
                if !(ctrl && key.code == KeyCode::Char('c')) {
                    return InputOutcome::Continue;
                }
            }

            if ctrl {
                match key.code {
                    KeyCode::Char('c') => {
                        if app.run_state != app::RunState::Idle {
                            let _ = controller.send(ControllerCommand::Interrupt).await;
                        } else {
                            app.composer.clear();
                        }
                    }
                    KeyCode::Char('d') | KeyCode::Char('q') => return InputOutcome::Quit,
                    KeyCode::Char('l') => app.clear_local_conversation(),
                    KeyCode::Char('s') => app.sidebar_visible = !app.sidebar_visible,
                    KeyCode::Char('p') => app.palette_visible = true,
                    KeyCode::Char('o') => app.open_transcript(),
                    _ => {}
                }
                return InputOutcome::Continue;
            }

            match key.code {
                KeyCode::F(1) => app.view = View::Work,
                KeyCode::F(2) => app.view = View::Sessions,
                KeyCode::F(3) => app.view = View::Reviews,
                KeyCode::Tab | KeyCode::BackTab => {
                    // While typing a command name, Tab completes it.
                    if let Some((name, _)) = app::slash_matches(&app.composer).first() {
                        app.composer = format!("{name} ");
                    } else {
                        let backwards = key.code == KeyCode::BackTab
                            || key.modifiers.contains(KeyModifiers::SHIFT);
                        app.cycle_focus(backwards);
                    }
                }
                // Leave a side panel first; from the conversation, stop the run.
                KeyCode::Esc if app.focus != Panel::Conversation => app.focus = Panel::Conversation,
                KeyCode::Esc if app.run_state != app::RunState::Idle => {
                    let _ = controller.send(ControllerCommand::Interrupt).await;
                }
                KeyCode::Char('?') if app.composer.is_empty() => app.open_help(),
                KeyCode::Up => app.step_focused(-1),
                KeyCode::Down => app.step_focused(1),
                KeyCode::PageUp => app.page_focused(false),
                KeyCode::PageDown => app.page_focused(true),
                KeyCode::Home => app.jump_focused(false),
                KeyCode::End => app.jump_focused(true),
                KeyCode::Backspace => {
                    app.composer.pop();
                }
                KeyCode::Enter if key.modifiers.contains(KeyModifiers::SHIFT) => {
                    app.composer.push('\n')
                }
                KeyCode::Enter => {
                    let text = app.composer.trim().to_string();
                    if text.is_empty() {
                        app.open_focused(&files);
                        return InputOutcome::Continue;
                    }
                    if app.run_state != app::RunState::Idle {
                        app.submitted(&text);
                        interjections.push(text);
                        return InputOutcome::Continue;
                    }
                    if text.starts_with('/') {
                        if handle_command(app, controller, &files, &text).await
                            == InputOutcome::Quit
                        {
                            return InputOutcome::Quit;
                        }
                    } else {
                        app.submitted(&text);
                        let expanded = crate::mentions::expand_mentions(&text, workspace);
                        app.run_state = app::RunState::Working;
                        let _ = controller.send(ControllerCommand::Run(expanded)).await;
                    }
                }
                KeyCode::Char(character) => {
                    // Typing means writing a message, so arrows return to the conversation.
                    app.focus = Panel::Conversation;
                    app.composer.push(character);
                }
                _ => {}
            }
        }
    }
    InputOutcome::Continue
}

async fn handle_command(
    app: &mut App,
    controller: &tokio::sync::mpsc::Sender<ControllerCommand>,
    files: &dyn app::FileSource,
    text: &str,
) -> InputOutcome {
    let mut parts = text.split_whitespace();
    let command = parts.next().unwrap_or_default();
    let argument = parts.next();
    match command {
        "/help" => app.open_help(),
        "/diff" => app.open_all_changes(files),
        "/transcript" => app.open_transcript(),
        "/copy" => copy_last_reply(app),
        "/work" => app.view = View::Work,
        "/sessions" => app.view = View::Sessions,
        "/reviews" => app.view = View::Reviews,
        "/model" => match argument {
            Some(model) => {
                app.model = model.to_string();
                let _ = controller
                    .send(ControllerCommand::SetModel(model.to_string()))
                    .await;
            }
            None => app.apply(events::UiEvent::Notice(format!(
                "Active model: {}",
                app.model
            ))),
        },
        "/reasoning" => match argument {
            Some("off") => {
                let _ = controller.send(ControllerCommand::SetReasoning(None)).await;
            }
            Some(level) => {
                let _ = controller
                    .send(ControllerCommand::SetReasoning(Some(level.to_string())))
                    .await;
            }
            None => app.apply(events::UiEvent::Notice("Set a level or use off".into())),
        },
        "/budget" => match argument {
            Some("off") => {
                let _ = controller.send(ControllerCommand::SetBudget(None)).await;
            }
            Some(value) => match value.parse::<f64>() {
                Ok(value) if value > 0.0 => {
                    let _ = controller
                        .send(ControllerCommand::SetBudget(Some(value)))
                        .await;
                }
                _ => app.apply(events::UiEvent::Notice(
                    "Budget must be a positive number".into(),
                )),
            },
            None => app.apply(events::UiEvent::Notice("Set an amount or use off".into())),
        },
        "/web" => match argument {
            Some("on") => {
                let _ = controller.send(ControllerCommand::SetWeb(true)).await;
            }
            Some("off") => {
                let _ = controller.send(ControllerCommand::SetWeb(false)).await;
            }
            _ => app.apply(events::UiEvent::Notice("Use /web on or /web off".into())),
        },
        "/skill" if argument == Some("off") => {
            let _ = controller.send(ControllerCommand::ClearSkill).await;
        }
        "/compact" => {
            let _ = controller.send(ControllerCommand::Compact).await;
        }
        "/undo" => {
            let count = argument.and_then(|value| value.parse().ok()).unwrap_or(1);
            let _ = controller.send(ControllerCommand::Undo(count)).await;
        }
        "/cost" | "/status" => {
            let _ = controller.send(ControllerCommand::Status).await;
        }
        "/clear" => app.clear_local_conversation(),
        "/exit" => return InputOutcome::Quit,
        _ => app.apply(events::UiEvent::Notice(format!(
            "Unknown command {command} -- type / to see the list"
        ))),
    }
    app.composer.clear();
    InputOutcome::Continue
}

async fn agent_controller(
    mut agent: Agent,
    bridge: Arc<TuiBridge>,
    mut commands: tokio::sync::mpsc::Receiver<ControllerCommand>,
) {
    'controller: while let Some(command) = commands.recv().await {
        match command {
            ControllerCommand::Run(input) => {
                let mut request = Box::pin(agent.run(&input));
                loop {
                    tokio::select! {
                        result = &mut request => {
                            bridge.run_finished(result);
                            break;
                        }
                        next = commands.recv() => match next {
                            Some(ControllerCommand::Interrupt) => {
                                drop(request);
                                let repaired = agent.repair_after_interrupt();
                                bridge.stopped(if repaired {
                                    "Interrupted and removed an incomplete tool call from the session"
                                } else {
                                    "Interrupted"
                                });
                                break;
                            }
                            Some(ControllerCommand::Shutdown) | None => {
                                drop(request);
                                let _ = agent.repair_after_interrupt();
                                break 'controller;
                            }
                            Some(_) => bridge.notice("That command is available after the current run finishes"),
                        }
                    }
                }
            }
            ControllerCommand::Interrupt => bridge.notice("Nothing is running"),
            ControllerCommand::SetModel(model) => {
                agent.set_model(model.clone());
                bridge.model_changed(model.clone());
                bridge.notice(format!("Model set to {model}"));
            }
            ControllerCommand::SetReasoning(level) => match level {
                None => {
                    agent.set_reasoning_effort(None);
                    bridge.notice("Reasoning effort disabled");
                }
                Some(level)
                    if agent
                        .reasoning_efforts_for_current_model()
                        .contains(&level.as_str()) =>
                {
                    agent.set_reasoning_effort(Some(level.clone()));
                    bridge.notice(format!("Reasoning effort set to {level}"));
                }
                Some(level) => bridge.notice(format!(
                    "{level} is not available for {}",
                    agent.current_model()
                )),
            },
            ControllerCommand::SetBudget(budget) => {
                agent.set_budget_usd(budget);
                match budget {
                    Some(value) => bridge.notice(format!("Budget set to ${value:.2}")),
                    None => bridge.notice("Budget disabled"),
                }
            }
            ControllerCommand::SetWeb(enabled) => match agent.set_web_enabled(enabled) {
                Ok(()) => bridge.notice(if enabled {
                    "Web access enabled"
                } else {
                    "Web access disabled"
                }),
                Err(error) => bridge.notice(error),
            },
            ControllerCommand::ClearSkill => match agent.set_skill(None) {
                Ok(()) => bridge.notice("Skill cleared"),
                Err(error) => bridge.notice(error),
            },
            ControllerCommand::Compact => {
                if !agent.force_compact().await {
                    bridge.notice("Nothing to compact yet");
                }
            }
            ControllerCommand::Undo(count) => match agent.undo(count).await {
                Some(report) => bridge.notice(format!(
                    "Undid {} turn(s): {} file(s) restored, {} removed",
                    report.turns_undone, report.files_restored, report.files_removed
                )),
                None => bridge.notice("Nothing to undo yet"),
            },
            ControllerCommand::Status => bridge.notice(format!(
                "{} turns · {} · web {} · ${:.6}",
                agent.turn_count(),
                agent.current_model(),
                if agent.web_enabled() { "on" } else { "off" },
                agent.session_cost_usd()
            )),
            ControllerCommand::Shutdown => break,
        }
    }
}

fn spawn_input_pump(
    sender: tokio::sync::mpsc::Sender<InputEvent>,
    stop: Arc<AtomicBool>,
) -> tokio::task::JoinHandle<()> {
    tokio::task::spawn_blocking(move || {
        while !stop.load(Ordering::Relaxed) {
            match event::poll(Duration::from_millis(50)) {
                Ok(false) => continue,
                Err(_) => break,
                Ok(true) => match event::read() {
                    Ok(Event::Key(key)) => {
                        if sender.blocking_send(InputEvent::Key(key)).is_err() {
                            break;
                        }
                    }
                    Ok(Event::Paste(text)) => {
                        if sender.blocking_send(InputEvent::Paste(text)).is_err() {
                            break;
                        }
                    }
                    // Wheel and left click only; pointer moves would flood the queue.
                    Ok(Event::Mouse(mouse)) => {
                        let (column, row) = (mouse.column, mouse.row);
                        let input = match mouse.kind {
                            MouseEventKind::ScrollDown => InputEvent::Wheel {
                                column,
                                row,
                                down: true,
                            },
                            MouseEventKind::ScrollUp => InputEvent::Wheel {
                                column,
                                row,
                                down: false,
                            },
                            MouseEventKind::Down(MouseButton::Left) => {
                                InputEvent::Click { column, row }
                            }
                            _ => continue,
                        };
                        if sender.blocking_send(input).is_err() {
                            break;
                        }
                    }
                    Ok(Event::Resize(_, _)) => {
                        if sender.blocking_send(InputEvent::Resize).is_err() {
                            break;
                        }
                    }
                    Ok(_) => {}
                    Err(_) => break,
                },
            }
        }
    })
}

struct TerminalSession {
    terminal: TuiTerminal,
}

impl TerminalSession {
    fn enter() -> anyhow::Result<Self> {
        install_panic_restore();
        enable_raw_mode().context("enable terminal raw mode")?;
        let mut stdout = io::stdout();
        if let Err(error) = execute!(
            stdout,
            EnterAlternateScreen,
            crossterm::event::EnableBracketedPaste
        ) {
            let _ = disable_raw_mode();
            return Err(error).context("enter alternate terminal screen");
        }
        // Best effort: without it the wheel simply does nothing.
        let _ = execute!(stdout, EnableMouseCapture);
        let backend = CrosstermBackend::new(stdout);
        let mut terminal = Terminal::new(backend).context("create terminal renderer")?;
        terminal.clear().context("clear terminal renderer")?;
        Ok(Self { terminal })
    }

    fn draw(&mut self, render: impl FnOnce(&mut ratatui::Frame)) -> anyhow::Result<()> {
        self.terminal.draw(render).context("draw terminal frame")?;
        Ok(())
    }
}

impl Drop for TerminalSession {
    fn drop(&mut self) {
        let _ = execute!(self.terminal.backend_mut(), DisableMouseCapture);
        let _ = disable_raw_mode();
        let _ = cleanup_terminal_output(self.terminal.backend_mut());
        let _ = self.terminal.show_cursor();
    }
}

fn install_panic_restore() {
    PANIC_HOOK.call_once(|| {
        let previous = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |info| {
            restore_terminal();
            previous(info);
        }));
    });
}

fn restore_terminal() {
    let mut stdout = io::stdout();
    let _ = execute!(stdout, DisableMouseCapture);
    let _ = disable_raw_mode();
    let _ = cleanup_terminal_output(&mut stdout);
}

fn cleanup_terminal_output(writer: &mut impl io::Write) -> io::Result<()> {
    execute!(
        writer,
        crossterm::event::DisableBracketedPaste,
        LeaveAlternateScreen
    )
}

#[cfg(test)]
mod lifecycle_tests {
    use super::*;

    #[test]
    fn base64_matches_the_standard_alphabet_and_padding() {
        assert_eq!(base64(b""), "");
        assert_eq!(base64(b"f"), "Zg==");
        assert_eq!(base64(b"fo"), "Zm8=");
        assert_eq!(base64(b"foo"), "Zm9v");
        assert_eq!(base64("नमस्ते".as_bytes()), "4KSo4KSu4KS44KWN4KSk4KWH");
    }

    #[test]
    fn osc52_wraps_the_text_for_the_terminal_clipboard() {
        assert_eq!(osc52("hi"), "\x1b]52;c;aGk=\x07");
    }

    #[test]
    fn the_connection_label_names_the_destination_without_paths_or_secrets() {
        use harness_config::Backend;
        assert_eq!(
            connection_label(Backend::Hosted, "https://api.hivemind.bmtai.in"),
            "hosted"
        );
        assert_eq!(
            connection_label(Backend::Byok, "https://openrouter.ai/api/v1"),
            "openrouter.ai"
        );
        assert_eq!(
            connection_label(Backend::Local, "http://127.0.0.1:11434/v1"),
            "local · 127.0.0.1:11434"
        );
    }

    #[test]
    fn cleanup_writes_the_commands_that_restore_the_normal_terminal_screen() {
        let mut output = Vec::new();
        cleanup_terminal_output(&mut output).unwrap();
        let output = String::from_utf8(output).unwrap();
        assert!(
            output.contains("[?2004l"),
            "paste mode was not disabled: {output:?}"
        );
        assert!(
            output.contains("[?1049l"),
            "alternate screen was not left: {output:?}"
        );
    }
}
