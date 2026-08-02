//! Line-level diff rendering for `/diff`.
//!
//! Hand-rolled rather than pulling in `similar`/`diffy`, for the same
//! reason `crate::secrets` avoids `regex`: this binary cross-compiles to
//! five targets and the project has deliberately kept its dependency
//! surface small. What's needed here is one bounded diff of one file at a
//! time for human eyeballs, not a general-purpose merge engine.
//!
//! # The algorithm, and its honest limits
//!
//! Common prefix and suffix are trimmed first, which for a typical edit
//! (a few lines changed in a large file) collapses the problem to a handful
//! of lines. Whatever is left goes through a straightforward LCS dynamic
//! program — O(n·m) in both time and memory, which is fine for a few dozen
//! lines and catastrophic for a few thousand.
//!
//! So there is a hard cap: past [`MAX_LCS_LINES`] on either side of the
//! trimmed middle, the LCS is skipped and the change is reported as a plain
//! replacement with counts. That degrades the *presentation* of an enormous
//! rewrite, which is the case where a line-by-line diff was unreadable
//! anyway — it never degrades correctness, and it never hangs the REPL.

/// Past this many lines in the trimmed middle, fall back to a summary
/// instead of running an O(n·m) table. 800×800 is ~640k cells — a few
/// milliseconds and a few megabytes, comfortably the ceiling worth paying
/// inside an interactive prompt.
const MAX_LCS_LINES: usize = 800;

/// Context lines shown either side of a change.
const CONTEXT: usize = 3;

#[derive(Debug, PartialEq, Eq, Clone, Copy)]
pub enum Op {
    Same,
    Add,
    Remove,
}

#[derive(Debug, PartialEq, Eq)]
pub struct Line<'a> {
    pub op: Op,
    pub text: &'a str,
}

/// Added and removed line counts — the `+3 -1` summary.
#[derive(Debug, PartialEq, Eq, Default, Clone, Copy)]
pub struct Stats {
    pub added: usize,
    pub removed: usize,
}

impl Stats {
    pub fn is_empty(&self) -> bool {
        self.added == 0 && self.removed == 0
    }
}

/// Full line-by-line diff of `before` → `after`.
pub fn diff<'a>(before: &'a str, after: &'a str) -> Vec<Line<'a>> {
    let a: Vec<&str> = before.lines().collect();
    let b: Vec<&str> = after.lines().collect();

    let prefix = a.iter().zip(b.iter()).take_while(|(x, y)| x == y).count();
    let max_suffix = a.len().min(b.len()) - prefix;
    let suffix = a
        .iter()
        .rev()
        .zip(b.iter().rev())
        .take(max_suffix)
        .take_while(|(x, y)| x == y)
        .count();

    let mid_a = &a[prefix..a.len() - suffix];
    let mid_b = &b[prefix..b.len() - suffix];

    let mut out: Vec<Line<'a>> = a[..prefix]
        .iter()
        .map(|t| Line {
            op: Op::Same,
            text: t,
        })
        .collect();

    if mid_a.len() > MAX_LCS_LINES || mid_b.len() > MAX_LCS_LINES {
        // Too big to align line-for-line; report it as a wholesale
        // replacement rather than spending seconds on a table nobody will
        // read.
        out.extend(mid_a.iter().map(|t| Line {
            op: Op::Remove,
            text: t,
        }));
        out.extend(mid_b.iter().map(|t| Line {
            op: Op::Add,
            text: t,
        }));
    } else {
        out.extend(lcs_diff(mid_a, mid_b));
    }

    out.extend(a[a.len() - suffix..].iter().map(|t| Line {
        op: Op::Same,
        text: t,
    }));
    out
}

