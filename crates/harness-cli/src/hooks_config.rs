//! `hivemind hooks list|enable|disable|check` — the CLI-and-config-file
//! side of [`crate::hook_presets`]: turning a preset name into a
//! `[[hooks]]` block in `config.toml`, and back out again.
//!
//! # Why text editing, not a TOML round-trip
//!
//! Parsing the whole file into a struct, mutating it, and reserializing
//! would strip the user's own comments and could reformat blocks they
//! hand-wrote — this file is explicitly meant to be human-edited (see
//! `config.example.toml`'s header). Every block this module generates
//! carries a `name` of the form `preset:<preset>` or
//! `preset:<preset>:<arg>`, which is what makes exact-match append/remove
//! possible without a real parser: `enable` looks for that exact string
//! before appending, `disable` looks for the `[[hooks]]` block containing
//! it and removes exactly that span.

use crate::hook_presets::{ArgRequirement, Preset};

/// The unique `name` a preset's generated hook is given — also the only
/// thing `disable` needs in order to find and remove it again.
///
/// Precondition: `arg`, if present, has already passed [`valid_preset_arg`].
/// This function interpolates it directly into a `name = "..."` TOML
/// string with no escaping of its own — callers taking arguments from a
/// user (just `enable`) are responsible for validating first.
pub fn generated_hook_name(preset: &str, arg: Option<&str>) -> String {
    match arg {
        Some(a) => format!("preset:{preset}:{a}"),
        None => format!("preset:{preset}"),
    }
}

/// Whether `s` is safe to embed as a preset argument: it ends up inside two
/// nested quoted-string contexts on its way into `config.toml` (a shell
/// argument, itself inside a TOML string), and gets used verbatim as an
/// exact-match key by [`append_if_absent`]/[`remove_block`]. Rather than
/// layer escaping through every one of those, arguments containing a
/// double quote or a control character are rejected outright at the point
/// they're accepted from the user (`enable`, in `main.rs`).
///
/// This costs nothing for the actual use case: every preset's argument is
/// a directory or file path, and `"` is already an illegal character in a
/// Windows path -- so nothing realistic is excluded.
pub fn valid_preset_arg(s: &str) -> bool {
    !s.is_empty() && !s.contains('"') && !s.chars().any(|c| c.is_control())
}

/// Minimal shell-argument quoting: wrap in double quotes, escaping an
/// embedded backslash. (A double quote itself never reaches here --
/// [`valid_preset_arg`] already rejected it.) Not a full shell grammar,
/// matching the same pragmatism as `crate::secrets` and the
/// destructive-shell list: covers every realistic path argument without
/// pretending to parse arbitrary shell syntax.
fn quote_arg(s: &str) -> String {
    debug_assert!(
        valid_preset_arg(s),
        "quote_arg called with an unvalidated argument"
    );
    format!("\"{}\"", s.replace('\\', "\\\\"))
}

/// Escapes backslashes and double quotes for embedding inside a TOML basic
/// string (`"..."`), independent of and applied *after* any shell-level
/// quoting the value already carries — see the caller for why the two are
/// separate passes over the same characters.
fn toml_escape(s: &str) -> String {
    s.replace('\\', "\\\\").replace('"', "\\\"")
}

/// The `[[hooks]]` block `enable` appends, exactly as it will sit on disk.
pub fn render_block(preset: &Preset, arg: Option<&str>) -> String {
    let name = generated_hook_name(preset.name, arg);
    let matcher = preset
        .matcher
        .iter()
        .map(|m| format!("\"{m}\""))
        .collect::<Vec<_>>()
        .join(", ");
    // Shells back into `hivemind` itself rather than jq/grep -- see
    // `crate::hook_presets`'s module docs for why.
    let command = match arg {
        Some(a) => format!("hivemind hooks check {} {}", preset.name, quote_arg(a)),
        None => format!("hivemind hooks check {}", preset.name),
    };
    // `command`'s own value already carries shell-level double quotes from
    // `quote_arg` (for an argument with a space). Those need a *second*,
    // independent escaping pass here -- for TOML's own basic-string syntax
    // this whole thing is about to sit inside -- or the result is invalid
    // TOML the moment an argument needs shell-quoting at all.
    let command_toml = toml_escape(&command);
    format!(
        "\n[[hooks]]\nname = \"{name}\"\nevent = \"{}\"\nmatcher = [{matcher}]\ncommand = \"{command_toml}\"\nenforcement = true\n",
        preset.event,
    )
}

