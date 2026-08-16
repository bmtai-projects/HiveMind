//! A bounded, read-only composite tool for repository discovery.
//!
//! The provider already gives HiveMind a typed JSON tool-call envelope, so
//! this deliberately uses that as its AST instead of parsing model-authored
//! source code. Only the operations represented by [`ReadOperation`] can run.
//! Execution delegates to the existing file tools and workspace resolver;
//! this module is orchestration, never a second security boundary.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::{Duration, Instant};

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::value::RawValue;
use serde_json::{Value, json};
use tokio::sync::Semaphore;
use tokio::task::JoinSet;

use crate::error::ToolError;
use crate::fs::{ListDir, ReadFile, Workspace};
use crate::search::{SearchHit, search_workspace};
use crate::tool::{Tool, ToolResult, ToolStatus};

const DEFAULT_SEARCH_RESULTS: usize = 50;
const DEFAULT_SEARCH_READS: usize = 5;
const DEFAULT_CONTEXT_LINES: usize = 40;
const MAX_CONTEXT_LINES: usize = 200;

/// Resource policy for one `read_program` invocation.
#[derive(Debug, Clone, Copy)]
pub struct ReadProgramPolicy {
    /// Maximum primitive operations after bounded search fan-out is counted.
    pub max_operations: usize,
    pub max_parallelism: usize,
    /// Budget for string payloads in the aggregate result. Structural JSON
    /// metadata is intentionally not charged against this budget.
    pub max_total_bytes: usize,
    pub max_search_results: usize,
    pub max_map_iterations: usize,
    pub execution_timeout_ms: u64,
}

impl Default for ReadProgramPolicy {
    fn default() -> Self {
        Self {
            max_operations: 16,
            max_parallelism: 6,
            max_total_bytes: 1_000_000,
            max_search_results: 50,
            max_map_iterations: 8,
            execution_timeout_ms: 3_000,
        }
    }
}

/// The single model-visible composite capability. It has no conflict key:
/// every operation it can represent is read-only.
pub struct ReadProgram {
    workspace: Workspace,
    policy: ReadProgramPolicy,
}

impl ReadProgram {
    pub fn new(workspace: Workspace) -> Self {
        Self {
            workspace,
            policy: ReadProgramPolicy::default(),
        }
    }

    pub fn with_policy(mut self, policy: ReadProgramPolicy) -> Self {
        self.policy = policy;
        self
    }
}

#[derive(Debug, Clone, Deserialize)]
struct ReadProgramArgs {
    operations: Vec<ReadOperation>,
}

/// The closed AST. Adding a field cannot add a capability; adding a new
/// operation requires changing this enum and its validator/executor.
#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case", deny_unknown_fields)]
enum ReadOperation {
    ReadFile {
        id: String,
        path: String,
        #[serde(default)]
        offset: Option<usize>,
        #[serde(default)]
        limit: Option<usize>,
    },
    Search {
        id: String,
        query: String,
        #[serde(default)]
        path: Option<String>,
        #[serde(default)]
        max_results: Option<usize>,
    },
    ListDir {
        id: String,
        path: String,
    },
    SearchThenRead {
        id: String,
        query: String,
        #[serde(default)]
        path: Option<String>,
        #[serde(default)]
        max_results: Option<usize>,
        #[serde(default)]
        max_reads: Option<usize>,
        #[serde(default)]
        context_lines: Option<usize>,
    },
}

impl ReadOperation {
    fn id(&self) -> &str {
        match self {
            Self::ReadFile { id, .. }
            | Self::Search { id, .. }
            | Self::ListDir { id, .. }
            | Self::SearchThenRead { id, .. } => id,
        }
    }

