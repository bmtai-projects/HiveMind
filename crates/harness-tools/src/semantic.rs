//! `semantic_search` — ranked similarity search over the codebase, the
//! retrieval half of what "modern agents" (Cursor, Claude, etc.) do: index
//! the repo into vectors, embed the query, and return the closest chunks by
//! cosine similarity, so the model can ask *"where do we handle rate
//! limiting?"* and get the throttling code back even when it never says
//! "rate limit".
//!
//! # Prototype scope — what's real vs. swappable
//!
//! The whole pipeline here is real: chunking, an incremental (fingerprint-
//! invalidated) index cached across turns, cosine ranking, workspace
//! confinement, cost-bounded output. The one deliberately simple piece is the
//! [`Embedder`]: the default [`HashingEmbedder`] is a *local, zero-dependency*
//! feature-hashing embedder (tokens + char-trigrams → signed hashed vector).
//! That keeps the shipped binary lean and lets this compile, test, and run on
//! every release target with no model download — but it captures *lexical /
//! fuzzy* similarity, not true synonymy ("throttle" ≈ "rate limit" won't
//! score highly).
//!
//! Making it genuinely semantic is a one-type change: implement [`Embedder`]
//! over a real sentence-embedding model (e.g. the `fastembed` crate running
//! BGE-small locally via ONNX, or a hosted embeddings API) and hand it to
//! [`SemanticSearch::with_embedder`]. Nothing else in this file changes. It's
//! kept out of the default build on purpose: `fastembed` pulls in the ONNX
//! runtime and a ~130 MB model download, which would bloat the "cheap enough
//! to give away" binary and complicate the 5-target release cross-compile —
//! so it belongs behind a cargo feature, opt-in.

use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::UNIX_EPOCH;

use async_trait::async_trait;
use serde::Deserialize;
use serde_json::value::RawValue;

use crate::error::ToolError;
use crate::fs::Workspace;
use crate::tool::{Tool, obj_schema};
use crate::walk::walk_files;

const EMBED_DIM: usize = 512;
/// Line window per indexed chunk, and the step between successive windows
/// (so `CHUNK_LINES - CHUNK_STEP` lines of overlap keep a match that straddles
/// a boundary from falling through the cracks).
const CHUNK_LINES: usize = 40;
const CHUNK_STEP: usize = 30;
/// Hard cap on indexed chunks so a huge repo can't blow up memory/time in this
/// prototype. Roughly `MAX_CHUNKS * EMBED_DIM * 4` bytes of vectors.
const MAX_CHUNKS: usize = 8_000;
const DEFAULT_TOP_K: usize = 8;
const MAX_TOP_K: usize = 25;
/// Cosine floor below which a "match" is just hash noise / incidental overlap.
const MIN_SCORE: f32 = 0.05;
const PREVIEW_CHARS: usize = 120;

// ---------------------------------------------------------------------------
// Embedder: the one swappable piece (see module docs).
// ---------------------------------------------------------------------------

/// Turns text into a fixed-length, L2-normalized vector. Cosine similarity is
/// then just a dot product. Swap the implementation to change *what* "similar"
/// means without touching the index or the tool.
pub trait Embedder: Send + Sync {
    fn dim(&self) -> usize;
    /// Must return an L2-normalized vector of length [`Embedder::dim`] (a
    /// zero vector is allowed for empty/degenerate input).
    fn embed(&self, text: &str) -> Vec<f32>;
}

/// Local, dependency-free feature-hashing embedder. Hashes each token and each
/// of its character trigrams into a signed bucket (the "hashing trick"), then
/// L2-normalizes. Deterministic across platforms (fixed FNV-1a), fast enough
/// to re-embed the whole workspace on every change, and good for lexical/fuzzy
/// recall — see the module docs for its limits and how to go neural.
pub struct HashingEmbedder {
    dim: usize,
}

impl HashingEmbedder {
    pub fn new(dim: usize) -> Self {
        Self { dim: dim.max(1) }
    }
}