/// Append `block` to `existing`, unless a block with `name = "<name>"` is
/// already present. `None` means nothing changed (already enabled).
pub fn append_if_absent(existing: &str, name: &str, block: &str) -> Option<String> {
    if existing.contains(&format!("name = \"{name}\"")) {
        return None;
    }
    let mut out = existing.trim_end_matches('\n').to_string();
    if !out.is_empty() {
        out.push('\n');
    }
    out.push_str(block.trim_start_matches('\n'));
    if !out.ends_with('\n') {
        out.push('\n');
    }
    Some(out)
}

/// Remove the `[[hooks]]` block whose body contains `name = "<name>"`.
/// `None` if no such block is found.
///
/// Finds every `[[hooks]]` table-header line, and for each treats its body
/// as running until the next `[[hooks]]` header, the next `[section]`
/// header, or EOF — whichever comes first — then drops the whole span for
/// whichever block's body contains the target `name` line.
pub fn remove_block(existing: &str, name: &str) -> Option<String> {
    let needle = format!("name = \"{name}\"");
    let lines: Vec<&str> = existing.lines().collect();

    let mut starts: Vec<usize> = lines
        .iter()
        .enumerate()
        .filter(|(_, l)| l.trim() == "[[hooks]]")
        .map(|(i, _)| i)
        .collect();
    starts.push(lines.len()); // sentinel: gives the last block an end

    for w in starts.windows(2) {
        let (start, next_start) = (w[0], w[1]);
        let mut end = next_start;
        for (i, l) in lines
            .iter()
            .enumerate()
            .skip(start + 1)
            .take(next_start - start - 1)
        {
            let t = l.trim();
            if t.starts_with('[') && t != "[[hooks]]" {
                end = i;
                break;
            }
        }
        if lines[start..end].iter().any(|l| l.trim() == needle) {
            let mut kept: Vec<&str> = Vec::with_capacity(lines.len());
            kept.extend_from_slice(&lines[..start]);
            kept.extend_from_slice(&lines[end..]);
            let mut out = kept.join("\n");
            // Collapse the blank-line seam the removal likely left.
            while out.contains("\n\n\n") {
                out = out.replace("\n\n\n", "\n\n");
            }
            if !out.is_empty() && !out.ends_with('\n') {
                out.push('\n');
            }
            return Some(out);
        }
    }
    None
}

/// Every generated-preset hook name currently present in `content`, read
/// straight off `name = "preset:..."` lines. Used by `hivemind hooks list`
/// to show what's on without a full TOML parse.
pub fn enabled_preset_names(content: &str) -> Vec<String> {
    content
        .lines()
        .filter_map(|l| {
            let t = l.trim();
            let rest = t.strip_prefix("name = \"preset:")?.strip_suffix('"')?;
            Some(format!("preset:{rest}"))
        })
        .collect()
}

/// `hivemind hooks list`
pub fn list() {
    let path = harness_config::default_config_path();
    let content = std::fs::read_to_string(&path).unwrap_or_default();
    let enabled = enabled_preset_names(&content);

    println!("hook presets (config: {}):\n", path.display());
    for p in crate::hook_presets::PRESETS {
        let no_arg_name = format!("preset:{}", p.name);
        let arg_prefix = format!("preset:{}:", p.name);
        let on: Vec<&str> = enabled
            .iter()
            .filter_map(|n| {
                if n == &no_arg_name {
                    Some("")
                } else {
                    n.strip_prefix(&arg_prefix)
                }
            })
            .collect();

        let status = if on.is_empty() {
            "\x1b[90moff\x1b[0m".to_string()
        } else if on == [""] {
            "\x1b[32mon\x1b[0m".to_string()
        } else {
            format!("\x1b[32mon\x1b[0m ({})", on.join(", "))
        };
        println!("  \x1b[1m{}\x1b[0m  {status}", p.name);
        println!("      {}", p.description);
        if let ArgRequirement::Required { hint } = p.arg {
            println!("      \x1b[90mneeds an argument: {hint}\x1b[0m");
        }
    }
    println!("\nhivemind hooks enable <preset> [arg]   -- turn one on");
    println!("hivemind hooks disable <preset> [arg]  -- turn one off");
}

