//! REPL slash commands: parsing, and the fixed name list the completer
//! reuses so `/`-completion and dispatch can never drift apart.

pub const COMMAND_NAMES: &[&str] = &[
    "/help",
    "/status",
    "/context",
    "/diff",
    "/clear",
    "/compact",
    "/model",
    "/reasoning",
    "/budget",
    "/web",
    "/cost",
    "/undo",
    "/exit",
    "/quit",
];

pub const HELP_TEXT: &str = "\
Commands:
  /help              show this list
  /status            model, mode, cost, and what this session has changed
  /context           how much of the model's context window is in use
  /diff [path]       what this session changed on disk, as a diff
  /compact           fold older turns into a summary now
  /model [id]        show available models, or switch the active one
  /reasoning [level]  show/set reasoning effort for the active model, or `off`
  /budget [amount]   show/set a session USD spend cap, or `off` (default: unbounded)
  /web [on|off]      show or toggle hosted web search (off by default)
  /cost              show session cost so far
  /undo [n]          undo the last n turns (default 1): restores edited/written
                     files and truncates the conversation back to before them
  /clear             clear the terminal
  /exit, /quit       leave
Reference a file inline with @path/to/file (tab-completes).";

pub enum ModelArg {
    Show,
    /// Any string is accepted, unvalidated — a BYOK key can point at a
    /// provider-native model id `KNOWN_MODELS` has never heard of.
    Set(String),
}

/// Unlike `ModelArg`, validity genuinely depends on the active model (see
/// `harness_config::ModelCatalogEntry::reasoning_efforts`), so this stays a
/// raw string here too -- validated at the dispatch site in `main.rs`,
/// which has the live `Agent` to check against.
pub enum ReasoningArg {
    Show,
    Off,
    Set(String),
}

pub enum BudgetArg {
    Show,
    Off,
    Set(f64),
    Invalid(String),
}

pub enum UndoArg {
    Count(usize),
    Invalid(String),
}

pub enum WebArg {
    Show,
    On,
    Off,
    Invalid(String),
}

pub enum SlashCommand {
    Help,
    Status,
    Context,
    /// `None` means every changed file; `Some(path)` narrows to one, matched
    /// as a suffix of the tracked path so `/diff main.rs` works without
    /// typing the whole thing.
    Diff(Option<String>),
    Clear,
    Compact,
    Model(ModelArg),
    Reasoning(ReasoningArg),
    Budget(BudgetArg),
    Web(WebArg),
    Cost,
    Undo(UndoArg),
    Exit,
    Unknown(String),
}