impl Embedder for HashingEmbedder {
    fn dim(&self) -> usize {
        self.dim
    }
    fn embed(&self, text: &str) -> Vec<f32> {
        let mut v = vec![0f32; self.dim];
        for token in tokenize(text) {
            add_feature(&mut v, token.as_bytes(), 1.0);
            for tri in char_trigrams(&token) {
                add_feature(&mut v, tri.as_bytes(), 0.5);
            }
        }
        l2_normalize(&mut v);
        v
    }
}

const FNV_OFFSET: u64 = 0xcbf2_9ce4_8422_2325;
const FNV_PRIME: u64 = 0x0000_0100_0000_01b3;

fn fnv1a(bytes: &[u8]) -> u64 {
    let mut h = FNV_OFFSET;
    for &b in bytes {
        h ^= b as u64;
        h = h.wrapping_mul(FNV_PRIME);
    }
    h
}

fn add_feature(v: &mut [f32], feat: &[u8], weight: f32) {
    let h = fnv1a(feat);
    let idx = (h % v.len() as u64) as usize;
    // Signed hashing: a second bit picks the sign, so collisions cancel in
    // expectation instead of always reinforcing.
    let sign = if (h >> 63) & 1 == 0 { 1.0 } else { -1.0 };
    v[idx] += sign * weight;
}

fn l2_normalize(v: &mut [f32]) {
    let norm = v.iter().map(|x| x * x).sum::<f32>().sqrt();
    if norm > 0.0 {
        for x in v.iter_mut() {
            *x /= norm;
        }
    }
}

fn dot(a: &[f32], b: &[f32]) -> f32 {
    a.iter().zip(b).map(|(x, y)| x * y).sum()
}

/// Lowercase alphanumeric tokens, with identifiers split on camelCase and
/// letter↔digit boundaries so `readFile`, `read_file`, and `read file` all
/// tokenize alike. Single characters are dropped as noise.
fn tokenize(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    for raw in text.split(|c: char| !c.is_alphanumeric()) {
        if raw.is_empty() {
            continue;
        }
        for sub in split_identifier(raw) {
            let s = sub.to_lowercase();
            if s.chars().count() >= 2 {
                out.push(s);
            }
        }
    }
    out
}

fn split_identifier(s: &str) -> Vec<String> {
    let mut parts = Vec::new();
    let mut cur = String::new();
    let mut prev: Option<char> = None;
    for c in s.chars() {
        if let Some(p) = prev {
            let camel = !p.is_uppercase() && c.is_uppercase();
            let alnum_flip = p.is_alphabetic() != c.is_alphabetic();
            if (camel || alnum_flip) && !cur.is_empty() {
                parts.push(std::mem::take(&mut cur));
            }
        }
        cur.push(c);
        prev = Some(c);
    }
    if !cur.is_empty() {
        parts.push(cur);
    }
    parts
}

fn char_trigrams(token: &str) -> Vec<String> {
    let padded: Vec<char> = format!("^{token}$").chars().collect();
    if padded.len() < 3 {
        return vec![padded.into_iter().collect()];
    }
    padded.windows(3).map(|w| w.iter().collect()).collect()
}

// ---------------------------------------------------------------------------
// Index: chunk vectors + a fingerprint that invalidates on any file change.
// ---------------------------------------------------------------------------

struct Chunk {
    /// Workspace-relative path (for display and scope filtering).
    path: String,
    start_line: usize,
    end_line: usize,
    preview: String,
    vec: Vec<f32>,
}

struct Index {
    fingerprint: u64,
    chunks: Vec<Chunk>,
    truncated: bool,
}

struct FileStat {
    path: std::path::PathBuf,
    rel: String,
    len: u64,
    mtime_nanos: u128,
}

/// Stat every indexable file under `root`, workspace-relative to `strip_base`,
/// sorted by path for a stable fingerprint.
fn collect_stats(root: &Path, strip_base: &Path) -> Vec<FileStat> {
    let mut v: Vec<FileStat> = walk_files(root)
        .filter_map(|p| {
            let md = std::fs::metadata(&p).ok()?;
            let mtime_nanos = md
                .modified()
                .ok()
                .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
                .map(|d| d.as_nanos())
                .unwrap_or(0);
            let rel = p
                .strip_prefix(strip_base)
                .unwrap_or(&p)
                .display()
                .to_string();
            Some(FileStat {
                path: p,
                rel,
                len: md.len(),
                mtime_nanos,
            })
        })
        .collect();
    v.sort_by(|a, b| a.path.cmp(&b.path));
    v
}

