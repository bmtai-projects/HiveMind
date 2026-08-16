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
use tokio::time::Instant as Deadline;

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
    ///
    /// Kept in the same order of magnitude as the primitives it aggregates:
    /// `fs::MAX_READ_BYTES` caps one `read_file` at 60 KB, so this is two
    /// whole-file reads' worth. Well above that the saving stops being real
    /// — anything over `DEFAULT_ARTIFACT_THRESHOLD_BYTES` is offloaded and
    /// the model sees a head/tail preview of pretty-printed JSON, which is
    /// mostly punctuation, and has to spend the round trip this tool exists
    /// to save fetching the artifact.
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
            max_total_bytes: 120_000,
            max_search_results: 50,
            max_map_iterations: 8,
            // Per operation, not for the program as a whole. A repo-wide
            // `search` walks and reads every file under the root, which on a
            // large tree takes seconds — and the plain `search` tool it
            // delegates to has no deadline at all, so a budget tight enough
            // to trip on ordinary work would make this tool strictly worse
            // than the primitives it replaces. Still far under `run_shell`'s
            // 120s, because nothing here should ever run that long.
            execution_timeout_ms: 30_000,
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

#[derive(Deserialize)]
struct RawProgramArgs<'a> {
    #[serde(borrow)]
    operations: Vec<&'a RawValue>,
}

/// Deserialize each operation on its own so a rejected one names itself.
///
/// A malformed operation still rejects the whole program — an argument the
/// executor cannot understand means the model's plan is not the plan that
/// would run, and half-running it is worse than saying so. What this adds is
/// *which* operation: serde's own message ("unknown field `max_results`")
/// is accurate but leaves the model guessing which of sixteen entries it
/// belongs to.
fn parse_operations(args: &RawValue) -> Result<Vec<ReadOperation>, ToolError> {
    let raw: RawProgramArgs = serde_json::from_str(args.get())?;
    raw.operations
        .iter()
        .enumerate()
        .map(|(index, item)| {
            serde_json::from_str::<ReadOperation>(item.get()).map_err(|error| {
                ToolError::Message(format!(
                    "read_program operation {index}{}: {error}",
                    operation_label(item)
                ))
            })
        })
        .collect()
}

