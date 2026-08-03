//! On-disk cache of embedding vectors, keyed by chunk *content* rather than
//! by file or repo state.
//!
//! Content-keying is the whole point: an unchanged chunk keeps the same key
//! no matter which file it moved to, which session asked for it, or what
//! else in the repo changed around it. That turns "re-embed everything on
//! any edit, every session" into "embed each distinct chunk once, ever".
//!
//! Every operation here is best-effort. A missing, truncated, corrupt, or
//! unwritable cache degrades to "embed it again" -- never to an error the
//! user sees, because a cache miss is only ever slower, not wrong.

use std::collections::HashMap;
use std::io::Write;
use std::path::{Path, PathBuf};

const MAGIC: &[u8; 4] = b"HMEC";
/// Bump on any format change; a mismatch discards the file rather than
/// trying to interpret old bytes with new rules.
const FORMAT_VERSION: u32 = 1;

/// Ceiling on retained entries, so a long-lived cache can't grow without
/// bound. At 768 dims an entry is ~3 KB, so this caps a single cache file
/// at roughly 150 MB.
const MAX_ENTRIES: usize = 50_000;

/// 128-bit content key (two independently-seeded 64-bit hashes). A single
/// 64-bit hash would be fine for collision *frequency*, but a collision
/// here returns a confidently wrong vector for unrelated code, which is
/// exactly the kind of silent corruption worth two extra words to avoid.
pub(crate) type Key = (u64, u64);

pub struct EmbedCache {
    path: PathBuf,
    dim: usize,
    entries: HashMap<Key, Vec<f32>>,
    /// Keys read or written this session. On eviction these are kept in
    /// preference to entries only present from previous runs -- a cheap
    /// stand-in for LRU that needs no access-time bookkeeping.
    touched: std::collections::HashSet<Key>,
    dirty: bool,
}

impl EmbedCache {
    /// Load the cache for `embedder_id`, or an empty one on any problem.
    pub fn load(dir: &Path, embedder_id: &str, dim: usize) -> Self {
        let path = dir.join(format!("{}.bin", sanitize(embedder_id)));
        let entries = read_file(&path, dim).unwrap_or_default();
        Self {
            path,
            dim,
            entries,
            touched: std::collections::HashSet::new(),
            dirty: false,
        }
    }

    pub fn get(&mut self, text: &str) -> Option<Vec<f32>> {
        let key = key_of(text);
        let hit = self.entries.get(&key).cloned();
        if hit.is_some() {
            self.touched.insert(key);
        }
        hit
    }