    fn name(&self) -> &'static str {
        match self {
            Self::ReadFile { .. } => "read_file",
            Self::Search { .. } => "search",
            Self::ListDir { .. } => "list_dir",
            Self::SearchThenRead { .. } => "search_then_read",
        }
    }

    /// Semantic identity without the caller-chosen result id. Two operations
    /// with different ids but the same capability/arguments execute once.
    fn execution_key(&self) -> String {
        match self {
            Self::ReadFile {
                path,
                offset,
                limit,
                ..
            } => json!({"op": "read_file", "path": path, "offset": offset, "limit": limit}),
            Self::Search {
                query,
                path,
                max_results,
                ..
            } => json!({"op": "search", "query": query, "path": path, "max_results": max_results}),
            Self::ListDir { path, .. } => json!({"op": "list_dir", "path": path}),
            Self::SearchThenRead {
                query,
                path,
                max_results,
                max_reads,
                context_lines,
                ..
            } => json!({
                "op": "search_then_read",
                "query": query,
                "path": path,
                "max_results": max_results,
                "max_reads": max_reads,
                "context_lines": context_lines,
            }),
        }
        .to_string()
    }

    fn planned_primitives(&self, policy: ReadProgramPolicy) -> usize {
        match self {
            Self::SearchThenRead {
                max_results,
                max_reads,
                ..
            } => {
                let results = max_results
                    .unwrap_or(DEFAULT_SEARCH_RESULTS)
                    .min(policy.max_search_results);
                let reads = max_reads.unwrap_or(DEFAULT_SEARCH_READS);
                1 + reads.min(results).min(policy.max_map_iterations)
            }
            _ => 1,
        }
    }
}

#[derive(Debug, Clone)]
struct ExecutedOperation {
    status: ToolStatus,
    data: Value,
    primitives: usize,
}

#[derive(Debug, Serialize)]
struct ProgramOperationResult {
    id: String,
    operation: &'static str,
    status: &'static str,
    data: Value,
    truncated: bool,
}

#[derive(Debug, Serialize)]
struct ExecutionMetrics {
    requested_operations: usize,
    executed_operations: usize,
    primitive_operations: usize,
    operations_deduplicated: usize,
    max_parallelism: usize,
    wall_ms: u64,
}

#[derive(Debug, Serialize)]
struct ProgramResult {
    status: &'static str,
    operations: Vec<ProgramOperationResult>,
    execution: ExecutionMetrics,
}

#[async_trait]
impl Tool for ReadProgram {
    fn name(&self) -> &str {
        "read_program"
    }

    fn description(&self) -> &str {
        "Run one bounded read-only repository program and return one structured result. Use it \
         for several independent read_file/search/list_dir operations, or use search_then_read \
         to search and read bounded context around matching lines without another model turn. \
         It cannot write files, run commands, use Git/network, or escape the workspace. Keep \
         ordinary single reads on their existing tools."
    }

    fn schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "operations": {
                    "type": "array",
                    "minItems": 1,
                    "maxItems": self.policy.max_operations,
                    "description": "Read-only operations. Every id must be unique. Operation-specific fields are validated before anything runs.",
                    "items": {
                        "type": "object",
                        "properties": {
                            "id": {"type": "string", "description": "short unique result name"},
                            "op": {"type": "string", "enum": ["read_file", "search", "list_dir", "search_then_read"]},
                            "path": {"type": "string", "description": "workspace-relative path; optional for search operations"},
                            "query": {"type": "string", "description": "literal case-sensitive search text"},
                            "offset": {"type": "integer", "minimum": 1},
                            "limit": {"type": "integer", "minimum": 1},
                            "max_results": {"type": "integer", "minimum": 1, "maximum": self.policy.max_search_results},
                            "max_reads": {"type": "integer", "minimum": 1, "maximum": self.policy.max_map_iterations},
                            "context_lines": {"type": "integer", "minimum": 1, "maximum": MAX_CONTEXT_LINES}
                        },
                        "required": ["id", "op"],
                        "additionalProperties": false
                    }
                }
            },
            "required": ["operations"],
            "additionalProperties": false
        })
    }

    async fn execute(&self, args: &RawValue) -> Result<ToolResult, ToolError> {
        let args: ReadProgramArgs = serde_json::from_str(args.get())?;
        self.validate(&args)?;

        let started = Instant::now();
        let (unique, result_indices) = deduplicate(&args.operations);
        let unique_count = unique.len();
        let semaphore = Arc::new(Semaphore::new(self.policy.max_parallelism));
        let run = execute_unique(unique, self.workspace.clone(), self.policy, semaphore);

        let unique_results = match tokio::time::timeout(
            Duration::from_millis(self.policy.execution_timeout_ms),
            run,
        )
        .await
        {
            Ok(results) => results,
            Err(_) => {
                let wall_ms = elapsed_ms(started);
                let summary = serde_json::to_string_pretty(&json!({
                    "status": "timeout",
                    "error": format!(
                        "read program exceeded {} ms",
                        self.policy.execution_timeout_ms
                    ),
                    "execution": {
                        "requested_operations": args.operations.len(),
                        "executed_operations": unique_count,
                        "operations_deduplicated": args.operations.len() - unique_count,
                        "max_parallelism": self.policy.max_parallelism,
                        "wall_ms": wall_ms,
                    }
                }))?;
                return Ok(ToolResult {
                    status: ToolStatus::Timeout,
                    summary,
                    retryable: true,
                    duration_ms: wall_ms,
                    ..Default::default()
                });
            }
        };

        let mut operations = Vec::with_capacity(args.operations.len());
        for (operation, result_index) in args.operations.iter().zip(result_indices) {
            let executed = &unique_results[result_index];
            operations.push(ProgramOperationResult {
                id: operation.id().to_string(),
                operation: operation.name(),
                status: status_name(executed.status),
                data: executed.data.clone(),
                truncated: false,
            });
        }

        let mut remaining = self.policy.max_total_bytes;
        for operation in &mut operations {
            operation.truncated = cap_string_payloads(&mut operation.data, &mut remaining);
        }

        let any_failure = unique_results
            .iter()
            .any(|result| result.status.is_failure());
        let any_success = unique_results
            .iter()
            .any(|result| !result.status.is_failure());
        let status = match (any_failure, any_success) {
            (false, _) => "success",
            (true, true) => "partial",
            (true, false) => "failed",
        };
        let wall_ms = elapsed_ms(started);
        let primitive_operations = unique_results.iter().map(|r| r.primitives).sum();
        let result = ProgramResult {
            status,
            operations,
            execution: ExecutionMetrics {
                requested_operations: args.operations.len(),
                executed_operations: unique_count,
                primitive_operations,
                operations_deduplicated: args.operations.len() - unique_count,
                max_parallelism: self.policy.max_parallelism,
                wall_ms,
            },
        };

        Ok(ToolResult {
            status: if any_failure {
                ToolStatus::Failed
            } else {
                ToolStatus::Ok
            },
            summary: serde_json::to_string_pretty(&result)?,
            duration_ms: wall_ms,
            ..Default::default()
        })
    }
}