/// A fingerprint over (path, size, mtime) of every file — changes the instant
/// the agent edits, adds, or deletes anything, so a stale index is never used.
fn fingerprint(stats: &[FileStat]) -> u64 {
    let mut h = FNV_OFFSET;
    for s in stats {
        h = mix(h, s.rel.as_bytes());
        h = mix(h, &s.len.to_le_bytes());
        h = mix(h, &s.mtime_nanos.to_le_bytes());
    }
    h
}

/// FNV-1a folded onto a running accumulator, so several fields chain into one
/// hash without resetting.
fn mix(mut h: u64, bytes: &[u8]) -> u64 {
    for &b in bytes {
        h ^= b as u64;
        h = h.wrapping_mul(FNV_PRIME);
    }
    h
}

fn build_index(stats: &[FileStat], embedder: &dyn Embedder, fingerprint: u64) -> Index {
    let mut chunks = Vec::new();
    let mut truncated = false;
    'files: for s in stats {
        let Ok(contents) = std::fs::read_to_string(&s.path) else {
            continue; // binary / unreadable
        };
        let lines: Vec<&str> = contents.lines().collect();
        let mut start = 0;
        while start < lines.len() {
            let end = (start + CHUNK_LINES).min(lines.len());
            let window = &lines[start..end];
            if let Some(preview) = first_non_blank(window) {
                chunks.push(Chunk {
                    path: s.rel.clone(),
                    start_line: start + 1,
                    end_line: end,
                    preview,
                    vec: embedder.embed(&window.join("\n")),
                });
                if chunks.len() >= MAX_CHUNKS {
                    truncated = true;
                    break 'files;
                }
            }
            if end == lines.len() {
                break;
            }
            start += CHUNK_STEP;
        }
    }
    Index {
        fingerprint,
        chunks,
        truncated,
    }
}

fn first_non_blank(lines: &[&str]) -> Option<String> {
    lines
        .iter()
        .map(|l| l.trim())
        .find(|l| !l.is_empty())
        .map(|l| {
            if l.chars().count() > PREVIEW_CHARS {
                let head: String = l.chars().take(PREVIEW_CHARS).collect();
                format!("{head}…")
            } else {
                l.to_string()
            }
        })
}

// ---------------------------------------------------------------------------
// The tool.
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
struct SemanticArgs {
    query: String,
    #[serde(default)]
    path: Option<String>,
    #[serde(default)]
    top_k: Option<usize>,
}

/// Ranked similarity search over the workspace. Holds a cross-call index cache
/// (rebuilt only when files change) behind a `Mutex`, so repeated searches in
/// a session don't re-embed an unchanged repo.
pub struct SemanticSearch {
    workspace: Workspace,
    embedder: Arc<dyn Embedder>,
    index: Arc<Mutex<Option<Index>>>,
}

impl SemanticSearch {
    pub fn new(workspace: Workspace) -> Self {
        Self::with_embedder(workspace, Arc::new(HashingEmbedder::new(EMBED_DIM)))
    }

    /// Build with a custom [`Embedder`] — the seam for dropping in a real
    /// neural embedding model without touching the index or tool logic.
    pub fn with_embedder(workspace: Workspace, embedder: Arc<dyn Embedder>) -> Self {
        Self {
            workspace,
            embedder,
            index: Arc::new(Mutex::new(None)),
        }
    }
}

