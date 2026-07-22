//! REPL slash commands: parsing, and the fixed name list the completer
//! reuses so `/`-completion and dispatch can never drift apart.

use harness_config::Tier;

pub const COMMAND_NAMES: &[&str] = &[
    "/help", "/clear", "/compact", "/tier", "/cost", "/undo", "/exit", "/quit",
];

pub const HELP_TEXT: &str = "\
Commands:
  /help              show this list
  /compact           fold older turns into a summary now
  /tier [flash|pro]  show or switch the active tier
  /cost              show session cost so far
  /undo [n]          undo the last n turns (default 1): restores edited/written
                     files and truncates the conversation back to before them
  /clear             clear the terminal
  /exit, /quit       leave
Reference a file inline with @path/to/file (tab-completes).";

pub enum TierArg {
    Show,
    Set(Tier),
    Invalid(String),
}

pub enum UndoArg {
    Count(usize),
    Invalid(String),
}

pub enum SlashCommand {
    Help,
    Clear,
    Compact,
    Tier(TierArg),
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
        "clear" | "cls" => SlashCommand::Clear,
        "compact" => SlashCommand::Compact,
        "tier" => SlashCommand::Tier(match arg {
            None => TierArg::Show,
            Some(a) => a
                .parse::<Tier>()
                .map(TierArg::Set)
                .unwrap_or_else(|_| TierArg::Invalid(a.to_string())),
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
    }

    #[test]
    fn tier_with_no_arg_means_show() {
        assert!(matches!(
            parse("/tier"),
            Some(SlashCommand::Tier(TierArg::Show))
        ));
    }

    #[test]
    fn tier_with_valid_arg_means_set() {
        assert!(matches!(
            parse("/tier pro"),
            Some(SlashCommand::Tier(TierArg::Set(Tier::Pro)))
        ));
        assert!(matches!(
            parse("/tier flash"),
            Some(SlashCommand::Tier(TierArg::Set(Tier::Flash)))
        ));
        assert!(matches!(
            parse("/tier PRO"),
            Some(SlashCommand::Tier(TierArg::Set(Tier::Pro)))
        ));
    }

    #[test]
    fn tier_with_bad_arg_is_invalid_not_silently_ignored() {
        match parse("/tier fastt") {
            Some(SlashCommand::Tier(TierArg::Invalid(s))) => assert_eq!(s, "fastt"),
            other => panic!("expected Invalid(\"fastt\"), got {}", other.is_some()),
        }
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