impl ReadProgram {
    fn validate(&self, args: &ReadProgramArgs) -> Result<(), ToolError> {
        if self.policy.max_operations == 0
            || self.policy.max_parallelism == 0
            || self.policy.max_total_bytes == 0
            || self.policy.max_search_results == 0
            || self.policy.max_map_iterations == 0
            || self.policy.execution_timeout_ms == 0
        {
            return Err(ToolError::Message(
                "read_program policy limits must all be greater than zero".into(),
            ));
        }
        if args.operations.is_empty() {
            return Err(ToolError::Message(
                "read_program requires at least one operation".into(),
            ));
        }

        let mut ids = HashSet::new();
        let mut primitives = 0usize;
        for operation in &args.operations {
            validate_id(operation.id())?;
            if !ids.insert(operation.id()) {
                return Err(ToolError::Message(format!(
                    "duplicate read_program operation id {:?}",
                    operation.id()
                )));
            }

            match operation {
                ReadOperation::ReadFile {
                    path,
                    offset,
                    limit,
                    ..
                } => {
                    validate_path(&self.workspace, path)?;
                    validate_positive("offset", *offset)?;
                    validate_positive("limit", *limit)?;
                }
                ReadOperation::Search {
                    query,
                    path,
                    max_results,
                    ..
                } => {
                    validate_search(
                        &self.workspace,
                        query,
                        path.as_deref(),
                        *max_results,
                        self.policy,
                    )?;
                }
                ReadOperation::ListDir { path, .. } => {
                    validate_path(&self.workspace, path)?;
                }
                ReadOperation::SearchThenRead {
                    query,
                    path,
                    max_results,
                    max_reads,
                    context_lines,
                    ..
                } => {
                    validate_search(
                        &self.workspace,
                        query,
                        path.as_deref(),
                        *max_results,
                        self.policy,
                    )?;
                    validate_bounded("max_reads", *max_reads, self.policy.max_map_iterations)?;
                    validate_bounded("context_lines", *context_lines, MAX_CONTEXT_LINES)?;
                }
            }

            primitives = primitives.saturating_add(operation.planned_primitives(self.policy));
        }

        if primitives > self.policy.max_operations {
            return Err(ToolError::Message(format!(
                "read_program plans {primitives} primitive operations; maximum is {}",
                self.policy.max_operations
            )));
        }
        Ok(())
    }
}

