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

    /// Stable identity for the vector space this embedder produces. Vectors
    /// from two different ids are not comparable, so this is written into
    /// the index and checked before a cached index is reused — without it,
    /// switching embedders would score a new query against stale vectors of
    /// a different width and silently return nonsense.
    fn id(&self) -> &str;

    /// Embed many texts at once. The default loops over [`Embedder::embed`],
    /// so a purely local implementation needs nothing extra; a network-backed
    /// one overrides this to spend one round trip per batch instead of one
    /// per chunk. Returning `Err` means "this embedder is unavailable" — the
    /// caller falls back to a local one and rebuilds the whole index rather
    /// than mixing two vector spaces.
    fn embed_batch(&self, texts: &[String]) -> Result<Vec<Vec<f32>>, String> {
        Ok(texts.iter().map(|t| self.embed(t)).collect())
    }
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
    fn id(&self) -> &str {
        "hash-v1"
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
    /// Which embedder produced `chunks[*].vec`. A cached index is only
    /// reusable by the embedder that built it.
    embedder_id: String,
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

/// Number of chunk texts handed to [`Embedder::embed_batch`] at a time.
/// Matters only for network-backed embedders, where it is the difference
/// between one request per chunk and one per 128 -- and therefore between
/// paying a connection round trip 400 times or 4. Kept at or below the
/// server's own per-request input cap.
const EMBED_BATCH: usize = 128;

/// Collect every chunk's text first, embed in batches, then zip the vectors
/// back on. The two-pass shape exists for `embed_batch`: a per-chunk
/// `embed()` call is fine locally but pathological over a network.
fn build_index(
    stats: &[FileStat],
    embedder: &dyn Embedder,
    fingerprint: u64,
) -> Result<Index, String> {
    struct Pending {
        path: String,
        start_line: usize,
        end_line: usize,
        preview: String,
        text: String,
    }

    let mut pending: Vec<Pending> = Vec::new();
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
                pending.push(Pending {
                    path: s.rel.clone(),
                    start_line: start + 1,
                    end_line: end,
                    preview,
                    text: window.join("\n"),
                });
                if pending.len() >= MAX_CHUNKS {
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

    let mut vectors: Vec<Vec<f32>> = Vec::with_capacity(pending.len());
    for group in pending.chunks(EMBED_BATCH) {
        let texts: Vec<String> = group.iter().map(|p| p.text.clone()).collect();
        let got = embedder.embed_batch(&texts)?;
        // A short or over-long reply would silently misalign every vector
        // after it with the wrong chunk, which is far worse than failing.
        if got.len() != texts.len() {
            return Err(format!(
                "embedder returned {} vectors for {} inputs",
                got.len(),
                texts.len()
            ));
        }
        vectors.extend(got);
    }

    let chunks = pending
        .into_iter()
        .zip(vectors)
        .map(|(p, vec)| Chunk {
            path: p.path,
            start_line: p.start_line,
            end_line: p.end_line,
            preview: p.preview,
            vec,
        })
        .collect();

    Ok(Index {
        fingerprint,
        embedder_id: embedder.id().to_string(),
        chunks,
        truncated,
    })
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
    /// Always local and always infallible. Used when `embedder` is a remote
    /// one that fails, so a network problem degrades search quality instead
    /// of failing the user's task outright.
    fallback: Arc<dyn Embedder>,
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
            fallback: Arc::new(HashingEmbedder::new(EMBED_DIM)),
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
        let fallback = self.fallback.clone();
        let cache = self.index.clone();
        let query = a.query.clone();

        // Index build (walk + read + embed) and search are blocking/CPU work;
        // run off the reactor. The std `Mutex` is only ever locked inside this
        // sync closure, never across an await.
        let output = tokio::task::spawn_blocking(move || {
            let stats = collect_stats(&strip_base, &strip_base);
            let fp = fingerprint(&stats);

            let mut guard = cache.lock().expect("semantic index mutex poisoned");
            // Reuse the cached index only if BOTH the files and the vector
            // space are unchanged. Ignoring the embedder id here would score
            // a fresh query against vectors of a different width/meaning.
            let stale = match guard.as_ref() {
                Some(i) => i.fingerprint != fp || i.embedder_id != embedder.id(),
                None => true,
            };
            if stale {
                let built = match build_index(&stats, embedder.as_ref(), fp) {
                    Ok(idx) => idx,
                    Err(e) => {
                        // Rebuild wholesale with the local embedder rather
                        // than salvaging a partial remote index -- half the
                        // chunks in one vector space and half in another is
                        // not a degraded index, it is a wrong one.
                        eprintln!(
                            "semantic_search: {} embedder unavailable ({e}); falling back to local index",
                            embedder.id()
                        );
                        build_index(&stats, fallback.as_ref(), fp)
                            .expect("local embedder is infallible")
                    }
                };
                *guard = Some(built);
            }
            let index = guard.as_ref().expect("index just populated");

            // The query must be embedded by whichever embedder actually
            // produced this index, not whichever one was requested.
            let active: &dyn Embedder = if index.embedder_id == embedder.id() {
                embedder.as_ref()
            } else {
                fallback.as_ref()
            };
            let qv = match active.embed_batch(std::slice::from_ref(&query)) {
                Ok(mut v) if v.len() == 1 => v.remove(0),
                // A query-time failure against an index that is already
                // built is not worth discarding the index over; the local
                // embedder cannot match those vectors, so report cleanly.
                _ => return "semantic_search: embedding the query failed; try again".to_string(),
            };
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

#[cfg(test)]
mod embedder_switch_tests {
    use super::*;

    /// A second embedder with a different id and width, standing in for the
    /// hosted one.
    struct WideEmbedder;
    impl Embedder for WideEmbedder {
        fn dim(&self) -> usize {
            8
        }
        fn id(&self) -> &str {
            "wide-test"
        }
        fn embed(&self, _text: &str) -> Vec<f32> {
            let mut v = vec![0.0; 8];
            v[0] = 1.0;
            v
        }
    }

    struct FailingEmbedder;
    impl Embedder for FailingEmbedder {
        fn dim(&self) -> usize {
            8
        }
        fn id(&self) -> &str {
            "failing-test"
        }
        fn embed(&self, _text: &str) -> Vec<f32> {
            vec![0.0; 8]
        }
        fn embed_batch(&self, _texts: &[String]) -> Result<Vec<Vec<f32>>, String> {
            Err("upstream down".to_string())
        }
    }

    fn stats_for(dir: &std::path::Path) -> Vec<FileStat> {
        collect_stats(dir, dir)
    }

    fn tmp_repo(name: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("hivemind_semantic_switch_{name}"));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("a.rs"), "fn alpha() {}\nfn beta() {}\n").unwrap();
        dir
    }

    #[test]
    fn index_records_which_embedder_built_it() {
        let dir = tmp_repo("records");
        let stats = stats_for(&dir);
        let local = HashingEmbedder::new(EMBED_DIM);
        let idx = build_index(&stats, &local, 1).unwrap();
        assert_eq!(idx.embedder_id, "hash-v1");
        assert_eq!(idx.chunks[0].vec.len(), EMBED_DIM);

        let wide = WideEmbedder;
        let idx2 = build_index(&stats, &wide, 1).unwrap();
        assert_eq!(idx2.embedder_id, "wide-test");
        // Different width: reusing idx across these two would be a bug.
        assert_eq!(idx2.chunks[0].vec.len(), 8);
    }

    #[test]
    fn a_failing_embedder_reports_error_rather_than_a_partial_index() {
        let dir = tmp_repo("failing");
        let stats = stats_for(&dir);
        match build_index(&stats, &FailingEmbedder, 1) {
            Err(e) => assert!(e.contains("upstream down"), "got: {e}"),
            Ok(_) => panic!("a failing embedder must not yield an index"),
        }
    }

    #[test]
    fn batching_preserves_chunk_to_vector_alignment() {
        // Enough chunks to span multiple EMBED_BATCH groups, so a
        // mis-zipped batch boundary would show up as a wrong preview.
        let dir = std::env::temp_dir().join("hivemind_semantic_switch_align");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let body: String = (0..(EMBED_BATCH * 3 * CHUNK_STEP))
            .map(|i| format!("fn f{i}() {{}}\n"))
            .collect();
        std::fs::write(dir.join("big.rs"), body).unwrap();

        let stats = stats_for(&dir);
        let local = HashingEmbedder::new(EMBED_DIM);
        let idx = build_index(&stats, &local, 7).unwrap();
        assert!(idx.chunks.len() > EMBED_BATCH * 2, "need multiple batches");
        for c in &idx.chunks {
            // Every chunk must carry a real vector from its own text.
            assert_eq!(c.vec.len(), EMBED_DIM);
            assert!(
                c.vec.iter().any(|v| *v != 0.0),
                "zero vector at {}",
                c.start_line
            );
        }
    }
}
