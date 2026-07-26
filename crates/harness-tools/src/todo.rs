//! A structured plan the model can externalize and update across turns —
//! the difference between "trust the model tracked a 6-step task in its
//! head" and "the plan is visible text in the transcript, both to the user
//! watching and to the model's own next turn." Deliberately stateless: each
//! call must pass the *complete* current list (not a diff), so there's
//! nothing to get out of sync — the returned rendering *is* the model's
//! source of truth for what it just declared, echoed straight back.

use async_trait::async_trait;
use serde::Deserialize;
use serde_json::value::RawValue;

use crate::error::ToolError;
use crate::tool::{Tool, obj_schema};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
enum TodoStatus {
    Pending,
    InProgress,
    Completed,
}

#[derive(Deserialize)]
struct TodoItemArgs {
    content: String,
    status: TodoStatus,
}

#[derive(Deserialize)]
struct TodoArgs {
    todos: Vec<TodoItemArgs>,
}

pub struct TodoWrite;

#[async_trait]
impl Tool for TodoWrite {
    fn name(&self) -> &str {
        "todo_write"
    }
    fn description(&self) -> &str {
        "Maintain a visible plan for multi-step work. Pass the FULL current list every call \
         (not a diff) -- each call replaces the previous one entirely. Use it for any task with \
         3+ distinct steps, especially ones spanning many files or turns (e.g. \"scaffold a \
         backend and a frontend and wire them together\"): write the initial breakdown before \
         starting, keep exactly one item in_progress at a time, and mark an item completed \
         immediately after finishing it, not in a batch at the end. Skip this entirely for \
         single-step or trivial requests -- it's overhead, not ceremony."
    }
    fn schema(&self) -> serde_json::Value {
        obj_schema(
            &[(
                "todos",
                serde_json::json!({
                    "type": "array",
                    "description": "the complete current task list, in order",
                    "items": {
                        "type": "object",
                        "properties": {
                            "content": {"type": "string", "description": "imperative description of the step, e.g. \"Scaffold Express backend\""},
                            "status": {"type": "string", "enum": ["pending", "in_progress", "completed"]}
                        },
                        "required": ["content", "status"]
                    }
                }),
            )],
            &["todos"],
        )
    }
    async fn execute(&self, args: &RawValue) -> Result<String, ToolError> {
        let parsed: TodoArgs = serde_json::from_str(args.get())?;
        if parsed.todos.is_empty() {
            return Err(ToolError::Message(
                "todos must not be empty -- pass at least one item".into(),
            ));
        }
        let in_progress = parsed
            .todos
            .iter()
            .filter(|t| t.status == TodoStatus::InProgress)
            .count();
        if in_progress > 1 {
            return Err(ToolError::Message(format!(
                "at most one todo may be in_progress at a time, got {in_progress} -- finish or \
                 park the current one before starting another"
            )));
        }
        Ok(render(&parsed.todos))
    }
}

fn render(todos: &[TodoItemArgs]) -> String {
    todos
        .iter()
        .map(|t| {
            let mark = match t.status {
                TodoStatus::Pending => "[ ]",
                TodoStatus::InProgress => "[~]",
                TodoStatus::Completed => "[x]",
            };
            format!("{mark} {}", t.content)
        })
        .collect::<Vec<_>>()
        .join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(json: serde_json::Value) -> Box<RawValue> {
        RawValue::from_string(json.to_string()).unwrap()
    }

    #[tokio::test]
    async fn renders_a_checkbox_per_item_in_order() {
        let tool = TodoWrite;
        let result = tool
            .execute(&args(serde_json::json!({
                "todos": [
                    {"content": "Scaffold backend", "status": "completed"},
                    {"content": "Scaffold frontend", "status": "in_progress"},
                    {"content": "Wire the two together", "status": "pending"}
                ]
            })))
            .await
            .unwrap();
        assert_eq!(
            result,
            "[x] Scaffold backend\n[~] Scaffold frontend\n[ ] Wire the two together"
        );
    }

    #[tokio::test]
    async fn rejects_an_empty_list() {
        let tool = TodoWrite;
        let err = tool
            .execute(&args(serde_json::json!({"todos": []})))
            .await
            .unwrap_err();
        assert!(err.to_string().contains("must not be empty"));
    }

    #[tokio::test]
    async fn rejects_more_than_one_in_progress_item() {
        let tool = TodoWrite;
        let err = tool
            .execute(&args(serde_json::json!({
                "todos": [
                    {"content": "A", "status": "in_progress"},
                    {"content": "B", "status": "in_progress"}
                ]
            })))
            .await
            .unwrap_err();
        assert!(
            err.to_string()
                .contains("at most one todo may be in_progress")
        );
    }

    #[tokio::test]
    async fn rejects_malformed_arguments() {
        let tool = TodoWrite;
        let result = tool
            .execute(&args(serde_json::json!({"todos": "not-an-array"})))
            .await;
        assert!(result.is_err());
    }
}
