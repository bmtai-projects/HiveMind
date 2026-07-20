//! The startup banner: a small hexagon mark (honeycomb cluster — "hive")
//! plus the wordmark, nothing else. Deliberately not literal ASCII-art
//! outline reproduction of the logo — exact multi-line silhouettes are easy
//! to misalign without live rendering to check against, and a scattered
//! cluster degrades gracefully (a stray space just looks like a cluster,
//! not a broken shape) where a precise outline wouldn't.
//!
//! No session info here on purpose (tier/model/workdir/tools) — that used
//! to print every boot and it was just noise; `/tier` shows the active
//! tier on demand instead.

const CYAN: &str = "\x1b[96m";
const BOLD_CYAN: &str = "\x1b[1;96m";
const RESET: &str = "\x1b[0m";

pub fn print() {
    println!();
    println!("{CYAN}         ⬡ ⬡ ⬡{RESET}");
    println!("{CYAN}        ⬡ ⬡ ⬡ ⬡{RESET}");
    println!("{CYAN}         ⬡ ⬡ ⬡{RESET}");
    println!();
    println!("{BOLD_CYAN}      H I V E M I N D{RESET}");
    println!();
}
