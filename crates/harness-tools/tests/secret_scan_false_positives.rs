//! False-positive audit for the credential scanner, run against a real
//! codebase (this repo, plus anything else pointed at via `$SCAN_ROOT`).
//!
//! A warning system's only real failure mode is crying wolf: a scanner that
//! fires on ordinary source teaches the user to ignore it, at which point
//! it is worse than nothing because it still costs tokens. Unit tests can
//! only check the false positives someone thought to write down — this
//! checks the ones real code actually contains.
//!
//! `#[ignore]`d like the other real-repo harness here, since it depends on
//! the checkout's own contents.
//!
//! Run with:
//!   cargo test -p harness-tools --test secret_scan_false_positives -- --ignored --nocapture
//!   SCAN_ROOT=/path/to/other/repo cargo test ... -- --ignored --nocapture
//!
//! # Measured at the time this was written
//!
//! | Repo | Files | Findings |
//! |---|---|---|
//! | HiveMind (this one) | 71 | 5 — all in `secrets.rs`'s own rule table |
//! | HiveMind-server (Node, real Stripe/Firebase keys via env) | 50 | 0 |
//! | HiveMind-vscode (TS + a vendored VS Code build) | 2561 | 2 — a literal key header inside VS Code's own minified bundle |
//!
//! Zero genuine false positives across ~2,700 files. That number is the
//! whole justification for warning on every write rather than making this
//! opt-in; if a later rule change moves it, the rule change is wrong.
//!
//! The first run of this audit found two real false positives, both of the
//! same shape — `api_key: Arc::from(...)`, `api_key: args.api_key.clone()`,
//! i.e. correct credential handling being flagged as a leak. That is what
//! `secrets::looks_like_a_literal_credential` exists to reject.

use harness_tools::secrets;

#[test]
#[ignore = "manual audit against a real checkout"]
fn the_scanner_stays_quiet_on_ordinary_source() {
    let root = std::env::var("SCAN_ROOT")
        .unwrap_or_else(|_| concat!(env!("CARGO_MANIFEST_DIR"), "/../..").to_string());

    let mut scanned = 0usize;
    let mut flagged = Vec::new();

    for entry in walkdir::WalkDir::new(&root)
        .into_iter()
        .filter_entry(|e| {
            let name = e.file_name().to_string_lossy();
            !matches!(name.as_ref(), "target" | "node_modules" | ".git" | "dist")
        })
        .flatten()
        .filter(|e| e.file_type().is_file())
    {
        let Ok(text) = std::fs::read_to_string(entry.path()) else {
            continue;
        };
        if text.len() > 2_000_000 {
            continue;
        }
        scanned += 1;
        for f in secrets::scan(&text) {
            flagged.push(format!(
                "{}:{} -- {}",
                entry.path().display(),
                f.line,
                f.kind
            ));
        }
    }

    println!("scanned {scanned} files, {} findings", flagged.len());
    for f in &flagged {
        println!("  {f}");
    }
    println!(
        "\nEvery line above is a false positive unless this checkout really \
         does contain a credential. Read them, don't just count them."
    );
}