fn validate_id(id: &str) -> Result<(), ToolError> {
    let valid = !id.is_empty()
        && id.len() <= 64
        && id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-');
    if valid {
        Ok(())
    } else {
        Err(ToolError::Message(format!(
            "operation id {id:?} must be 1-64 ASCII letters, digits, '_' or '-'"
        )))
    }
}

fn validate_path(workspace: &Workspace, path: &str) -> Result<(), ToolError> {
    workspace.resolve(path).map(|_| ())
}

fn validate_positive(name: &str, value: Option<usize>) -> Result<(), ToolError> {
    if value == Some(0) {
        Err(ToolError::Message(format!(
            "{name} must be greater than zero"
        )))
    } else {
        Ok(())
    }
}

fn validate_bounded(name: &str, value: Option<usize>, max: usize) -> Result<(), ToolError> {
    match value {
        Some(0) => Err(ToolError::Message(format!(
            "{name} must be greater than zero"
        ))),
        Some(value) if value > max => Err(ToolError::Message(format!(
            "{name} {value} exceeds the maximum {max}"
        ))),
        _ => Ok(()),
    }
}

fn validate_search(
    workspace: &Workspace,
    query: &str,
    path: Option<&str>,
    max_results: Option<usize>,
    policy: ReadProgramPolicy,
) -> Result<(), ToolError> {
    if query.is_empty() {
        return Err(ToolError::Message("query is required".into()));
    }
    validate_path(workspace, path.filter(|p| !p.is_empty()).unwrap_or("."))?;
    validate_bounded("max_results", max_results, policy.max_search_results)
}

fn deduplicate(operations: &[ReadOperation]) -> (Vec<ReadOperation>, Vec<usize>) {
    let mut unique = Vec::new();
    let mut by_key = HashMap::new();
    let mut indices = Vec::with_capacity(operations.len());

    for operation in operations {
        let key = operation.execution_key();
        let index = match by_key.get(&key) {
            Some(index) => *index,
            None => {
                let index = unique.len();
                unique.push(operation.clone());
                by_key.insert(key, index);
                index
            }
        };
        indices.push(index);
    }
    (unique, indices)
}

async fn execute_unique(
    operations: Vec<ReadOperation>,
    workspace: Workspace,
    policy: ReadProgramPolicy,
    semaphore: Arc<Semaphore>,
) -> Vec<ExecutedOperation> {
    let count = operations.len();
    let mut set = JoinSet::new();
    for (index, operation) in operations.into_iter().enumerate() {
        let workspace = workspace.clone();
        let semaphore = semaphore.clone();
        set.spawn(async move {
            let result = execute_operation(operation, workspace, policy, semaphore).await;
            (index, result)
        });
    }

    let mut slots: Vec<Option<ExecutedOperation>> =
        std::iter::repeat_with(|| None).take(count).collect();
    while let Some(joined) = set.join_next().await {
        if let Ok((index, result)) = joined {
            slots[index] = Some(result);
        }
    }

    slots
        .into_iter()
        .map(|slot| {
            slot.unwrap_or_else(|| ExecutedOperation {
                status: ToolStatus::Failed,
                data: json!({"error": "read_program operation panicked"}),
                primitives: 0,
            })
        })
        .collect()
}

async fn execute_operation(
    operation: ReadOperation,
    workspace: Workspace,
    policy: ReadProgramPolicy,
    semaphore: Arc<Semaphore>,
) -> ExecutedOperation {
    match operation {
        ReadOperation::ReadFile {
            path,
            offset,
            limit,
            ..
        } => execute_read_file(workspace, path, offset, limit, semaphore).await,
        ReadOperation::Search {
            query,
            path,
            max_results,
            ..
        } => {
            execute_search(
                workspace,
                query,
                path,
                max_results
                    .unwrap_or(DEFAULT_SEARCH_RESULTS)
                    .min(policy.max_search_results),
                semaphore,
            )
            .await
        }
        ReadOperation::ListDir { path, .. } => execute_list_dir(workspace, path, semaphore).await,
        ReadOperation::SearchThenRead {
            query,
            path,
            max_results,
            max_reads,
            context_lines,
            ..
        } => {
            execute_search_then_read(
                workspace,
                query,
                path,
                max_results
                    .unwrap_or(DEFAULT_SEARCH_RESULTS)
                    .min(policy.max_search_results),
                max_reads
                    .unwrap_or(DEFAULT_SEARCH_READS)
                    .min(policy.max_map_iterations),
                context_lines.unwrap_or(DEFAULT_CONTEXT_LINES),
                semaphore,
            )
            .await
        }
    }
}

