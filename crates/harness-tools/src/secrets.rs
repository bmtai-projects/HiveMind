//! Flags credentials in content the agent is about to write.
//!
//! # Why warn instead of block
//!
//! Blocking would be wrong more often than it is right. Test fixtures,
//! documentation, and example configs legitimately contain things that look
//! exactly like credentials, and a tool that refuses to write them turns a
//! safety feature into an obstacle the model has to work around — usually
//! by doing something worse, like base64-ing the value to get past the
//! check. A warning attached to the tool result puts the fact in front of
//! both the model and the user without taking the decision away from them.
//!
//! Anyone who genuinely wants a hard stop now has one: a `PreToolUse` hook
//! with `enforcement = true` can refuse the write outright.
//!
//! # Why hand-rolled instead of `regex`
//!
//! Every pattern here is a literal prefix followed by a run of characters
//! from a fixed class, which is a few lines of `char` matching. Pulling in
//! `regex` for that would add well over a megabyte to a binary this project
//! cross-compiles to five targets and has already gone out of its way to
//! keep lean (see `printpdf`'s `default-features = false` in Cargo.toml).
//!
//! # Precision over recall
//!
//! A warning that cries wolf gets ignored, and an ignored warning is worse
//! than none because it also costs tokens. Every rule here is anchored on a
//! vendor-specific prefix; the one general rule (`secret = "..."`) is gated
//! behind both a length floor and a Shannon-entropy floor, and placeholder
//! values are filtered explicitly. Missing a novel credential format is an
//! accepted cost of not flagging `API_KEY=your_key_here`.

/// One credential-shaped thing found in written content.
#[derive(Debug, PartialEq, Eq)]
pub struct Finding {
    /// 1-based, counted the way an editor counts.
    pub line: usize,
    /// Human-readable name of what matched, e.g. "AWS access key ID".
    pub kind: &'static str,
}

/// Cap on reported findings. A file that trips a hundred rules says the
/// same thing as one that trips five, at twenty times the token cost.
const MAX_FINDINGS: usize = 5;

/// Minimum length for the value in a generic `secret = "..."` assignment.
/// Real API keys are comfortably longer; most placeholders are shorter.
const MIN_GENERIC_VALUE: usize = 20;

/// Shannon entropy floor, in bits per character, for that same generic
/// rule. Random base62 sits near 5.95; English prose and repeated
/// placeholder text sit well under 3.
const MIN_ENTROPY_BITS: f64 = 3.0;

type CharPred = fn(char) -> bool;

fn alnum(c: char) -> bool {
    c.is_ascii_alphanumeric()
}
fn alnum_dash(c: char) -> bool {
    c.is_ascii_alphanumeric() || c == '-' || c == '_'
}
fn upper_num(c: char) -> bool {
    c.is_ascii_uppercase() || c.is_ascii_digit()
}

/// `(kind, prefix, minimum characters after the prefix, allowed class)`.
///
/// Ordered longest/most-specific prefix first where two could both match,
/// so the reported `kind` is the more informative one.
const PREFIX_RULES: &[(&str, &str, usize, CharPred)] = &[
    ("AWS access key ID", "AKIA", 16, upper_num),
    ("AWS access key ID", "ASIA", 16, upper_num),
    ("GitHub personal access token", "ghp_", 36, alnum),
    ("GitHub OAuth token", "gho_", 36, alnum),
    ("GitHub user-to-server token", "ghu_", 36, alnum),
    ("GitHub server-to-server token", "ghs_", 36, alnum),
    ("GitHub refresh token", "ghr_", 36, alnum),
    ("GitHub fine-grained token", "github_pat_", 20, alnum_dash),
    ("Anthropic API key", "sk-ant-", 20, alnum_dash),
    ("Stripe live secret key", "sk_live_", 20, alnum),
    ("Stripe live restricted key", "rk_live_", 20, alnum),
    ("OpenAI-style API key", "sk-", 20, alnum_dash),
    ("Google API key", "AIza", 35, alnum_dash),
    ("Slack token", "xoxb-", 20, alnum_dash),
    ("Slack token", "xoxp-", 20, alnum_dash),
    ("Slack token", "xoxa-", 20, alnum_dash),
    ("Slack token", "xoxr-", 20, alnum_dash),
    (
        "Slack webhook URL",
        "https://hooks.slack.com/services/",
        20,
        alnum_dash,
    ),
    ("SendGrid API key", "SG.", 20, alnum_dash),
    ("npm access token", "npm_", 36, alnum),
];