/// Classic LCS table, then walk it back to a diff. Only ever called on the
/// trimmed middle, and only when that middle is under [`MAX_LCS_LINES`].
fn lcs_diff<'a>(a: &[&'a str], b: &[&'a str]) -> Vec<Line<'a>> {
    let (n, m) = (a.len(), b.len());
    // (n+1) × (m+1) table of common-subsequence lengths.
    let mut table = vec![0u32; (n + 1) * (m + 1)];
    let at = |i: usize, j: usize| i * (m + 1) + j;
    for i in (0..n).rev() {
        for j in (0..m).rev() {
            table[at(i, j)] = if a[i] == b[j] {
                table[at(i + 1, j + 1)] + 1
            } else {
                table[at(i + 1, j)].max(table[at(i, j + 1)])
            };
        }
    }

    let mut out = Vec::new();
    let (mut i, mut j) = (0, 0);
    while i < n && j < m {
        if a[i] == b[j] {
            out.push(Line {
                op: Op::Same,
                text: a[i],
            });
            i += 1;
            j += 1;
        } else if table[at(i + 1, j)] >= table[at(i, j + 1)] {
            out.push(Line {
                op: Op::Remove,
                text: a[i],
            });
            i += 1;
        } else {
            out.push(Line {
                op: Op::Add,
                text: b[j],
            });
            j += 1;
        }
    }
    out.extend(a[i..].iter().map(|t| Line {
        op: Op::Remove,
        text: t,
    }));
    out.extend(b[j..].iter().map(|t| Line {
        op: Op::Add,
        text: t,
    }));
    out
}

pub fn stats(lines: &[Line<'_>]) -> Stats {
    let mut s = Stats::default();
    for l in lines {
        match l.op {
            Op::Add => s.added += 1,
            Op::Remove => s.removed += 1,
            Op::Same => {}
        }
    }
    s
}

/// Render as coloured unified-ish output, collapsing runs of unchanged
/// lines longer than `2 * CONTEXT` into a `⋯` marker.
///
/// `max_lines` bounds the output: a 4,000-line diff scrolled past in a
/// terminal is not information, and the whole point of `/diff` is a glance.
pub fn render(lines: &[Line<'_>], max_lines: usize) -> String {
    let keep = lines_worth_showing(lines);
    // Nothing kept means nothing changed. Falling through would print a
    // lone `⋯` for the collapsed run, i.e. a diff that looks like it has
    // hidden content when it has none.
    if !keep.iter().any(|k| *k) {
        return String::new();
    }
    let mut out = String::new();
    let mut shown = 0usize;
    let mut skipping = false;

    for (i, line) in lines.iter().enumerate() {
        if !keep[i] {
            if !skipping {
                out.push_str("\x1b[90m   ⋯\x1b[0m\n");
                skipping = true;
            }
            continue;
        }
        skipping = false;
        if shown >= max_lines {
            out.push_str(&format!(
                "\x1b[90m   … {} more lines not shown\x1b[0m\n",
                keep[i..].iter().filter(|k| **k).count()
            ));
            break;
        }
        match line.op {
            Op::Add => out.push_str(&format!("\x1b[32m  + {}\x1b[0m\n", line.text)),
            Op::Remove => out.push_str(&format!("\x1b[31m  - {}\x1b[0m\n", line.text)),
            Op::Same => out.push_str(&format!("\x1b[90m    {}\x1b[0m\n", line.text)),
        }
        shown += 1;
    }
    out
}

/// Marks each line as worth printing: every change, plus [`CONTEXT`] lines
/// either side of one.
fn lines_worth_showing(lines: &[Line<'_>]) -> Vec<bool> {
    let mut keep = vec![false; lines.len()];
    for (i, l) in lines.iter().enumerate() {
        if l.op == Op::Same {
            continue;
        }
        let from = i.saturating_sub(CONTEXT);
        let to = (i + CONTEXT + 1).min(lines.len());
        for k in keep.iter_mut().take(to).skip(from) {
            *k = true;
        }
    }
    keep
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ops(before: &str, after: &str) -> Vec<(Op, String)> {
        diff(before, after)
            .into_iter()
            .map(|l| (l.op, l.text.to_string()))
            .collect()
    }

    #[test]
    fn identical_content_is_all_unchanged() {
        let d = diff("a\nb\nc", "a\nb\nc");
        assert!(d.iter().all(|l| l.op == Op::Same));
        assert!(stats(&d).is_empty());
    }

    #[test]
    fn a_single_changed_line_shows_as_one_removal_and_one_addition() {
        let s = stats(&diff("a\nb\nc", "a\nB\nc"));
        assert_eq!(
            s,
            Stats {
                added: 1,
                removed: 1
            }
        );
    }

    #[test]
    fn a_pure_insertion_removes_nothing() {
        assert_eq!(
            stats(&diff("a\nc", "a\nb\nc")),
            Stats {
                added: 1,
                removed: 0
            }
        );
    }

    #[test]
    fn a_pure_deletion_adds_nothing() {
        assert_eq!(
            stats(&diff("a\nb\nc", "a\nc")),
            Stats {
                added: 0,
                removed: 1
            }
        );
    }

    #[test]
    fn creating_a_file_is_all_additions() {
        assert_eq!(
            stats(&diff("", "one\ntwo")),
            Stats {
                added: 2,
                removed: 0
            }
        );
    }

    #[test]
    fn emptying_a_file_is_all_removals() {
        assert_eq!(
            stats(&diff("one\ntwo", "")),
            Stats {
                added: 0,
                removed: 2
            }
        );
    }

    #[test]
    fn unchanged_context_is_preserved_around_an_edit() {
        let d = ops("a\nb\nc\nd", "a\nb\nX\nd");
        assert_eq!(d[0], (Op::Same, "a".into()));
        assert_eq!(d[1], (Op::Same, "b".into()));
        assert!(d.contains(&(Op::Remove, "c".into())));
        assert!(d.contains(&(Op::Add, "X".into())));
        assert_eq!(d[d.len() - 1], (Op::Same, "d".into()));
    }

    #[test]
    fn a_moved_line_is_not_reported_as_unchanged() {
        // LCS keeps the longest common run; the other copy has to show as
        // a real add/remove pair, not silently vanish.
        let s = stats(&diff("a\nb\nc", "c\na\nb"));
        assert!(s.added > 0 && s.removed > 0);
    }

    #[test]
    fn crlf_and_lf_files_diff_the_same_way() {
        // `.lines()` strips the trailing \r, so a Windows checkout and a
        // Unix one must not report every line as changed.
        assert_eq!(
            stats(&diff("a\r\nb\r\nc", "a\r\nB\r\nc")),
            stats(&diff("a\nb\nc", "a\nB\nc"))
        );
    }

    #[test]
    fn a_rewrite_too_large_to_align_still_reports_correct_counts() {
        // Past the LCS cap the *presentation* degrades to a wholesale
        // replacement; the counts must still be right and it must not hang.
        let before: String = (0..MAX_LCS_LINES + 100)
            .map(|i| format!("old line {i}\n"))
            .collect();
        let after: String = (0..MAX_LCS_LINES + 50)
            .map(|i| format!("new line {i}\n"))
            .collect();
        let s = stats(&diff(&before, &after));
        assert_eq!(s.removed, MAX_LCS_LINES + 100);
        assert_eq!(s.added, MAX_LCS_LINES + 50);
    }

    #[test]
    fn a_large_file_with_a_tiny_edit_stays_on_the_fast_path() {
        // Prefix/suffix trimming is what keeps the common case cheap: a
        // one-line change in a 5,000-line file must not hit the cap.
        let mut before: Vec<String> = (0..5_000).map(|i| format!("line {i}")).collect();
        let after = before.clone();
        before[2_500] = "changed".into();
        let s = stats(&diff(&before.join("\n"), &after.join("\n")));
        assert_eq!(
            s,
            Stats {
                added: 1,
                removed: 1
            },
            "trimming should have reduced this to a single-line diff"
        );
    }

    #[test]
    fn rendering_collapses_long_unchanged_runs() {
        let before: String = (0..60).map(|i| format!("line {i}\n")).collect();
        let mut after: Vec<String> = (0..60).map(|i| format!("line {i}")).collect();
        after[30] = "changed".into();
        let rendered = render(&diff(&before, &after.join("\n")), 200);
        assert!(rendered.contains('⋯'), "far-away context should collapse");
        assert!(rendered.contains("changed"));
        assert!(rendered.contains("line 29"), "nearby context is kept");
        assert!(!rendered.contains("line 5\n"), "distant context is not");
    }

    #[test]
    fn rendering_is_bounded_by_max_lines() {
        let before = "";
        let after: String = (0..500).map(|i| format!("line {i}\n")).collect();
        let rendered = render(&diff(before, &after), 20);
        assert!(rendered.contains("more lines not shown"));
        assert!(rendered.lines().count() < 30);
    }

    #[test]
    fn an_empty_diff_renders_to_nothing() {
        assert_eq!(render(&diff("same", "same"), 100), "");
    }
}