async fn execute_read_file(
    workspace: Workspace,
    path: String,
    offset: Option<usize>,
    limit: Option<usize>,
    semaphore: Arc<Semaphore>,
) -> ExecutedOperation {
    let _permit = semaphore.acquire_owned().await.expect("semaphore is open");
    let args = raw(json!({"path": path, "offset": offset, "limit": limit}));
    match ReadFile(workspace).execute(&args).await {
        Ok(result) => ExecutedOperation {
            status: result.status,
            data: json!({
                "path": path,
                "offset": offset,
                "limit": limit,
                "content": result.summary,
            }),
            primitives: 1,
        },
        Err(error) => failed_execution(error, 1),
    }
}

async fn execute_search(
    workspace: Workspace,
    query: String,
    path: Option<String>,
    max_results: usize,
    semaphore: Arc<Semaphore>,
) -> ExecutedOperation {
    let _permit = semaphore.acquire_owned().await.expect("semaphore is open");
    match search_workspace(&workspace, &query, path.as_deref(), max_results).await {
        Ok(output) => ExecutedOperation {
            status: ToolStatus::Ok,
            data: search_data(&query, path.as_deref(), &output.hits, output.truncated),
            primitives: 1,
        },
        Err(error) => failed_execution(error, 1),
    }
}

async fn execute_list_dir(
    workspace: Workspace,
    path: String,
    semaphore: Arc<Semaphore>,
) -> ExecutedOperation {
    let _permit = semaphore.acquire_owned().await.expect("semaphore is open");
    let args = raw(json!({"path": path}));
    match ListDir(workspace).execute(&args).await {
        Ok(result) => ExecutedOperation {
            status: result.status,
            data: json!({"path": path, "content": result.summary}),
            primitives: 1,
        },
        Err(error) => failed_execution(error, 1),
    }
}

async fn execute_search_then_read(
    workspace: Workspace,
    query: String,
    path: Option<String>,
    max_results: usize,
    max_reads: usize,
    context_lines: usize,
    semaphore: Arc<Semaphore>,
) -> ExecutedOperation {
    // The search releases its permit before dependent reads are queued. This
    // avoids a nested-semaphore deadlock while keeping one global concurrency
    // ceiling across independent operations and fan-out reads.
    let search = {
        let _permit = semaphore
            .clone()
            .acquire_owned()
            .await
            .expect("semaphore is open");
        search_workspace(&workspace, &query, path.as_deref(), max_results).await
    };
    let output = match search {
        Ok(output) => output,
        Err(error) => return failed_execution(error, 1),
    };

    let search_data = search_data(&query, path.as_deref(), &output.hits, output.truncated);
    let selected: Vec<SearchHit> = output.hits.iter().take(max_reads).cloned().collect();
    let selected_count = selected.len();
    let mut set = JoinSet::new();
    for (index, hit) in selected.into_iter().enumerate() {
        let workspace = workspace.clone();
        let semaphore = semaphore.clone();
        set.spawn(async move {
            let offset = hit.line.saturating_sub(context_lines).max(1);
            let limit = context_lines.saturating_mul(2).saturating_add(1);
            let result = execute_read_file(
                workspace,
                hit.path.clone(),
                Some(offset),
                Some(limit),
                semaphore,
            )
            .await;
            (index, hit, offset, limit, result)
        });
    }

    let mut slots: Vec<Option<(SearchHit, usize, usize, ExecutedOperation)>> =
        std::iter::repeat_with(|| None)
            .take(selected_count)
            .collect();
    while let Some(joined) = set.join_next().await {
        if let Ok((index, hit, offset, limit, result)) = joined {
            slots[index] = Some((hit, offset, limit, result));
        }
    }

    let mut failed = false;
    let reads: Vec<Value> = slots
        .into_iter()
        .map(|slot| match slot {
            Some((hit, offset, limit, result)) => {
                failed |= result.status.is_failure();
                json!({
                    "path": hit.path,
                    "match_line": hit.line,
                    "offset": offset,
                    "limit": limit,
                    "status": status_name(result.status),
                    "result": result.data,
                })
            }
            None => {
                failed = true;
                json!({"status": "failed", "error": "dependent read panicked"})
            }
        })
        .collect();

    ExecutedOperation {
        status: if failed {
            ToolStatus::Failed
        } else {
            ToolStatus::Ok
        },
        data: json!({"search": search_data, "reads": reads}),
        primitives: 1 + selected_count,
    }
}