#[async_trait]
impl Tool for SemanticSearch {
    fn name(&self) -> &str {
        "semantic_search"
    }
    fn description(&self) -> &str {
        "Find the code most relevant to a natural-language or code query, ranked by similarity \
         (not exact matching). Use it to locate where a concept lives when you don't know the exact \
         symbol; use `search` when you know the literal string. Returns `path:startLine-endLine` \
         ranges with a preview — read those ranges for detail. Read-only, no approval needed."
    }
    fn schema(&self) -> serde_json::Value {
        obj_schema(
            &[
                (
                    "query",
                    serde_json::json!({"type": "string", "description": "what to look for, in words or code"}),
                ),
                (
                    "path",
                    serde_json::json!({"type": "string", "description": "optional workspace-relative file/dir to restrict results to"}),
                ),
                (
                    "top_k",
                    serde_json::json!({"type": "integer", "description": "how many ranked results to return (default 8)"}),
                ),
            ],
            &["query"],
        )
    }
    async fn execute(&self, args: &RawValue) -> Result<String, ToolError> {
        let a: SemanticArgs = serde_json::from_str(args.get())?;
        if a.query.trim().is_empty() {
            return Err(ToolError::Message("query is required".into()));
        }
        let top_k = a.top_k.unwrap_or(DEFAULT_TOP_K).clamp(1, MAX_TOP_K);

        // Always index the whole workspace (so the cache is stable regardless
        // of per-query scoping); `path` becomes a relative-prefix filter on the
        // results. Both paths go through `Workspace::resolve` for confinement.
        let strip_base = self.workspace.resolve(".")?;
        let scope_rel = match &a.path {
            Some(p) if !p.is_empty() => {
                let resolved = self.workspace.resolve(p)?;
                Some(
                    resolved
                        .strip_prefix(&strip_base)
                        .unwrap_or(&resolved)
                        .display()
                        .to_string(),
                )
            }
            _ => None,
        };

        let embedder = self.embedder.clone();
        let cache = self.index.clone();
        let query = a.query.clone();

        // Index build (walk + read + embed) and search are blocking/CPU work;
        // run off the reactor. The std `Mutex` is only ever locked inside this
        // sync closure, never across an await.
        let output = tokio::task::spawn_blocking(move || {
            let stats = collect_stats(&strip_base, &strip_base);
            let fp = fingerprint(&stats);

            let mut guard = cache.lock().expect("semantic index mutex poisoned");
            if guard.as_ref().map(|i| i.fingerprint) != Some(fp) {
                *guard = Some(build_index(&stats, embedder.as_ref(), fp));
            }
            let index = guard.as_ref().expect("index just populated");

            let qv = embedder.embed(&query);
            let mut scored: Vec<(f32, &Chunk)> = index
                .chunks
                .iter()
                .filter(|c| match &scope_rel {
                    Some(scope) => in_scope(&c.path, scope),
                    None => true,
                })
                .map(|c| (dot(&qv, &c.vec), c))
                .filter(|(s, _)| *s > MIN_SCORE)
                .collect();
            scored.sort_by(|a, b| b.0.total_cmp(&a.0));
            scored.truncate(top_k);
            format_hits(&scored, index.truncated)
        })
        .await
        .map_err(|e| ToolError::Message(format!("semantic search task failed: {e}")))?;

        Ok(output)
    }
}

/// A chunk is in scope if its path equals the scope (a single file) or sits
/// under it as a directory prefix — segment-aware so `src` doesn't match
/// `src_other/`.
fn in_scope(chunk_path: &str, scope: &str) -> bool {
    chunk_path == scope || chunk_path.starts_with(&format!("{scope}/"))
}

