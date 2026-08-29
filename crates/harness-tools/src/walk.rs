//! Shared workspace-walk policy: which files the read-only code tools look at,
//! and which directories they skip. Centralized so `search` and
//! `semantic_search` can't drift apart on what "the codebase" means. (The REPL
//! file-completer in `harness-cli` keeps its own copy — a different crate —
//! but deliberately uses the same names.)

use std::path::{Path, PathBuf};

use walkdir::{DirEntry, WalkDir};

/// Directories never worth walking — VCS internals, build output, vendored
/// deps, caches. Pruning them keeps a walk fast and its results signal-dense,
/// and avoids descending a multi-GB `target/` in a Rust workspace.
///
/// # Why this is a curated list and not `.gitignore`
///
/// Reading `.gitignore` is the obvious idea, and it was implemented, measured
/// and reverted. Two findings killed it:
///
/// 1. **`.gitignore` means "don't version this", not "this isn't source."**
///    This repository's own ignore file lists `/scripts` (the release
///    script), `/Mydoc/` (design and research notes) and `HiveMind.md` — all
///    hand-written, all exactly what someone would ask an agent to work on.
///    Honouring it would have made those files invisible to every read-only
///    tool, with no error to explain why. The failure this whole change set
///    exists to fix was the harness withholding context from the model; a
///    gitignore-driven walk does the same thing, more quietly.
///
/// 2. It cost **+816 KB (+12.6%)** of release binary, across five
///    cross-compiled targets, for the `ignore` crate and its globset/regex
///    dependencies.
///
/// The asymmetry decides it. A name missing from this list wastes some
/// context and is fixed by adding one word. A name wrongly present makes real
/// code unreachable. So entries here must be names that are *never*
/// hand-written source — which is why `out/` is absent despite being build
/// output in most JS projects: it is a real source directory often enough to
/// be dangerous.
pub const IGNORED_DIRS: &[&str] = &[
    ".git",
    "target",
    "node_modules",
    ".DS_Store",
    "dist",
    "build",
    ".venv",
    ".next",
    "__pycache__",
    ".pytest_cache",
    ".mypy_cache",
    ".turbo",
    ".gradle",
];

/// Skip files larger than this before reading them — almost always generated,
/// minified, or binary, and not what a code tool is for.
pub const MAX_FILE_BYTES: u64 = 2_000_000;

fn is_ignored(entry: &DirEntry) -> bool {
    entry
        .file_name()
        .to_str()
        .map(|n| IGNORED_DIRS.contains(&n))
        .unwrap_or(false)
}