/// Substrings that are themselves conclusive, no charset run needed.
const LITERAL_RULES: &[(&str, &str)] = &[
    ("private key block", "-----BEGIN RSA PRIVATE KEY-----"),
    ("private key block", "-----BEGIN EC PRIVATE KEY-----"),
    ("private key block", "-----BEGIN DSA PRIVATE KEY-----"),
    ("private key block", "-----BEGIN OPENSSH PRIVATE KEY-----"),
    ("private key block", "-----BEGIN PGP PRIVATE KEY BLOCK-----"),
    ("private key block", "-----BEGIN PRIVATE KEY-----"),
];

/// Names that make the value after `=` or `:` credential-shaped.
const SECRET_NAMES: &[&str] = &[
    "api_key",
    "apikey",
    "api-key",
    "secret_key",
    "secretkey",
    "secret",
    "password",
    "passwd",
    "access_token",
    "accesstoken",
    "auth_token",
    "authtoken",
    "client_secret",
    "clientsecret",
    "private_key",
    "privatekey",
    "credential",
];

/// Values that look like credentials but are obviously stand-ins. Matched
/// case-insensitively against the whole value.
const PLACEHOLDER_MARKERS: &[&str] = &[
    "your",
    "example",
    "placeholder",
    "changeme",
    "change_me",
    "dummy",
    "sample",
    "insert",
    "replace",
    "todo",
    "xxxx",
    "....",
    "<",
    "${",
    "process.env",
    "os.environ",
    "getenv",
    "redacted",
    "hidden",
    "test_key",
    "fake",
];

/// Scan `content`, returning at most [`MAX_FINDINGS`] findings.
///
/// Line-oriented, single pass, no allocation per line beyond the lowercase
/// copy the generic rule needs — cheap enough to run on every write.
pub fn scan(content: &str) -> Vec<Finding> {
    let mut out = Vec::new();
    for (i, line) in content.lines().enumerate() {
        if out.len() >= MAX_FINDINGS {
            break;
        }
        if let Some(kind) = scan_line(line) {
            out.push(Finding { line: i + 1, kind });
        }
    }
    out
}

/// At most one finding per line: a line that trips two rules is still one
/// place for a human to look.
fn scan_line(line: &str) -> Option<&'static str> {
    for (kind, needle) in LITERAL_RULES {
        if line.contains(needle) {
            return Some(kind);
        }
    }
    for (kind, prefix, min_len, pred) in PREFIX_RULES {
        if has_prefixed_token(line, prefix, *min_len, *pred) {
            return Some(kind);
        }
    }
    generic_assignment(line)
}

/// True when `line` contains `prefix` followed by at least `min_len`
/// characters that all satisfy `pred`.
fn has_prefixed_token(line: &str, prefix: &str, min_len: usize, pred: CharPred) -> bool {
    let mut rest = line;
    while let Some(at) = rest.find(prefix) {
        let after = &rest[at + prefix.len()..];
        let run = after.chars().take_while(|c| pred(*c)).count();
        if run >= min_len && !is_masked_token(&after[..after.len().min(run)]) {
            return true;
        }
        // Overlapping occurrences matter: `sk-` appears inside `sk-ant-`,
        // and a false start must not hide a real match later in the line.
        rest = &rest[at + prefix.len()..];
    }
    false
}