/// `hivemind hooks enable <preset> [arg]`
pub fn enable(preset_name: &str, arg: Option<&str>) -> anyhow::Result<()> {
    let preset = crate::hook_presets::find(preset_name).ok_or_else(|| {
        anyhow::anyhow!(
            "unknown preset {preset_name:?} -- `hivemind hooks list` shows the available ones"
        )
    })?;
    match (&preset.arg, arg) {
        (ArgRequirement::Required { hint }, None) => anyhow::bail!(
            "{preset_name} needs an argument, e.g. `hivemind hooks enable {preset_name} {hint}`"
        ),
        (ArgRequirement::None, Some(_)) => {
            anyhow::bail!("{preset_name} does not take an argument")
        }
        _ => {}
    }
    if let Some(a) = arg
        && !valid_preset_arg(a)
    {
        anyhow::bail!(
            "argument {a:?} contains a double quote or control character, which can't be \
             safely written into config.toml -- use a plain path with no quotes"
        );
    }

    let path = harness_config::default_config_path();
    let existing = std::fs::read_to_string(&path).unwrap_or_default();
    let name = generated_hook_name(preset.name, arg);
    let block = render_block(preset, arg);

    match append_if_absent(&existing, &name, &block) {
        None => println!(
            "{preset_name}{} is already enabled",
            arg.map(|a| format!(" ({a})")).unwrap_or_default()
        ),
        Some(updated) => {
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent)?;
            }
            std::fs::write(&path, updated)?;
            println!(
                "enabled {preset_name}{} -- {}",
                arg.map(|a| format!(" ({a})")).unwrap_or_default(),
                path.display()
            );
        }
    }
    Ok(())
}

/// `hivemind hooks disable <preset> [arg]`
pub fn disable(preset_name: &str, arg: Option<&str>) -> anyhow::Result<()> {
    let path = harness_config::default_config_path();
    let existing = match std::fs::read_to_string(&path) {
        Ok(s) => s,
        Err(_) => {
            println!("no config file at {} -- nothing is enabled", path.display());
            return Ok(());
        }
    };
    let name = generated_hook_name(preset_name, arg);
    match remove_block(&existing, &name) {
        None => println!(
            "{preset_name}{} was not enabled",
            arg.map(|a| format!(" ({a})")).unwrap_or_default()
        ),
        Some(updated) => {
            std::fs::write(&path, updated)?;
            println!(
                "disabled {preset_name}{}",
                arg.map(|a| format!(" ({a})")).unwrap_or_default()
            );
        }
    }
    Ok(())
}