/// Walk `root`, yielding regular files that pass the ignore/size policy above.
///
/// Hidden files are deliberately *not* skipped: `.github/`, `.cargo/` and
/// friends are real project content.
///
/// Walk errors (an unreadable subtree, a vanished file) are skipped rather
/// than fatal — a best-effort view of the workspace is what these tools want.
pub fn walk_files(root: &Path) -> impl Iterator<Item = PathBuf> {
    WalkDir::new(root)
        .into_iter()
        .filter_entry(|e| !is_ignored(e))
        .filter_map(Result::ok)
        .filter(|e| e.file_type().is_file())
        .filter(|e| {
            e.metadata()
                .map(|m| m.len() <= MAX_FILE_BYTES)
                .unwrap_or(false)
        })
        .map(walkdir::DirEntry::into_path)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp(tag: &str) -> PathBuf {
        let p = std::env::temp_dir().join(format!(
            "hm-walk-{tag}-{}-{:?}",
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_dir_all(&p);
        std::fs::create_dir_all(&p).unwrap();
        p
    }

    fn names(root: &Path) -> Vec<String> {
        let mut v: Vec<String> = walk_files(root)
            .filter_map(|p| {
                p.strip_prefix(root)
                    .ok()
                    .map(|r| r.to_string_lossy().into_owned())
            })
            .collect();
        v.sort();
        v
    }

    /// The reported bug: `IGNORED_DIRS` named `build` but not `.next`, so a
    /// Next.js workspace had its entire turbopack output walked -- thousands
    /// of generated chunks and source maps, which blew `project_map`'s entry
    /// cap before it reached any application source.
    #[test]
    fn generated_build_output_is_not_walked() {
        let root = tmp("generated");
        for d in [".next/static", "__pycache__", ".turbo", "src"] {
            std::fs::create_dir_all(root.join(d)).unwrap();
        }
        std::fs::write(root.join("src/page.tsx"), "export {}").unwrap();
        std::fs::write(root.join(".next/static/chunk.js"), "generated").unwrap();
        std::fs::write(root.join("__pycache__/x.pyc"), "generated").unwrap();
        std::fs::write(root.join(".turbo/log"), "generated").unwrap();

        assert_eq!(names(&root), vec!["src/page.tsx".to_string()]);
    }

    /// The property that decided against reading `.gitignore` at all. This
    /// repository gitignores `/scripts`, `/Mydoc/` and `HiveMind.md` -- all
    /// hand-written, all things someone would ask an agent to work on. A
    /// walk that honoured ignore files would make them unreachable with no
    /// error to explain it, which is the same withholding-context failure
    /// this module was fixed for, just quieter.
    #[test]
    fn a_gitignored_but_hand_written_file_is_still_walked() {
        let root = tmp("gitignored_source");
        std::fs::create_dir_all(root.join("scripts")).unwrap();
        std::fs::create_dir_all(root.join("Mydoc")).unwrap();
        std::fs::write(root.join(".gitignore"), "/scripts\n/Mydoc/\nNOTES.md\n").unwrap();
        std::fs::write(root.join("scripts/release.sh"), "#!/bin/sh").unwrap();
        std::fs::write(root.join("Mydoc/research.md"), "# notes").unwrap();
        std::fs::write(root.join("NOTES.md"), "# notes").unwrap();

        let found = names(&root);
        for expected in ["scripts/release.sh", "Mydoc/research.md", "NOTES.md"] {
            assert!(
                found.contains(&expected.to_string()),
                "{expected} must stay visible -- gitignore is not a source/not-source signal; got {found:?}"
            );
        }
    }

    /// `out/` is build output in most JS projects and a real source directory
    /// in enough others that excluding it would hide hand-written code.
    #[test]
    fn an_ambiguous_directory_name_is_not_excluded() {
        let root = tmp("ambiguous");
        std::fs::create_dir_all(root.join("out")).unwrap();
        std::fs::write(root.join("out/handwritten.ts"), "export {}").unwrap();

        assert!(names(&root).contains(&"out/handwritten.ts".to_string()));
    }

    /// Dotfiles are project content -- `.github/workflows` must stay
    /// searchable.
    #[test]
    fn hidden_directories_that_are_real_content_are_still_walked() {
        let root = tmp("hidden");
        std::fs::create_dir_all(root.join(".github/workflows")).unwrap();
        std::fs::write(root.join(".github/workflows/ci.yml"), "on: push").unwrap();

        assert!(names(&root).contains(&".github/workflows/ci.yml".to_string()));
    }

    /// `.git` is never listed in a `.gitignore` anyway, so the list has to
    /// carry it regardless of how ignore files are treated.
    #[test]
    fn the_git_directory_is_skipped_even_though_no_ignore_file_names_it() {
        let root = tmp("gitdir");
        std::fs::create_dir_all(root.join(".git/objects")).unwrap();
        std::fs::write(root.join(".git/objects/abc"), "blob").unwrap();
        std::fs::write(root.join("main.rs"), "fn main() {}").unwrap();

        let found = names(&root);
        assert_eq!(found, vec!["main.rs".to_string()], "{found:?}");
    }

    /// A multi-GB `target/` must never be walked.
    #[test]
    fn heavy_build_directories_are_always_excluded() {
        let root = tmp("backstop");
        std::fs::create_dir_all(root.join("target/debug")).unwrap();
        std::fs::create_dir_all(root.join("node_modules/left-pad")).unwrap();
        std::fs::write(root.join("target/debug/huge"), "artifact").unwrap();
        std::fs::write(root.join("node_modules/left-pad/i.js"), "dep").unwrap();
        std::fs::write(root.join("main.rs"), "fn main() {}").unwrap();

        assert_eq!(names(&root), vec!["main.rs".to_string()]);
    }
}
