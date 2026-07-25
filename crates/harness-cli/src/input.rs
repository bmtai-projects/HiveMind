//! The interactive line editor: history, and completion for `/` commands
//! and `@` file references (see [`crate::completion`]) that pops up as soon
//! as you type the trigger character — no Tab needed, though Tab still
//! opens/cycles it too.

use std::borrow::Cow;
use std::path::{Path, PathBuf};

use reedline::{
    ColumnarMenu, DefaultHinter, EditCommand, Emacs, FileBackedHistory, KeyCode, KeyModifiers,
    MenuBuilder, Prompt, PromptEditMode, PromptHistorySearch, PromptHistorySearchStatus, Reedline,
    ReedlineEvent, ReedlineMenu, default_emacs_keybindings,
};

use crate::completion::HiveCompleter;

const MENU_NAME: &str = "hivemind_completion_menu";

/// Build the line editor. `history_path` is best-effort — if the parent
/// directory can't be created or the file can't be opened, history is
/// silently disabled rather than failing the whole REPL over it.
pub fn build_line_editor(workdir: &Path, history_path: Option<PathBuf>) -> Reedline {
    let completer = Box::new(HiveCompleter::new(workdir.to_path_buf()));
    let completion_menu = Box::new(ColumnarMenu::default().with_name(MENU_NAME));

    let mut keybindings = default_emacs_keybindings();
    keybindings.add_binding(
        KeyModifiers::NONE,
        KeyCode::Tab,
        ReedlineEvent::UntilFound(vec![
            ReedlineEvent::Menu(MENU_NAME.to_string()),
            ReedlineEvent::MenuNext,
        ]),
    );
    // Typing '@' or '/' inserts the character *and* opens the menu in the
    // same keystroke -- matches the "type the trigger, suggestions just
    // appear" feel instead of requiring a Tab press afterward. The
    // completer itself still decides what (if anything) matches, so typing
    // '/' mid-sentence (e.g. "src/main.rs") just opens an empty menu, not
    // an error.
    for trigger in ['@', '/'] {
        keybindings.add_binding(
            KeyModifiers::NONE,
            KeyCode::Char(trigger),
            ReedlineEvent::Multiple(vec![
                ReedlineEvent::Edit(vec![EditCommand::InsertChar(trigger)]),
                ReedlineEvent::Menu(MENU_NAME.to_string()),
            ]),
        );
    }
    let edit_mode = Box::new(Emacs::new(keybindings));

    let mut line_editor = Reedline::create()
        .with_completer(completer)
        .with_menu(ReedlineMenu::EngineCompleter(completion_menu))
        .with_hinter(Box::new(DefaultHinter::default()))
        .with_edit_mode(edit_mode);

    if let Some(path) = history_path {
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        if let Ok(history) = FileBackedHistory::with_file(1000, path) {
            line_editor = line_editor.with_history(Box::new(history));
        }
    }

    line_editor
}

/// Rebuilt fresh before every `read_line()` call (see `crate::repl`) so it
/// always reflects the model actually active for the *next* input --
/// `/model` can change it mid-session, and there's no cheaper way to keep
/// a `Prompt` impl in sync with that than just reconstructing it each turn.
pub struct HivePrompt {
    pub model: String,
    pub yolo: bool,
}

impl Prompt for HivePrompt {
    fn render_prompt_left(&self) -> Cow<'_, str> {
        Cow::Borrowed("")
    }

    fn render_prompt_right(&self) -> Cow<'_, str> {
        let mode = if self.yolo { "yolo" } else { "approve" };
        Cow::Owned(format!("\x1b[90m{} · {mode}\x1b[0m", self.model))
    }

    fn render_prompt_indicator(&self, _edit_mode: PromptEditMode) -> Cow<'_, str> {
        Cow::Borrowed("\x1b[1m\u{203a} \x1b[0m")
    }

    fn render_prompt_multiline_indicator(&self) -> Cow<'_, str> {
        Cow::Borrowed("\u{2026} ")
    }

    fn render_prompt_history_search_indicator(
        &self,
        history_search: PromptHistorySearch,
    ) -> Cow<'_, str> {
        let prefix = match history_search.status {
            PromptHistorySearchStatus::Passing => "",
            PromptHistorySearchStatus::Failing => "failing ",
        };
        Cow::Owned(format!(
            "({prefix}reverse-search: {}) ",
            history_search.term
        ))
    }
}