/// The one rule not anchored on a vendor prefix: `secret = "<long random
/// thing>"`. Both gates — length and entropy — are required, because
/// either alone produces constant false positives.
fn generic_assignment(line: &str) -> Option<&'static str> {
    let lower = line.to_ascii_lowercase();
    let name_at = SECRET_NAMES
        .iter()
        .find_map(|n| lower.find(n).map(|i| i + n.len()))?;
    let after = line.get(name_at..)?;

    // Only an assignment counts. `"the secret is in the vault"` should not.
    let sep = after.find(['=', ':'])?;
    // A separator far from the name means they're unrelated words.
    if after[..sep]
        .chars()
        .any(|c| !c.is_whitespace() && c != '"' && c != '\'')
    {
        return None;
    }

    let value: String = after[sep + 1..]
        .trim()
        .trim_start_matches(['"', '\'', '`'])
        .chars()
        .take_while(|c| !matches!(c, '"' | '\'' | '`' | ',' | ';' | ' '))
        .collect();

    if value.len() < MIN_GENERIC_VALUE
        || is_placeholder(&value)
        || !looks_like_a_literal_credential(&value)
        || shannon_bits_per_char(&value) < MIN_ENTROPY_BITS
    {
        return None;
    }
    Some("hardcoded credential")
}

/// Rejects values that are *code* rather than a literal credential.
///
/// Found by auditing this scanner against a real codebase: the single
/// biggest false-positive class was a secret-named field being assigned an
/// expression — `api_key: Arc::from(key.into().as_str())`,
/// `api_key: args.api_key.clone()`. That is precisely what correct
/// credential handling looks like, so warning on it would train the user to
/// ignore the warning on the code that deserves it least.
///
/// Three cheap signals, all of which a real key passes and code rarely does:
///
/// 1. **Charset.** A credential is base64/base62-ish. Anything containing
///    `(`, `:`, `[`, `$`, a space, or similar is an expression.
/// 2. **Mixed content.** Random keys carry both letters and digits;
///    `self.config.credentials.api_key` carries no digits.
/// 3. **Not a dotted identifier path.** `a.b.c` where every segment is a
///    plain identifier is attribute access, not a token.
fn looks_like_a_literal_credential(value: &str) -> bool {
    const CREDENTIAL_CHARS: fn(char) -> bool =
        |c| c.is_ascii_alphanumeric() || matches!(c, '+' | '/' | '=' | '_' | '-' | '.' | '~');
    if !value.chars().all(CREDENTIAL_CHARS) {
        return false;
    }
    if !value.chars().any(|c| c.is_ascii_digit()) || !value.chars().any(|c| c.is_ascii_alphabetic())
    {
        return false;
    }
    // `foo.bar.baz` -- attribute access. A JWT also has dots, but its
    // segments are long random base64 runs, not identifiers, so requiring
    // *every* segment to be identifier-shaped keeps JWTs detectable.
    if value.contains('.') {
        let all_identifiers = value.split('.').all(|seg| {
            !seg.is_empty()
                && seg.starts_with(|c: char| c.is_ascii_alphabetic() || c == '_')
                && seg.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
        });
        if all_identifiers {
            return false;
        }
    }
    true
}

/// Placeholder check for a *token body*: a charset-constrained random
/// string, not prose.
///
/// Deliberately much narrower than [`is_placeholder`]. Words like "your" or
/// "getenv" cannot meaningfully appear in a random base62 run, but they
/// *can* appear by coincidence — and rejecting a real 36-character token
/// because it happens to contain "todo" would be a silent miss. Only the
/// two conventions documentation actually uses are checked: vendors write
/// their example keys with EXAMPLE in the body (AWS's
/// `AKIAIOSFODNN7EXAMPLE` is the canonical one), and humans mask with a
/// repeated character.
fn is_masked_token(body: &str) -> bool {
    let lower = body.to_ascii_lowercase();
    if lower.contains("example") || lower.contains("xxxx") {
        return true;
    }
    let mut chars = body.chars();
    match chars.next() {
        Some(first) => chars.all(|c| c == first),
        None => true,
    }
}