/// `hivemind hooks check <preset> [arg]` — reads the hook JSON envelope
/// from stdin, evaluates the preset, and prints the `{"decision":...}`
/// protocol `harness_agent::hooks::parse_decision` expects. This is what a
/// preset's generated `command` actually invokes; not meant to be run by
/// hand.
pub fn check(preset: &str, arg: Option<&str>) -> anyhow::Result<()> {
    use std::io::Read;
    let mut input = String::new();
    std::io::stdin().read_to_string(&mut input)?;

    match crate::hook_presets::evaluate(preset, arg, &input) {
        Ok(crate::hook_presets::Decision::Allow) => {
            println!("{}", serde_json::json!({"decision": "allow"}));
            Ok(())
        }
        Ok(crate::hook_presets::Decision::Deny(reason)) => {
            println!(
                "{}",
                serde_json::json!({"decision": "deny", "reason": reason})
            );
            Ok(())
        }
        // Deliberately NOT printed as {"decision":...} and deliberately a
        // nonzero exit (via the `?`-propagated Err reaching main's error
        // handler): an unparseable envelope or a misconfigured preset must
        // fall through to the hook's own `enforcement` setting
        // (harness_agent::hooks::on_failure), not have this module invent
        // its own separate allow/deny default.
        Err(e) => anyhow::bail!("hook preset check failed: {e}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hook_presets::find;

    #[test]
    fn generated_names_are_namespaced_and_distinct() {
        assert_eq!(
            generated_hook_name("no-force-push", None),
            "preset:no-force-push"
        );
        assert_eq!(
            generated_hook_name("restrict-writes-to", Some("src")),
            "preset:restrict-writes-to:src"
        );
        // Different args must produce different names, or enabling the
        // same preset for two directories would collide.
        assert_ne!(
            generated_hook_name("restrict-writes-to", Some("src")),
            generated_hook_name("restrict-writes-to", Some("lib"))
        );
    }

    #[test]
    fn a_rendered_block_is_valid_toml_and_matches_the_hook_spec_shape() {
        let preset = find("restrict-writes-to").unwrap();
        let block = render_block(preset, Some("src"));
        let parsed: toml::Value = toml::from_str(&block).expect("valid toml");
        let hooks = parsed["hooks"].as_array().unwrap();
        assert_eq!(hooks.len(), 1);
        assert_eq!(
            hooks[0]["name"].as_str().unwrap(),
            "preset:restrict-writes-to:src"
        );
        assert_eq!(hooks[0]["event"].as_str().unwrap(), "pre_tool_use");
        assert!(hooks[0]["enforcement"].as_bool().unwrap());
        assert!(
            hooks[0]["command"]
                .as_str()
                .unwrap()
                .contains("hivemind hooks check restrict-writes-to")
        );
    }

    #[test]
    fn quoting_survives_a_directory_with_a_space() {
        let preset = find("protect-path").unwrap();
        let block = render_block(preset, Some("my secrets"));
        let parsed: toml::Value = toml::from_str(&block).expect("valid toml");
        let cmd = parsed["hooks"][0]["command"].as_str().unwrap();
        assert!(cmd.contains("\"my secrets\""), "got {cmd:?}");
    }

    #[test]
    fn the_command_field_round_trips_through_toml_byte_for_byte() {
        // Two independent escaping passes are involved (shell-quoting the
        // argument, then TOML-escaping the whole command) -- this proves
        // they compose correctly rather than double-escaping or
        // under-escaping. What a TOML parser hands back must be exactly
        // what a real shell would need to see to parse `my secrets` as one
        // argument.
        let preset = find("protect-path").unwrap();
        let block = render_block(preset, Some("my secrets"));
        let parsed: toml::Value = toml::from_str(&block).unwrap();
        let cmd = parsed["hooks"][0]["command"].as_str().unwrap();
        assert_eq!(cmd, "hivemind hooks check protect-path \"my secrets\"");
    }

    #[test]
    fn an_argument_with_a_quote_is_rejected_before_it_can_corrupt_the_file() {
        // This is the boundary check that makes it safe for render_block to
        // do no escaping of the name field at all -- an argument that would
        // break the generated TOML never reaches it.
        assert!(!valid_preset_arg("weird\"name"));
        assert!(enable("protect-path", Some("weird\"name")).is_err());
    }

    #[test]
    fn ordinary_path_shaped_arguments_are_all_valid() {
        for a in [
            "src",
            "src/",
            "lib/core",
            ".env",
            "my secrets",
            "C:\\Users\\me",
        ] {
            assert!(valid_preset_arg(a), "{a:?} should be a valid argument");
        }
    }

    #[test]
    fn empty_or_control_character_arguments_are_invalid() {
        assert!(!valid_preset_arg(""));
        assert!(!valid_preset_arg("a\tb"));
        assert!(!valid_preset_arg("a\nb"));
    }

    #[test]
    fn appending_to_an_empty_file_produces_valid_toml() {
        let preset = find("no-force-push").unwrap();
        let block = render_block(preset, None);
        let out = append_if_absent("", "preset:no-force-push", &block).unwrap();
        toml::from_str::<toml::Value>(&out).expect("valid toml");
        assert!(out.contains("name = \"preset:no-force-push\""));
    }

    #[test]
    fn appending_preserves_existing_content() {
        let existing = "[model]\nmodel = \"hivemind\"\n";
        let preset = find("no-force-push").unwrap();
        let block = render_block(preset, None);
        let out = append_if_absent(existing, "preset:no-force-push", &block).unwrap();
        assert!(out.contains("model = \"hivemind\""));
        assert!(out.contains("preset:no-force-push"));
        toml::from_str::<toml::Value>(&out).expect("valid toml");
    }

    #[test]
    fn enabling_the_same_preset_twice_does_not_duplicate() {
        let preset = find("no-force-push").unwrap();
        let block = render_block(preset, None);
        let once = append_if_absent("", "preset:no-force-push", &block).unwrap();
        let twice = append_if_absent(&once, "preset:no-force-push", &block);
        assert!(twice.is_none(), "must be a no-op, not a duplicate block");
    }

    #[test]
    fn two_different_directories_for_the_same_preset_both_land() {
        let preset = find("restrict-writes-to").unwrap();
        let step1 = append_if_absent(
            "",
            "preset:restrict-writes-to:src",
            &render_block(preset, Some("src")),
        )
        .unwrap();
        let step2 = append_if_absent(
            &step1,
            "preset:restrict-writes-to:lib",
            &render_block(preset, Some("lib")),
        )
        .unwrap();
        assert!(step2.contains("preset:restrict-writes-to:src"));
        assert!(step2.contains("preset:restrict-writes-to:lib"));
        let parsed: toml::Value = toml::from_str(&step2).expect("valid toml");
        assert_eq!(parsed["hooks"].as_array().unwrap().len(), 2);
    }

    #[test]
    fn removing_the_only_block_leaves_a_parseable_empty_ish_file() {
        let preset = find("no-force-push").unwrap();
        let with =
            append_if_absent("", "preset:no-force-push", &render_block(preset, None)).unwrap();
        let without = remove_block(&with, "preset:no-force-push").unwrap();
        assert!(!without.contains("preset:no-force-push"));
        // What's left must still be valid TOML, even if that's "nothing".
        toml::from_str::<toml::Value>(&without).expect("valid toml");
    }

    #[test]
    fn removing_one_of_several_blocks_leaves_the_others_intact() {
        let a = find("no-force-push").unwrap();
        let b = find("no-destructive-shell").unwrap();
        let with_a = append_if_absent("", "preset:no-force-push", &render_block(a, None)).unwrap();
        let with_both = append_if_absent(
            &with_a,
            "preset:no-destructive-shell",
            &render_block(b, None),
        )
        .unwrap();

        let after = remove_block(&with_both, "preset:no-force-push").unwrap();
        assert!(!after.contains("preset:no-force-push"));
        assert!(after.contains("preset:no-destructive-shell"));
        let parsed: toml::Value = toml::from_str(&after).expect("valid toml");
        assert_eq!(parsed["hooks"].as_array().unwrap().len(), 1);
    }

    #[test]
    fn removing_a_block_does_not_touch_a_hand_written_hook() {
        let existing =
            "\n[[hooks]]\nname = \"my-own-hook\"\nevent = \"pre_tool_use\"\ncommand = \"exit 0\"\n";
        let preset = find("no-force-push").unwrap();
        let with_preset = append_if_absent(
            existing,
            "preset:no-force-push",
            &render_block(preset, None),
        )
        .unwrap();
        let after = remove_block(&with_preset, "preset:no-force-push").unwrap();
        assert!(after.contains("my-own-hook"));
        assert!(!after.contains("preset:no-force-push"));
    }

    #[test]
    fn removing_a_block_that_sits_before_a_different_section_stops_at_that_section() {
        // The block's body must be bounded by the next [section] header,
        // not swallow everything after it to EOF.
        let preset = find("no-force-push").unwrap();
        let mut existing =
            append_if_absent("", "preset:no-force-push", &render_block(preset, None)).unwrap();
        existing.push_str("\n[agent]\nmax_turns = 60\n");

        let after = remove_block(&existing, "preset:no-force-push").unwrap();
        assert!(!after.contains("preset:no-force-push"));
        assert!(after.contains("[agent]"));
        assert!(after.contains("max_turns = 60"));
    }

    #[test]
    fn removing_a_nonexistent_block_is_none_not_a_silent_no_op_disguised_as_success() {
        assert!(remove_block("", "preset:no-force-push").is_none());
        assert!(remove_block("[model]\n", "preset:no-force-push").is_none());
    }

    #[test]
    fn enabled_names_are_read_back_correctly() {
        let a = find("no-force-push").unwrap();
        let b = find("restrict-writes-to").unwrap();
        let content = append_if_absent(
            &append_if_absent("", "preset:no-force-push", &render_block(a, None)).unwrap(),
            "preset:restrict-writes-to:src",
            &render_block(b, Some("src")),
        )
        .unwrap();

        let names = enabled_preset_names(&content);
        assert!(names.contains(&"preset:no-force-push".to_string()));
        assert!(names.contains(&"preset:restrict-writes-to:src".to_string()));
        assert_eq!(names.len(), 2);
    }

    #[test]
    fn a_hand_written_hook_is_not_mistaken_for_a_preset() {
        let content =
            "\n[[hooks]]\nname = \"my-own-hook\"\nevent = \"pre_tool_use\"\ncommand = \"exit 0\"\n";
        assert!(enabled_preset_names(content).is_empty());
    }
}
