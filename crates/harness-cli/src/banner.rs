//! The startup banner: a bordered box (title/version, a one-line tip, and
//! the handful of commands worth knowing up front), plus a real
//! GitHub-backed update check printed underneath it. Printed once at REPL
//! start, not re-rendered -- unlike the model/approval-mode status, which
//! lives next to the prompt itself (see `crate::input::HivePrompt`) since
//! it can change mid-session.

use crate::update_check;

const CYAN: &str = "\x1b[96m";
const BOLD: &str = "\x1b[1m";
const DIM: &str = "\x1b[90m";
const YELLOW: &str = "\x1b[33m";
const RESET: &str = "\x1b[0m";

/// Width between the two vertical borders, not counting the borders
/// themselves or their single-space padding. Fixed rather than
/// terminal-width-detected -- comfortably fits an 80-column terminal, and a
/// fixed box is simpler to keep correctly aligned than one that reflows.
const INNER_WIDTH: usize = 58;

/// One bordered line: `plain` is padded to `INNER_WIDTH` based on its own
/// character count (ANSI color codes embedded in `plain` are not counted,
/// since a caller only ever wraps *already width-accounted-for* text in
/// color -- see the call sites below), so the single place that can
/// misalign the box is here, not every call site.
fn boxed(plain: &str) -> String {
    let visible_len = plain
        .replace(BOLD, "")
        .replace(DIM, "")
        .replace(RESET, "")
        .chars()
        .count();
    let pad = INNER_WIDTH.saturating_sub(visible_len);
    format!("{CYAN}│{RESET} {plain}{} {CYAN}│{RESET}", " ".repeat(pad))
}

fn boxed_blank() -> String {
    boxed("")
}

/// Async because the update check is a real (short-timeout) network call --
/// called once from the REPL's async startup path, never from a sync
/// context.
pub async fn print() {
    let top = format!("{CYAN}╭{}╮{RESET}", "─".repeat(INNER_WIDTH + 2));
    let bottom = format!("{CYAN}╰{}╯{RESET}", "─".repeat(INNER_WIDTH + 2));

    println!();
    println!("{top}");
    println!(
        "{}",
        boxed(&format!(
            "{BOLD}⬡ HiveMind{RESET}  {DIM}v{}{RESET}",
            env!("CARGO_PKG_VERSION")
        ))
    );
    println!("{}", boxed_blank());
    println!(
        "{}",
        boxed("hivemind (cheap default) + 6 real coding models --")
    );
    println!(
        "{}",
        boxed("/model to switch, `hivemind models` to list them.")
    );
    println!("{}", boxed_blank());
    println!(
        "{}",
        boxed(&format!(
            "{DIM}/help{RESET}   commands        {DIM}/model{RESET}  switch model"
        ))
    );
    println!(
        "{}",
        boxed(&format!(
            "{DIM}/undo{RESET}   undo last turn   {DIM}Ctrl-D{RESET}  quit"
        ))
    );
    println!("{bottom}");

    if let Some(latest) = update_check::newer_version_available().await {
        println!("{YELLOW}Update: v{latest} available -- re-run the installer to upgrade{RESET}");
    }
    println!();
}