fn is_placeholder(value: &str) -> bool {
    let lower = value.to_ascii_lowercase();
    if PLACEHOLDER_MARKERS.iter().any(|m| lower.contains(m)) {
        return true;
    }
    // A single repeated character (`****`, `aaaa`, `0000`) is a mask, not a
    // credential.
    let mut chars = value.chars();
    match chars.next() {
        Some(first) => chars.all(|c| c == first),
        None => true,
    }
}

/// Shannon entropy in bits per character. Zero for an empty or
/// single-character string, which correctly fails the floor.
fn shannon_bits_per_char(s: &str) -> f64 {
    if s.len() < 2 {
        return 0.0;
    }
    let mut counts = [0usize; 256];
    let mut total = 0usize;
    for b in s.bytes() {
        counts[b as usize] += 1;
        total += 1;
    }
    let total = total as f64;
    -counts
        .iter()
        .filter(|&&c| c > 0)
        .map(|&c| {
            let p = c as f64 / total;
            p * p.log2()
        })
        .sum::<f64>()
}

/// Render findings as the note appended to a tool result, or `None` when
/// there's nothing to say.
///
/// Phrased as a question about intent rather than an accusation: the agent
/// is very often writing a fixture or an example on purpose, and a warning
/// that assumes wrongdoing invites it to argue instead of check.
pub fn warning(findings: &[Finding]) -> Option<String> {
    if findings.is_empty() {
        return None;
    }
    let list = findings
        .iter()
        .map(|f| format!("line {}: {}", f.line, f.kind))
        .collect::<Vec<_>>()
        .join("; ");
    Some(format!(
        "\n⚠ possible credential in this content ({list}). If any of these are real, do not leave \
         them in the file -- move them to an environment variable or an untracked config file, and \
         tell the user. If they are placeholders or fixtures, say so and carry on."
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn kinds(s: &str) -> Vec<&'static str> {
        scan(s).into_iter().map(|f| f.kind).collect()
    }

    #[test]
    fn clean_code_produces_nothing() {
        let src = "fn main() {\n    let name = \"world\";\n    println!(\"hello {name}\");\n}";
        assert!(scan(src).is_empty());
        assert!(warning(&scan(src)).is_none());
    }

    /// Realistic token bodies. Using a repeated character or a vendor's
    /// documented EXAMPLE key here would make these tests pass or fail for
    /// reasons unrelated to what they claim to check.
    const AWS_BODY: &str = "J3KQR7XZM2VP4NB6";
    const GH_BODY: &str = "R2d4Kx9mQ7pL3vN8sT1bW5yC6zA0eG4hJ2kM";
    const SK_BODY: &str = "Ab3xY9zQ1mNp7Kd2Lw8s";

    #[test]
    fn an_aws_access_key_is_found_with_its_line_number() {
        let src = format!("region = us-east-1\naws_access_key_id = AKIA{AWS_BODY}\n");
        let found = scan(&src);
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].line, 2, "line numbers are 1-based like an editor");
        assert_eq!(found[0].kind, "AWS access key ID");
    }

    #[test]
    fn github_and_openai_style_tokens_are_found() {
        assert_eq!(
            kinds(&format!("token = \"ghp_{GH_BODY}\"")),
            vec!["GitHub personal access token"]
        );
        assert_eq!(
            kinds(&format!("key = \"sk-{SK_BODY}\"")),
            vec!["OpenAI-style API key"]
        );
    }

    #[test]
    fn a_vendors_own_documented_example_key_is_not_flagged() {
        // `AKIAIOSFODNN7EXAMPLE` is the key AWS puts in its own docs, so it
        // turns up in READMEs and fixtures constantly. Flagging it would be
        // the single most common false positive this scanner could have.
        assert!(scan("aws_access_key_id = AKIAIOSFODNN7EXAMPLE").is_empty());
    }

    #[test]
    fn a_more_specific_prefix_wins_over_a_general_one() {
        // "sk-" is a prefix of "sk-ant-"; reporting the vaguer one would be
        // a worse message for the same finding.
        assert_eq!(
            kinds(&format!("key = \"sk-ant-{SK_BODY}\"")),
            vec!["Anthropic API key"]
        );
    }

    #[test]
    fn a_private_key_header_is_enough_on_its_own() {
        assert_eq!(
            kinds("-----BEGIN OPENSSH PRIVATE KEY-----"),
            vec!["private key block"]
        );
    }

    #[test]
    fn a_long_random_assignment_is_flagged_generically() {
        assert_eq!(
            kinds("client_secret = \"f8Kq2mZx9Lp4Rv7Nw3Ty6Bd1Hj5Gc0As\""),
            vec!["hardcoded credential"]
        );
    }

    // --- the false-positive gauntlet ---------------------------------------
    //
    // Each of these is something a coding agent legitimately writes. A
    // warning here would train the user to ignore all of them.

    #[test]
    fn env_var_indirection_is_not_a_secret() {
        assert!(scan("const apiKey = process.env.API_KEY;").is_empty());
        assert!(scan("api_key = os.environ['API_KEY']").is_empty());
        assert!(scan("secret = getenv(\"APP_SECRET\")").is_empty());
        assert!(scan("password: ${DB_PASSWORD}").is_empty());
    }

    #[test]
    fn placeholders_in_example_config_are_not_secrets() {
        assert!(scan("API_KEY=your_api_key_here").is_empty());
        assert!(scan("api_key = \"<insert your key>\"").is_empty());
        assert!(scan("password = \"changeme_please_now_1234\"").is_empty());
        assert!(scan("secret_key = \"replace-with-real-value-xx\"").is_empty());
    }

    #[test]
    fn masked_values_are_not_secrets() {
        assert!(scan("password = \"************************\"").is_empty());
        assert!(scan("token: 'aaaaaaaaaaaaaaaaaaaaaaaaaaa'").is_empty());
    }

    #[test]
    fn short_or_low_entropy_values_are_not_secrets() {
        assert!(scan("password = \"hunter2\"").is_empty(), "too short");
        assert!(
            scan("secret = \"the quick brown fox jumped over\"").is_empty(),
            "prose has low entropy"
        );
    }

    #[test]
    fn prose_mentioning_a_secret_is_not_an_assignment() {
        assert!(scan("// the secret is stored in the vault").is_empty());
        assert!(scan("Ask the user for their password before continuing.").is_empty());
    }

    #[test]
    fn a_variable_named_like_a_secret_holding_a_reference_is_fine() {
        assert!(scan("let api_key = config.api_key;").is_empty());
        assert!(scan("self.access_token = token").is_empty());
    }

    // --- bounds ------------------------------------------------------------

    #[test]
    fn findings_are_capped_so_one_bad_file_cannot_flood_the_context() {
        let line = format!("key = \"ghp_{GH_BODY}\"\n");
        let found = scan(&line.repeat(50));
        assert_eq!(found.len(), MAX_FINDINGS);
    }

    #[test]
    fn one_line_reports_at_most_one_finding() {
        let src = format!("a = \"AKIA{AWS_BODY}\"; b = \"ghp_{GH_BODY}\"");
        assert_eq!(scan(&src).len(), 1);
    }

    #[test]
    fn crlf_content_reports_the_same_line_numbers_as_lf() {
        let lf = format!("one\ntwo\naws = AKIA{AWS_BODY}");
        let crlf = format!("one\r\ntwo\r\naws = AKIA{AWS_BODY}");
        assert_eq!(scan(&lf), scan(&crlf));
        assert_eq!(scan(&crlf)[0].line, 3);
    }

    #[test]
    fn the_warning_names_every_finding_and_says_what_to_do() {
        let w = warning(&scan(&format!("aws = AKIA{AWS_BODY}"))).unwrap();
        assert!(w.contains("line 1"));
        assert!(w.contains("AWS access key ID"));
        assert!(w.contains("environment variable"));
        // Must leave room for the legitimate case, or the model will just
        // argue with it.
        assert!(w.contains("placeholders or fixtures"));
    }

    #[test]
    fn entropy_separates_random_strings_from_prose() {
        assert!(shannon_bits_per_char("f8Kq2mZx9Lp4Rv7Nw3Ty") > MIN_ENTROPY_BITS);
        assert!(shannon_bits_per_char("aaaaaaaaaaaaaaaaaaaa") < MIN_ENTROPY_BITS);
        assert_eq!(shannon_bits_per_char(""), 0.0);
        assert_eq!(shannon_bits_per_char("a"), 0.0);
    }
}