fn search_data(query: &str, path: Option<&str>, hits: &[SearchHit], truncated: bool) -> Value {
    let results: Vec<Value> = hits
        .iter()
        .map(|hit| json!({"path": hit.path, "line": hit.line, "text": hit.text}))
        .collect();
    json!({
        "query": query,
        "path": path.unwrap_or("."),
        "count": hits.len(),
        "truncated": truncated,
        "results": results,
    })
}

fn failed_execution(error: ToolError, primitives: usize) -> ExecutedOperation {
    ExecutedOperation {
        status: ToolStatus::Failed,
        data: json!({"error": error.to_string()}),
        primitives,
    }
}

fn raw(value: Value) -> Box<RawValue> {
    RawValue::from_string(value.to_string()).expect("serde_json::Value is valid JSON")
}

fn status_name(status: ToolStatus) -> &'static str {
    match status {
        ToolStatus::Ok => "ok",
        ToolStatus::Failed => "failed",
        ToolStatus::Denied => "denied",
        ToolStatus::Timeout => "timeout",
    }
}

fn elapsed_ms(started: Instant) -> u64 {
    started.elapsed().as_millis().min(u128::from(u64::MAX)) as u64
}

/// Deterministically trim string leaves while retaining the JSON structure,
/// operation ids, status, and execution metrics around them.
fn cap_string_payloads(value: &mut Value, remaining: &mut usize) -> bool {
    match value {
        Value::String(text) => {
            if text.len() <= *remaining {
                *remaining -= text.len();
                false
            } else {
                let end = char_boundary_at_or_before(text, *remaining);
                text.truncate(end);
                *remaining = 0;
                true
            }
        }
        Value::Array(values) => {
            let mut truncated = false;
            for value in values {
                // Visit every value even after the first truncation: later
                // strings must be emptied once the shared budget is spent.
                truncated |= cap_string_payloads(value, remaining);
            }
            truncated
        }
        Value::Object(values) => {
            let mut truncated = false;
            for value in values.values_mut() {
                truncated |= cap_string_payloads(value, remaining);
            }
            truncated
        }
        _ => false,
    }
}