    pub fn insert(&mut self, text: &str, vec: Vec<f32>) {
        // A wrong-width vector would poison every later comparison, and the
        // cache is not the right place to discover that.
        if vec.len() != self.dim {
            return;
        }
        let key = key_of(text);
        self.touched.insert(key);
        self.entries.insert(key, vec);
        self.dirty = true;
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Write back if anything changed. Best-effort and atomic: a temp file
    /// plus a rename, so a crash or a second process mid-write can never
    /// leave a half-written cache behind (`fs::rename` replaces the
    /// destination on both Unix and Windows).
    pub fn save(&mut self) {
        if !self.dirty {
            return;
        }
        self.dirty = false;

        if self.entries.len() > MAX_ENTRIES {
            self.evict_down_to(MAX_ENTRIES);
        }
        if std::fs::create_dir_all(self.path.parent().unwrap_or(Path::new("."))).is_err() {
            return;
        }

        // Unique temp name: two concurrent sessions must not write the same
        // temp path, or one truncates the other's file mid-write.
        let tmp = self
            .path
            .with_extension(format!("tmp{}", std::process::id()));
        if write_file(&tmp, self.dim, &self.entries).is_err() {
            let _ = std::fs::remove_file(&tmp);
            return;
        }
        if std::fs::rename(&tmp, &self.path).is_err() {
            let _ = std::fs::remove_file(&tmp);
        }
    }

    fn evict_down_to(&mut self, target: usize) {
        // Entries used this session survive first; only if those alone
        // still exceed the cap does it drop into them arbitrarily.
        let mut keep: Vec<Key> = self.touched.iter().copied().take(target).collect();
        if keep.len() < target {
            for k in self.entries.keys() {
                if keep.len() >= target {
                    break;
                }
                if !self.touched.contains(k) {
                    keep.push(*k);
                }
            }
        }
        let keep: std::collections::HashSet<Key> = keep.into_iter().collect();
        self.entries.retain(|k, _| keep.contains(k));
    }
}

/// Replace anything that isn't portable in a filename. Model ids can carry
/// `/` (`org/model`), which would otherwise be read as a directory.
fn sanitize(s: &str) -> String {
    s.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect()
}

const FNV_OFFSET_A: u64 = 0xcbf2_9ce4_8422_2325;
const FNV_OFFSET_B: u64 = 0x9e37_79b9_7f4a_7c15;
const FNV_PRIME: u64 = 0x0000_0100_0000_01b3;

pub(crate) fn key_of(text: &str) -> Key {
    let bytes = text.as_bytes();
    let mut a = FNV_OFFSET_A;
    for &b in bytes {
        a ^= b as u64;
        a = a.wrapping_mul(FNV_PRIME);
    }
    // Second pass over the reversed bytes from a different seed, so the two
    // halves don't collide together on the same input.
    let mut b2 = FNV_OFFSET_B;
    for &b in bytes.iter().rev() {
        b2 ^= b as u64;
        b2 = b2.wrapping_mul(FNV_PRIME);
    }
    // Length is folded in so two texts of different length can't share a key.
    b2 ^= bytes.len() as u64;
    (a, b2)
}

fn read_file(path: &Path, dim: usize) -> Option<HashMap<Key, Vec<f32>>> {
    let data = std::fs::read(path).ok()?;
    if data.len() < 16 || &data[0..4] != MAGIC {
        return None;
    }
    let version = u32::from_le_bytes(data[4..8].try_into().ok()?);
    let file_dim = u32::from_le_bytes(data[8..12].try_into().ok()?) as usize;
    let count = u32::from_le_bytes(data[12..16].try_into().ok()?) as usize;
    // A different format or vector width isn't an error -- the model
    // changed, so the old vectors are simply not comparable any more.
    if version != FORMAT_VERSION || file_dim != dim {
        return None;
    }

    let entry_len = 16 + dim * 4;
    let mut out = HashMap::with_capacity(count);
    let mut off = 16;
    for _ in 0..count {
        // Truncated tail (interrupted write on an older build, disk full):
        // keep whatever parsed cleanly rather than discarding the lot.
        if off + entry_len > data.len() {
            break;
        }
        let a = u64::from_le_bytes(data[off..off + 8].try_into().ok()?);
        let b = u64::from_le_bytes(data[off + 8..off + 16].try_into().ok()?);
        let mut vec = Vec::with_capacity(dim);
        let mut p = off + 16;
        for _ in 0..dim {
            vec.push(f32::from_le_bytes(data[p..p + 4].try_into().ok()?));
            p += 4;
        }
        out.insert((a, b), vec);
        off += entry_len;
    }
    Some(out)
}

fn write_file(path: &Path, dim: usize, entries: &HashMap<Key, Vec<f32>>) -> std::io::Result<()> {
    let file = std::fs::File::create(path)?;
    let mut w = std::io::BufWriter::new(file);
    w.write_all(MAGIC)?;
    w.write_all(&FORMAT_VERSION.to_le_bytes())?;
    w.write_all(&(dim as u32).to_le_bytes())?;
    w.write_all(&(entries.len() as u32).to_le_bytes())?;
    for ((a, b), vec) in entries {
        w.write_all(&a.to_le_bytes())?;
        w.write_all(&b.to_le_bytes())?;
        for v in vec {
            w.write_all(&v.to_le_bytes())?;
        }
    }
    w.flush()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp_dir(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!(
            "hivemind_embed_cache_{name}_{}_{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn round_trips_through_disk() {
        let dir = tmp_dir("roundtrip");
        let mut c = EmbedCache::load(&dir, "test-model", 4);
        assert_eq!(c.len(), 0);
        c.insert("fn main() {}", vec![0.1, 0.2, 0.3, 0.4]);
        c.save();

        let mut reopened = EmbedCache::load(&dir, "test-model", 4);
        assert_eq!(reopened.len(), 1);
        assert_eq!(reopened.get("fn main() {}"), Some(vec![0.1, 0.2, 0.3, 0.4]));
        assert_eq!(reopened.get("something else"), None);
    }

    #[test]
    fn a_different_model_id_uses_a_separate_file() {
        let dir = tmp_dir("per_model");
        let mut a = EmbedCache::load(&dir, "model-a", 4);
        a.insert("x", vec![1.0, 0.0, 0.0, 0.0]);
        a.save();

        // Vectors from a different model are not comparable, so they must
        // not be visible here.
        let mut b = EmbedCache::load(&dir, "model-b", 4);
        assert_eq!(b.get("x"), None);
    }

    #[test]
    fn a_dimension_change_discards_the_old_cache() {
        let dir = tmp_dir("dim_change");
        let mut a = EmbedCache::load(&dir, "m", 4);
        a.insert("x", vec![1.0, 0.0, 0.0, 0.0]);
        a.save();

        let mut wider = EmbedCache::load(&dir, "m", 8);
        assert_eq!(wider.len(), 0, "4-dim vectors must not load as 8-dim");
        assert_eq!(wider.get("x"), None);
    }

    #[test]
    fn a_corrupt_file_is_ignored_rather_than_failing() {
        let dir = tmp_dir("corrupt");
        std::fs::write(dir.join("m.bin"), b"not a cache file at all").unwrap();
        let mut c = EmbedCache::load(&dir, "m", 4);
        assert_eq!(c.len(), 0);
        // Still usable afterwards.
        c.insert("x", vec![1.0, 0.0, 0.0, 0.0]);
        c.save();
        assert_eq!(EmbedCache::load(&dir, "m", 4).len(), 1);
    }

    #[test]
    fn a_truncated_tail_keeps_the_entries_that_parsed() {
        let dir = tmp_dir("truncated");
        let mut c = EmbedCache::load(&dir, "m", 4);
        c.insert("one", vec![1.0, 0.0, 0.0, 0.0]);
        c.insert("two", vec![0.0, 1.0, 0.0, 0.0]);
        c.save();

        // Lop off part of the final entry, as an interrupted write would.
        let p = dir.join("m.bin");
        let mut bytes = std::fs::read(&p).unwrap();
        bytes.truncate(bytes.len() - 10);
        std::fs::write(&p, bytes).unwrap();

        assert_eq!(EmbedCache::load(&dir, "m", 4).len(), 1);
    }

    #[test]
    fn a_wrong_width_vector_is_refused() {
        let dir = tmp_dir("wrong_width");
        let mut c = EmbedCache::load(&dir, "m", 4);
        c.insert("x", vec![1.0, 0.0]); // too short
        assert_eq!(c.get("x"), None);
        assert_eq!(c.len(), 0);
    }

    #[test]
    fn keys_separate_texts_that_differ_only_by_order_or_length() {
        assert_ne!(key_of("ab"), key_of("ba"));
        assert_ne!(key_of("a"), key_of("aa"));
        assert_ne!(key_of(""), key_of("a"));
        assert_eq!(key_of("same"), key_of("same"));
    }

    #[test]
    fn a_model_id_with_a_slash_stays_one_file() {
        assert_eq!(
            sanitize("jinaai/jina-embeddings-v2"),
            "jinaai_jina-embeddings-v2"
        );
        assert!(!sanitize("a/b\\c:d").contains(['/', '\\', ':']));
    }
}
