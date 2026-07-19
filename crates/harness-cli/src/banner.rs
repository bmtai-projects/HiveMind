//! The startup banner. Deliberately not full ASCII-art block lettering —
//! that needs exact-width alignment to avoid looking broken across
//! terminals, and a left-bordered card (no right border to close) sidesteps
//! that risk entirely while still reading as a proper "welcome" screen.

use std::path::Path;

use harness_config::Resolved;

pub fn print(resolved: &Resolved, workdir: &Path, tools: &str) {
    println!("\x1b[36m┌─ HiveMind ─────────────────────────────────\x1b[0m");
    println!(
        "\x1b[36m│\x1b[0m DeepSeek coding agent · v{}",
        env!("CARGO_PKG_VERSION")
    );
    println!(
        "\x1b[36m│\x1b[0m tier={} · flash={} · pro={}",
        resolved.policy.default_tier, resolved.flash.wire_id, resolved.pro.wire_id
    );
    println!("\x1b[36m│\x1b[0m workdir={}", workdir.display());
    println!("\x1b[36m│\x1b[0m tools=[{tools}]");
    println!("\x1b[36m└─────────────────────────────────────────────\x1b[0m");
}