fn format_hits(scored: &[(f32, &Chunk)], truncated: bool) -> String {
    if scored.is_empty() {
        return "no relevant code found (try `search` for an exact string, or rephrase)"
            .to_string();
    }
    let mut out = String::new();
    for (score, c) in scored {
        out.push_str(&format!(
            "{}:{}-{}  (score {:.2})\n    {}\n",
            c.path, c.start_line, c.end_line, score, c.preview
        ));
    }
    if truncated {
        out.push_str(&format!(
            "[index capped at {MAX_CHUNKS} chunks; results may be incomplete on a very large repo]\n"
        ));
    }
    out.trim_end().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ws(name: &str) -> Workspace {
        let dir = std::env::temp_dir().join(format!("hivemind_semantic_test_{name}"));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        Workspace::new(dir)
    }

    fn args(json: serde_json::Value) -> Box<RawValue> {
        RawValue::from_string(json.to_string()).unwrap()
    }

    #[test]
    fn embeddings_are_unit_length_and_deterministic() {
        let e = HashingEmbedder::new(EMBED_DIM);
        let a = e.embed("fn read_file(path: &str)");
        let b = e.embed("fn read_file(path: &str)");
        assert_eq!(a, b, "embedding must be deterministic");
        let norm = a.iter().map(|x| x * x).sum::<f32>().sqrt();
        assert!(
            (norm - 1.0).abs() < 1e-4,
            "expected unit length, got {norm}"
        );
    }

    #[test]
    fn identifier_splitting_unifies_naming_styles() {
        // camelCase, snake_case, and spaced words all reduce to the same tokens.
        assert!(tokenize("readFile").contains(&"read".to_string()));
        assert!(tokenize("readFile").contains(&"file".to_string()));
        assert!(tokenize("read_file").contains(&"file".to_string()));
    }

    #[test]
    fn ranks_the_relevant_chunk_above_an_unrelated_one() {
        let e = HashingEmbedder::new(EMBED_DIM);
        let q = e.embed("read the contents of a file from disk");
        let relevant =
            e.embed("fn read_file(path) { let bytes = fs::read(path); return contents; }");
        let unrelated = e.embed("fn backoff_delay(attempt) { exponential jitter for retry sleep }");
        assert!(
            dot(&q, &relevant) > dot(&q, &unrelated),
            "relevant {:.3} should outrank unrelated {:.3}",
            dot(&q, &relevant),
            dot(&q, &unrelated)
        );
    }

    #[tokio::test]
    async fn end_to_end_ranks_the_matching_file_first() {
        let w = ws("e2e");
        std::fs::write(
            w.root.join("reader.rs"),
            "fn read_file(path: &str) -> String {\n    let bytes = std::fs::read(path);\n    String::from_utf8(bytes)\n}\n",
        )
        .unwrap();
        std::fs::write(
            w.root.join("retry.rs"),
            "fn backoff_delay(attempt: u32) -> Duration {\n    // exponential backoff with jitter\n    base * 2u64.pow(attempt)\n}\n",
        )
        .unwrap();

        let out = SemanticSearch::new(w.clone())
            .execute(&args(
                serde_json::json!({"query": "read a file from disk", "top_k": 1}),
            ))
            .await
            .unwrap();
        assert!(
            out.contains("reader.rs"),
            "expected reader.rs first, got:\n{out}"
        );
        assert!(
            !out.contains("retry.rs"),
            "top-1 should exclude retry.rs, got:\n{out}"
        );
    }

    #[tokio::test]
    async fn scope_filter_restricts_results_to_a_subdirectory() {
        let w = ws("scope");
        std::fs::create_dir_all(w.root.join("src")).unwrap();
        std::fs::write(
            w.root.join("src/auth.rs"),
            "fn login_with_token(token) { verify(token) }",
        )
        .unwrap();
        std::fs::write(
            w.root.join("notes.rs"),
            "fn login_with_token(token) { verify(token) }",
        )
        .unwrap();

        let out = SemanticSearch::new(w.clone())
            .execute(&args(
                serde_json::json!({"query": "login with token", "path": "src"}),
            ))
            .await
            .unwrap();
        assert!(out.contains("src/auth.rs"), "got:\n{out}");
        assert!(!out.contains("notes.rs"), "scope leaked, got:\n{out}");
    }

    #[tokio::test]
    async fn reports_cleanly_when_nothing_matches() {
        let w = ws("empty");
        std::fs::write(w.root.join("a.rs"), "the quick brown fox").unwrap();
        let out = SemanticSearch::new(w.clone())
            .execute(&args(
                serde_json::json!({"query": "zzqqxx_nonexistent_symbol_9000"}),
            ))
            .await
            .unwrap();
        assert!(out.contains("no relevant code"), "got:\n{out}");
    }
}