/// Integration through the real tools. `scan`/`warning` are unit-tested
/// above; what matters here is that the note actually reaches the model,
/// and that it never turns a successful write into a failure.
#[cfg(test)]
mod tool_integration_tests {
    use crate::edit::EditFile;
    use crate::fs::{Workspace, WriteFile};
    use crate::tool::Tool;
    use serde_json::value::RawValue;

    fn ws(name: &str) -> Workspace {
        let dir = std::env::temp_dir().join(format!(
            "hivemind_secrets_{name}_{}_{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        Workspace::new(dir)
    }

    fn args(json: serde_json::Value) -> Box<RawValue> {
        RawValue::from_string(json.to_string()).unwrap()
    }

    #[tokio::test]
    async fn writing_a_credential_still_succeeds_but_says_so() {
        let w = ws("write_warn");
        let out = WriteFile(w.clone())
            .execute(&args(serde_json::json!({
                "path": "config.py",
                "content": "AWS_KEY = \"AKIAJ3KQR7XZM2VP4NB6\"\n"
            })))
            .await
            .expect("a credential must not turn the write into an error");

        assert!(out.starts_with("wrote "), "the normal result is preserved");
        assert!(out.contains("possible credential"));
        assert!(out.contains("AWS access key ID"));
        // And the file really was written -- warning, not blocking.
        assert!(w.root.join("config.py").exists());
    }

    #[tokio::test]
    async fn an_ordinary_write_gains_no_extra_noise() {
        let w = ws("write_clean");
        let out = WriteFile(w.clone())
            .execute(&args(serde_json::json!({
                "path": "main.rs", "content": "fn main() { println!(\"hi\"); }"
            })))
            .await
            .unwrap();
        assert!(!out.contains("credential"), "got {out:?}");
    }

    #[tokio::test]
    async fn an_edit_that_introduces_a_credential_is_flagged() {
        let w = ws("edit_warn");
        std::fs::write(w.root.join("a.py"), "KEY = \"placeholder\"\n").unwrap();
        let out = EditFile(w.clone())
            .execute(&args(serde_json::json!({
                "path": "a.py",
                "old_string": "\"placeholder\"",
                "new_string": "\"ghp_R2d4Kx9mQ7pL3vN8sT1bW5yC6zA0eG4hJ2kM\""
            })))
            .await
            .unwrap();
        assert!(out.contains("GitHub personal access token"), "got {out:?}");
    }

    #[tokio::test]
    async fn an_edit_near_a_pre_existing_credential_is_not_re_flagged() {
        // The file already contains a key the agent isn't touching. Warning
        // about it on every unrelated edit is how a useful signal turns
        // into noise the model learns to skip.
        let w = ws("edit_preexisting");
        std::fs::write(
            w.root.join("a.py"),
            "KEY = \"AKIAJ3KQR7XZM2VP4NB6\"\nname = \"old\"\n",
        )
        .unwrap();
        let out = EditFile(w.clone())
            .execute(&args(serde_json::json!({
                "path": "a.py", "old_string": "\"old\"", "new_string": "\"new\""
            })))
            .await
            .unwrap();
        assert!(!out.contains("credential"), "got {out:?}");
    }
}