fn char_boundary_at_or_before(text: &str, max: usize) -> usize {
    let mut end = max.min(text.len());
    while end > 0 && !text.is_char_boundary(end) {
        end -= 1;
    }
    end
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ws(name: &str) -> Workspace {
        let dir = std::env::temp_dir().join(format!(
            "hivemind_read_program_test_{name}_{}_{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        Workspace::new(dir)
    }

    fn args(value: Value) -> Box<RawValue> {
        raw(value)
    }

    #[tokio::test]
    async fn independent_reads_return_one_ordered_structured_result() {
        let workspace = ws("parallel");
        std::fs::write(workspace.root.join("a.txt"), "alpha\n").unwrap();
        std::fs::write(workspace.root.join("b.txt"), "beta\n").unwrap();

        let result = ReadProgram::new(workspace)
            .execute(&args(json!({"operations": [
                {"id": "a", "op": "read_file", "path": "a.txt"},
                {"id": "b", "op": "read_file", "path": "b.txt"}
            ]})))
            .await
            .unwrap();

        assert_eq!(result.status, ToolStatus::Ok);
        let output: Value = serde_json::from_str(&result.summary).unwrap();
        assert_eq!(output["status"], "success");
        assert_eq!(output["operations"][0]["id"], "a");
        assert_eq!(output["operations"][0]["data"]["content"], "alpha\n");
        assert_eq!(output["operations"][1]["id"], "b");
        assert_eq!(output["execution"]["primitive_operations"], 2);
    }

    #[tokio::test]
    async fn search_then_read_follows_typed_hits_with_bounded_context() {
        let workspace = ws("follow");
        let body = (1..=100)
            .map(|line| {
                if line == 50 {
                    "needle here".to_string()
                } else {
                    format!("line {line}")
                }
            })
            .collect::<Vec<_>>()
            .join("\n");
        std::fs::write(workspace.root.join("code.rs"), body).unwrap();

        let result = ReadProgram::new(workspace)
            .execute(&args(json!({"operations": [{
                "id": "found",
                "op": "search_then_read",
                "query": "needle",
                "path": ".",
                "max_results": 5,
                "max_reads": 2,
                "context_lines": 2
            }]})))
            .await
            .unwrap();

        let output: Value = serde_json::from_str(&result.summary).unwrap();
        let operation = &output["operations"][0];
        assert_eq!(operation["data"]["search"]["count"], 1);
        assert_eq!(operation["data"]["reads"][0]["match_line"], 50);
        assert_eq!(operation["data"]["reads"][0]["offset"], 48);
        assert!(
            operation["data"]["reads"][0]["result"]["content"]
                .as_str()
                .unwrap()
                .contains("needle here")
        );
        assert_eq!(output["execution"]["primitive_operations"], 2);
    }

    #[tokio::test]
    async fn identical_operations_are_executed_once_but_keep_both_ids() {
        let workspace = ws("dedup");
        std::fs::write(workspace.root.join("same.txt"), "same").unwrap();
        let result = ReadProgram::new(workspace)
            .execute(&args(json!({"operations": [
                {"id": "first", "op": "read_file", "path": "same.txt"},
                {"id": "second", "op": "read_file", "path": "same.txt"}
            ]})))
            .await
            .unwrap();

        let output: Value = serde_json::from_str(&result.summary).unwrap();
        assert_eq!(output["operations"].as_array().unwrap().len(), 2);
        assert_eq!(output["execution"]["executed_operations"], 1);
        assert_eq!(output["execution"]["operations_deduplicated"], 1);
        assert_eq!(output["execution"]["primitive_operations"], 1);
    }

    #[tokio::test]
    async fn validation_rejects_workspace_escape_before_execution() {
        let workspace = ws("escape");
        std::fs::write(workspace.root.join("safe.txt"), "safe").unwrap();
        let error = ReadProgram::new(workspace)
            .execute(&args(json!({"operations": [
                {"id": "safe", "op": "read_file", "path": "safe.txt"},
                {"id": "escape", "op": "read_file", "path": "../outside.txt"}
            ]})))
            .await
            .unwrap_err();

        assert!(
            error.to_string().contains("escapes the workspace"),
            "{error}"
        );
    }

    #[tokio::test]
    async fn validation_counts_bounded_fanout_as_real_operations() {
        let workspace = ws("fanout");
        let policy = ReadProgramPolicy {
            max_operations: 3,
            ..ReadProgramPolicy::default()
        };
        let error = ReadProgram::new(workspace)
            .with_policy(policy)
            .execute(&args(json!({"operations": [{
                "id": "too_wide",
                "op": "search_then_read",
                "query": "x",
                "max_results": 5,
                "max_reads": 3
            }]})))
            .await
            .unwrap_err();

        assert!(error.to_string().contains("plans 4 primitive operations"));
    }

    #[tokio::test]
    async fn aggregate_string_budget_truncates_content_without_breaking_json() {
        let workspace = ws("budget");
        std::fs::write(workspace.root.join("large.txt"), "x".repeat(1_000)).unwrap();
        let policy = ReadProgramPolicy {
            max_total_bytes: 32,
            ..ReadProgramPolicy::default()
        };
        let result = ReadProgram::new(workspace)
            .with_policy(policy)
            .execute(&args(json!({"operations": [{
                "id": "large", "op": "read_file", "path": "large.txt"
            }]})))
            .await
            .unwrap();

        let output: Value = serde_json::from_str(&result.summary).unwrap();
        assert_eq!(output["operations"][0]["truncated"], true);
        assert!(
            output["operations"][0]["data"]["content"]
                .as_str()
                .unwrap()
                .len()
                <= 32
        );
    }

    #[tokio::test]
    async fn non_read_capabilities_are_not_in_the_ast() {
        let workspace = ws("allowlist");
        let error = ReadProgram::new(workspace)
            .execute(&args(json!({"operations": [{
                "id": "nope", "op": "run_shell", "command": "echo nope"
            }]})))
            .await
            .unwrap_err();
        assert!(error.to_string().contains("unknown variant"), "{error}");
    }
}