/// Best-effort `id`/`op` for an operation that failed to deserialize. Either
/// field may be missing or the wrong type — this is only ever used to make an
/// error message easier to act on, so anything unusable is simply omitted.
fn operation_label(item: &RawValue) -> String {
    let Ok(value) = serde_json::from_str::<Value>(item.get()) else {
        return String::new();
    };
    let field = |name: &str| value.get(name).and_then(Value::as_str).map(str::to_owned);
    match (field("id"), field("op")) {
        (Some(id), Some(op)) => format!(" (id {id:?}, op {op:?})"),
        (Some(id), None) => format!(" (id {id:?})"),
        (None, Some(op)) => format!(" (op {op:?})"),
        (None, None) => String::new(),
    }
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
    truncated: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    note: Option<String>,
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

    // One schema per operation rather than one flat object listing every
    // field. The AST is an internally tagged enum with `deny_unknown_fields`,
    // so `max_results` on a `read_file` is a hard rejection of the *whole*
    // program -- and a flat schema is exactly what invites the model to send
    // it, since it advertises every field as legal on every operation.
    fn schema(&self) -> Value {
        let id = json!({"type": "string", "description": "short unique result name"});
        let query = json!({"type": "string", "description": "literal case-sensitive search text"});
        let search_path = json!({
            "type": "string",
            "description": "workspace-relative directory to search under; omit for the whole workspace",
        });
        let max_results = json!({
            "type": "integer", "minimum": 1, "maximum": self.policy.max_search_results,
        });
        json!({
            "type": "object",
            "properties": {
                "operations": {
                    "type": "array",
                    "minItems": 1,
                    "maxItems": self.policy.max_operations,
                    "description": "Read-only operations, run concurrently. Every id must be unique. Send only the fields listed for the op you choose.",
                    "items": {
                        "oneOf": [
                            {
                                "title": "read_file",
                                "type": "object",
                                "properties": {
                                    "id": id,
                                    "op": {"type": "string", "enum": ["read_file"]},
                                    "path": {"type": "string", "description": "workspace-relative file path"},
                                    "offset": {"type": "integer", "minimum": 1, "description": "1-indexed first line"},
                                    "limit": {"type": "integer", "minimum": 1, "description": "line count from offset"}
                                },
                                "required": ["id", "op", "path"],
                                "additionalProperties": false
                            },
                            {
                                "title": "search",
                                "type": "object",
                                "properties": {
                                    "id": id,
                                    "op": {"type": "string", "enum": ["search"]},
                                    "query": query,
                                    "path": search_path,
                                    "max_results": max_results
                                },
                                "required": ["id", "op", "query"],
                                "additionalProperties": false
                            },
                            {
                                "title": "list_dir",
                                "type": "object",
                                "properties": {
                                    "id": id,
                                    "op": {"type": "string", "enum": ["list_dir"]},
                                    "path": {"type": "string", "description": "workspace-relative directory path; '.' for the root"}
                                },
                                "required": ["id", "op", "path"],
                                "additionalProperties": false
                            },
                            {
                                "title": "search_then_read",
                                "type": "object",
                                "description": "Search, then read bounded context around the first matches, in one call.",
                                "properties": {
                                    "id": id,
                                    "op": {"type": "string", "enum": ["search_then_read"]},
                                    "query": query,
                                    "path": search_path,
                                    "max_results": max_results,
                                    "max_reads": {"type": "integer", "minimum": 1, "maximum": self.policy.max_map_iterations, "description": "how many matches to read around"},
                                    "context_lines": {"type": "integer", "minimum": 1, "maximum": MAX_CONTEXT_LINES, "description": "lines of context each side of a match"}
                                },
                                "required": ["id", "op", "query"],
                                "additionalProperties": false
                            }
                        ]
                    }
                }
            },
            "required": ["operations"],
            "additionalProperties": false
        })
    }

    async fn execute(&self, args: &RawValue) -> Result<ToolResult, ToolError> {
        let requested = parse_operations(args)?;
        self.validate(&requested)?;

        let started = Instant::now();
        let (unique, result_indices) = deduplicate(&requested);
        let unique_count = unique.len();
        let semaphore = Arc::new(Semaphore::new(self.policy.max_parallelism));
        // The deadline is applied per operation rather than to the program as
        // a whole. Timing out the aggregate threw away every result that had
        // already arrived, so one slow repo-wide search cost the model the
        // five reads that finished in milliseconds beside it -- and told it
        // nothing about which operation was the slow one.
        let deadline = Deadline::now() + Duration::from_millis(self.policy.execution_timeout_ms);
        let unique_results = execute_unique(
            unique,
            self.workspace.clone(),
            self.policy,
            semaphore,
            deadline,
        )
        .await;

        let mut operations = Vec::with_capacity(requested.len());
        for (operation, result_index) in requested.iter().zip(result_indices) {
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
        let mut truncated = false;
        for operation in &mut operations {
            operation.truncated = cap_string_payloads(&mut operation.data, &mut remaining);
            truncated |= operation.truncated;
        }

        let any_timeout = unique_results
            .iter()
            .any(|result| result.status == ToolStatus::Timeout);
        let any_failure = unique_results
            .iter()
            .any(|result| result.status.is_failure());
        let any_success = unique_results
            .iter()
            .any(|result| !result.status.is_failure());
        let status = match (any_failure, any_success, any_timeout) {
            (false, _, _) => "success",
            (true, true, _) => "partial",
            (true, false, true) => "timeout",
            (true, false, false) => "failed",
        };
        let wall_ms = elapsed_ms(started);
        let primitive_operations = unique_results.iter().map(|r| r.primitives).sum();
        let result = ProgramResult {
            status,
            truncated,
            // Said once, at the top, because a per-operation `truncated: true`
            // on an emptied string looks exactly like an empty file. This is
            // the same reason `fs::slice_file` labels a partial read.
            note: truncated.then(|| {
                format!(
                    "the shared {}-byte result budget ran out; some values are cut short or \
                     empty. Re-run with fewer operations, a narrower path, or read_file \
                     offset/limit to see the rest.",
                    self.policy.max_total_bytes
                )
            }),
            operations,
            execution: ExecutionMetrics {
                requested_operations: requested.len(),
                executed_operations: unique_count,
                primitive_operations,
                operations_deduplicated: requested.len() - unique_count,
                max_parallelism: self.policy.max_parallelism,
                wall_ms,
            },
        };

        // A composite call that came back with usable results ended fine, even
        // if one operation missed. Reporting `Failed` for a partial success
        // feeds `last_turn_had_failure` into the agent's escalation counter,
        // which buys an automatic jump to a model costing ~25x more -- for one
        // guessed path that did not exist. The per-operation status in the
        // payload is what tells the model what actually happened.
        let tool_status = match (any_success, any_timeout) {
            (true, _) => ToolStatus::Ok,
            (false, true) => ToolStatus::Timeout,
            (false, false) => ToolStatus::Failed,
        };
        Ok(ToolResult {
            status: tool_status,
            summary: serde_json::to_string_pretty(&result)?,
            retryable: tool_status == ToolStatus::Timeout,
            duration_ms: wall_ms,
            ..Default::default()
        })
    }
}

impl ReadProgram {
    fn validate(&self, operations: &[ReadOperation]) -> Result<(), ToolError> {
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
        if operations.is_empty() {
            return Err(ToolError::Message(
                "read_program requires at least one operation".into(),
            ));
        }

        let mut ids = HashSet::new();
        let mut primitives = 0usize;
        for operation in operations {
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

/// Reject a path that escapes the workspace — nothing else.
///
/// A path that simply is not there yet resolves to an IO error, and failing
/// the program on it made one mistyped filename discard every other
/// operation's results. Worse, it did so inconsistently: a missing file in an
/// existing directory already degraded to a per-operation failure, while a
/// missing *directory* aborted everything with a bare "No such file or
/// directory" that named neither the operation nor the path.
///
/// Deferring is safe because the tool that runs the operation resolves the
/// path again and enforces the same boundary; this is orchestration, not a
/// second security check.
fn validate_path(workspace: &Workspace, path: &str) -> Result<(), ToolError> {
    match workspace.resolve(path) {
        Ok(_) | Err(ToolError::Io(_)) => Ok(()),
        Err(escape) => Err(escape),
    }
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
    deadline: Deadline,
) -> Vec<ExecutedOperation> {
    let count = operations.len();
    let mut set = JoinSet::new();
    for (index, operation) in operations.into_iter().enumerate() {
        let workspace = workspace.clone();
        let semaphore = semaphore.clone();
        let name = operation.name();
        set.spawn(async move {
            let run = execute_operation(operation, workspace, policy, semaphore);
            let result = match tokio::time::timeout_at(deadline, run).await {
                Ok(result) => result,
                Err(_) => ExecutedOperation {
                    status: ToolStatus::Timeout,
                    data: json!({
                        "error": format!(
                            "{name} exceeded the {} ms read_program time budget; \
                             narrow the path or query and try again",
                            policy.execution_timeout_ms
                        )
                    }),
                    primitives: 0,
                },
            };
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
        // Normalized the same way `search_workspace` normalizes it, so the
        // echo names the directory that was actually walked.
        "path": path.filter(|p| !p.is_empty()).unwrap_or("."),
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

    #[test]
    fn every_field_the_schema_advertises_is_one_the_ast_accepts() {
        // The mismatch this pins is not theoretical: with a single flat item
        // schema, `max_results` was advertised as legal on `read_file`, and
        // sending it rejected the *whole* program with "unknown field".
        let schema = ReadProgram::new(Workspace::new(std::env::temp_dir())).schema();
        let variants = schema["properties"]["operations"]["items"]["oneOf"]
            .as_array()
            .expect("the item schema is a per-operation oneOf");
        assert_eq!(variants.len(), 4, "one schema per operation");

        for variant in variants {
            let op = variant["title"].as_str().unwrap();
            let mut probe = serde_json::Map::new();
            for (field, spec) in variant["properties"].as_object().unwrap() {
                let value = match (field.as_str(), spec["type"].as_str().unwrap()) {
                    ("op", _) => json!(op),
                    (_, "integer") => json!(1),
                    _ => json!("probe"),
                };
                probe.insert(field.clone(), value);
            }
            let filled = Value::Object(probe).to_string();
            let parsed = serde_json::from_str::<ReadOperation>(&filled);
            assert!(
                parsed.is_ok(),
                "{op} advertises a field the AST rejects: {:?} in {filled}",
                parsed.err()
            );
        }
    }

    #[tokio::test]
    async fn a_rejected_operation_says_which_one_it_was() {
        // serde's own message names the field but not the entry, which is no
        // help when the model sent sixteen of them.
        let workspace = ws("named_error");
        let error = ReadProgram::new(workspace)
            .execute(&args(json!({"operations": [
                {"id": "ok", "op": "list_dir", "path": "."},
                {"id": "muddled", "op": "read_file", "path": "a.txt", "max_results": 3}
            ]})))
            .await
            .unwrap_err()
            .to_string();

        assert!(error.contains("operation 1"), "{error}");
        assert!(error.contains("muddled"), "{error}");
        assert!(error.contains("max_results"), "{error}");
    }

    #[tokio::test]
    async fn a_path_that_is_not_there_fails_only_its_own_operation() {
        // A missing file in an existing directory always degraded to a single
        // failed operation; a missing *directory* aborted the whole program
        // with a bare "No such file or directory". Same mistake, same blast
        // radius -- one of the two was just louder about it.
        let workspace = ws("missing_dir");
        std::fs::write(workspace.root.join("real.txt"), "content\n").unwrap();

        let result = ReadProgram::new(workspace)
            .execute(&args(json!({"operations": [
                {"id": "real", "op": "read_file", "path": "real.txt"},
                {"id": "guessed", "op": "read_file", "path": "no/such/dir/file.rs"}
            ]})))
            .await
            .unwrap();

        let output: Value = serde_json::from_str(&result.summary).unwrap();
        assert_eq!(output["status"], "partial");
        assert_eq!(output["operations"][0]["status"], "ok");
        assert_eq!(output["operations"][0]["data"]["content"], "content\n");
        assert_eq!(output["operations"][1]["id"], "guessed");
        assert_eq!(output["operations"][1]["status"], "failed");
    }

    #[tokio::test]
    async fn one_missed_read_does_not_report_the_whole_call_as_failed() {
        // `ToolStatus::Failed` is not cosmetic here: `Agent::update_escalation`
        // counts it and can switch to a model costing ~25x more. A discovery
        // program where one guessed path missed is not a failed tool call.
        let workspace = ws("partial_status");
        std::fs::write(workspace.root.join("real.txt"), "content\n").unwrap();

        let result = ReadProgram::new(workspace)
            .execute(&args(json!({"operations": [
                {"id": "real", "op": "read_file", "path": "real.txt"},
                {"id": "gone", "op": "read_file", "path": "gone.txt"}
            ]})))
            .await
            .unwrap();

        assert_eq!(result.status, ToolStatus::Ok);
        let output: Value = serde_json::from_str(&result.summary).unwrap();
        assert_eq!(
            output["status"], "partial",
            "the model still has to be told, in the payload it reads"
        );
    }

    #[tokio::test]
    async fn every_operation_failing_is_still_a_failed_call() {
        let workspace = ws("all_failed");
        let result = ReadProgram::new(workspace)
            .execute(&args(json!({"operations": [
                {"id": "gone", "op": "read_file", "path": "gone.txt"}
            ]})))
            .await
            .unwrap();

        assert_eq!(result.status, ToolStatus::Failed);
        let output: Value = serde_json::from_str(&result.summary).unwrap();
        assert_eq!(output["status"], "failed");
    }

    #[tokio::test]
    async fn a_timeout_reports_per_operation_results_instead_of_discarding_them() {
        // The whole program used to share one deadline, so a slow repo-wide
        // search cost the model every read that had already finished beside
        // it -- and the error named no operation, so it could not even retry
        // the program without the slow part.
        let workspace = ws("timeout");
        for i in 0..800 {
            std::fs::write(
                workspace.root.join(format!("f{i}.txt")),
                "filler line\n".repeat(400),
            )
            .unwrap();
        }
        let policy = ReadProgramPolicy {
            execution_timeout_ms: 1,
            ..ReadProgramPolicy::default()
        };

        let result = ReadProgram::new(workspace)
            .with_policy(policy)
            .execute(&args(json!({"operations": [
                // A query with no match anywhere, so the walk cannot stop
                // early at max_results after the first file.
                {"id": "slow", "op": "search", "query": "no-such-token-anywhere"},
                {"id": "quick", "op": "list_dir", "path": "."}
            ]})))
            .await
            .unwrap();

        let output: Value = serde_json::from_str(&result.summary).unwrap();
        let operations = output["operations"].as_array().unwrap();
        assert_eq!(operations.len(), 2, "both operations must be accounted for");
        assert_eq!(operations[0]["id"], "slow");
        assert_eq!(operations[0]["status"], "timeout");
        assert!(
            operations[0]["data"]["error"]
                .as_str()
                .unwrap()
                .contains("search"),
            "the timed-out operation must name itself: {}",
            operations[0]["data"]["error"]
        );
        assert_eq!(operations[1]["id"], "quick");
        assert_eq!(output["execution"]["requested_operations"], 2);
    }

    #[tokio::test]
    async fn an_exhausted_budget_is_announced_once_at_the_top() {
        // A per-operation `truncated: true` beside an emptied string reads
        // exactly like an empty file -- the failure `fs::slice_file` already
        // labels its partial reads to avoid.
        let workspace = ws("budget_note");
        std::fs::write(workspace.root.join("big.txt"), "x".repeat(1_000)).unwrap();
        let policy = ReadProgramPolicy {
            max_total_bytes: 32,
            ..ReadProgramPolicy::default()
        };

        let result = ReadProgram::new(workspace)
            .with_policy(policy)
            .execute(&args(json!({"operations": [
                {"id": "big", "op": "read_file", "path": "big.txt"}
            ]})))
            .await
            .unwrap();

        let output: Value = serde_json::from_str(&result.summary).unwrap();
        assert_eq!(output["truncated"], true);
        assert!(
            output["note"].as_str().unwrap().contains("32-byte"),
            "{}",
            output["note"]
        );
    }

    #[tokio::test]
    async fn a_whole_result_that_fits_carries_no_truncation_note() {
        let workspace = ws("no_note");
        std::fs::write(workspace.root.join("small.txt"), "hi\n").unwrap();
        let result = ReadProgram::new(workspace)
            .execute(&args(json!({"operations": [
                {"id": "small", "op": "read_file", "path": "small.txt"}
            ]})))
            .await
            .unwrap();

        let output: Value = serde_json::from_str(&result.summary).unwrap();
        assert_eq!(output["truncated"], false);
        assert!(output.get("note").is_none(), "{output}");
    }

    #[tokio::test]
    async fn an_empty_search_path_echoes_the_directory_actually_walked() {
        let workspace = ws("echo_path");
        std::fs::write(workspace.root.join("a.txt"), "needle\n").unwrap();
        let result = ReadProgram::new(workspace)
            .execute(&args(json!({"operations": [
                {"id": "s", "op": "search", "query": "needle", "path": ""}
            ]})))
            .await
            .unwrap();

        let output: Value = serde_json::from_str(&result.summary).unwrap();
        assert_eq!(output["operations"][0]["data"]["path"], ".");
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