/// Parse a line as a slash command. Returns `None` if `line` doesn't start
/// with `/` at all (i.e. it's an ordinary message, not a command).
pub fn parse(line: &str) -> Option<SlashCommand> {
    let rest = line.trim().strip_prefix('/')?;
    let mut parts = rest.split_whitespace();
    let name = parts.next().unwrap_or("");
    let arg = parts.next();

    Some(match name {
        "help" | "h" | "?" => SlashCommand::Help,
        "status" => SlashCommand::Status,
        "context" => SlashCommand::Context,
        "diff" | "changes" => SlashCommand::Diff(arg.map(str::to_string)),
        "clear" | "cls" => SlashCommand::Clear,
        "compact" => SlashCommand::Compact,
        "model" => SlashCommand::Model(match arg {
            None => ModelArg::Show,
            Some(a) => ModelArg::Set(a.to_string()),
        }),
        "reasoning" => SlashCommand::Reasoning(match arg {
            None => ReasoningArg::Show,
            Some("off") => ReasoningArg::Off,
            Some(a) => ReasoningArg::Set(a.to_string()),
        }),
        "budget" => SlashCommand::Budget(match arg {
            None => BudgetArg::Show,
            Some("off") => BudgetArg::Off,
            Some(a) => a
                .parse::<f64>()
                .map(BudgetArg::Set)
                .unwrap_or_else(|_| BudgetArg::Invalid(a.to_string())),
        }),
        "web" => SlashCommand::Web(match arg {
            None | Some("status") => WebArg::Show,
            Some("on") => WebArg::On,
            Some("off") => WebArg::Off,
            Some(other) => WebArg::Invalid(other.to_string()),
        }),
        "cost" | "usage" => SlashCommand::Cost,
        "undo" => SlashCommand::Undo(match arg {
            None => UndoArg::Count(1),
            Some(a) => a
                .parse::<usize>()
                .map(UndoArg::Count)
                .unwrap_or_else(|_| UndoArg::Invalid(a.to_string())),
        }),
        "exit" | "quit" | "q" => SlashCommand::Exit,
        other => SlashCommand::Unknown(other.to_string()),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn non_slash_input_is_not_a_command() {
        assert!(parse("just a message").is_none());
        assert!(parse("").is_none());
    }

    #[test]
    fn recognizes_all_documented_commands_and_aliases() {
        assert!(matches!(parse("/help"), Some(SlashCommand::Help)));
        assert!(matches!(parse("/?"), Some(SlashCommand::Help)));
        assert!(matches!(parse("/clear"), Some(SlashCommand::Clear)));
        assert!(matches!(parse("/compact"), Some(SlashCommand::Compact)));
        assert!(matches!(parse("/cost"), Some(SlashCommand::Cost)));
        assert!(matches!(parse("/exit"), Some(SlashCommand::Exit)));
        assert!(matches!(parse("/quit"), Some(SlashCommand::Exit)));
        assert!(matches!(parse("/status"), Some(SlashCommand::Status)));
        assert!(matches!(parse("/context"), Some(SlashCommand::Context)));
        assert!(matches!(parse("/diff"), Some(SlashCommand::Diff(None))));
        assert!(matches!(parse("/changes"), Some(SlashCommand::Diff(None))));
    }

    #[test]
    fn diff_takes_an_optional_path_filter() {
        match parse("/diff src/main.rs") {
            Some(SlashCommand::Diff(Some(p))) => assert_eq!(p, "src/main.rs"),
            _ => panic!("expected a filtered diff"),
        }
    }

    #[test]
    fn every_completable_command_actually_parses() {
        // The completer offers COMMAND_NAMES; a name there that dispatch
        // doesn't know would tab-complete straight into "unknown command".
        for name in COMMAND_NAMES {
            assert!(
                !matches!(parse(name), Some(SlashCommand::Unknown(_)) | None),
                "{name} completes but does not parse"
            );
        }
    }

    #[test]
    fn every_completable_command_is_documented() {
        for name in COMMAND_NAMES {
            assert!(
                HELP_TEXT.contains(name),
                "{name} is offered by the completer but missing from /help"
            );
        }
    }

    #[test]
    fn model_with_no_arg_means_show() {
        assert!(matches!(
            parse("/model"),
            Some(SlashCommand::Model(ModelArg::Show))
        ));
    }

    #[test]
    fn model_with_arg_means_set_to_that_exact_string() {
        match parse("/model claude-sonnet-5") {
            Some(SlashCommand::Model(ModelArg::Set(id))) => assert_eq!(id, "claude-sonnet-5"),
            _ => panic!("expected Model(Set(..))"),
        }
        // Unrecognized strings are accepted too -- validation, if any,
        // happens downstream (a BYOK key might point at a model id
        // KNOWN_MODELS has never heard of).
        match parse("/model some-custom-id") {
            Some(SlashCommand::Model(ModelArg::Set(id))) => assert_eq!(id, "some-custom-id"),
            _ => panic!("expected Model(Set(..))"),
        }
    }

    #[test]
    fn reasoning_with_no_arg_means_show() {
        assert!(matches!(
            parse("/reasoning"),
            Some(SlashCommand::Reasoning(ReasoningArg::Show))
        ));
    }

    #[test]
    fn reasoning_off_is_its_own_variant_not_a_literal_string_set() {
        assert!(matches!(
            parse("/reasoning off"),
            Some(SlashCommand::Reasoning(ReasoningArg::Off))
        ));
    }

    #[test]
    fn reasoning_with_a_level_means_set_to_that_exact_string() {
        match parse("/reasoning high") {
            Some(SlashCommand::Reasoning(ReasoningArg::Set(level))) => assert_eq!(level, "high"),
            _ => panic!("expected Reasoning(Set(..))"),
        }
    }

    #[test]
    fn budget_with_no_arg_means_show() {
        assert!(matches!(
            parse("/budget"),
            Some(SlashCommand::Budget(BudgetArg::Show))
        ));
    }

    #[test]
    fn budget_off_is_its_own_variant() {
        assert!(matches!(
            parse("/budget off"),
            Some(SlashCommand::Budget(BudgetArg::Off))
        ));
    }

    #[test]
    fn budget_with_a_number_means_set() {
        match parse("/budget 0.50") {
            Some(SlashCommand::Budget(BudgetArg::Set(amount))) => {
                assert!((amount - 0.5).abs() < 1e-9)
            }
            _ => panic!("expected Budget(Set(..))"),
        }
    }

    #[test]
    fn budget_with_a_non_number_is_invalid_not_silently_ignored() {
        match parse("/budget lots") {
            Some(SlashCommand::Budget(BudgetArg::Invalid(s))) => assert_eq!(s, "lots"),
            other => panic!("expected Invalid(\"lots\"), got {}", other.is_some()),
        }
    }

    #[test]
    fn web_supports_status_on_and_off() {
        assert!(matches!(
            parse("/web"),
            Some(SlashCommand::Web(WebArg::Show))
        ));
        assert!(matches!(
            parse("/web status"),
            Some(SlashCommand::Web(WebArg::Show))
        ));
        assert!(matches!(
            parse("/web on"),
            Some(SlashCommand::Web(WebArg::On))
        ));
        assert!(matches!(
            parse("/web off"),
            Some(SlashCommand::Web(WebArg::Off))
        ));
        assert!(matches!(
            parse("/web maybe"),
            Some(SlashCommand::Web(WebArg::Invalid(_)))
        ));
    }

    #[test]
    fn undo_with_no_arg_means_one() {
        assert!(matches!(
            parse("/undo"),
            Some(SlashCommand::Undo(UndoArg::Count(1)))
        ));
    }

    #[test]
    fn undo_with_valid_arg_means_that_many() {
        assert!(matches!(
            parse("/undo 3"),
            Some(SlashCommand::Undo(UndoArg::Count(3)))
        ));
    }

    #[test]
    fn undo_with_bad_arg_is_invalid_not_silently_ignored() {
        match parse("/undo all") {
            Some(SlashCommand::Undo(UndoArg::Invalid(s))) => assert_eq!(s, "all"),
            other => panic!("expected Invalid(\"all\"), got {}", other.is_some()),
        }
    }

    #[test]
    fn unknown_command_is_reported_not_dropped() {
        match parse("/bogus") {
            Some(SlashCommand::Unknown(s)) => assert_eq!(s, "bogus"),
            _ => panic!("expected Unknown"),
        }
    }
}
