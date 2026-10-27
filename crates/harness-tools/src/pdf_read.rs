//! Text extraction from PDFs, for `read_file`.
//!
//! Parsing runs in a child process when the host registers one. The release
//! build aborts on panic and PDF parsers can panic on damaged files, so an
//! in-process crash would take the whole agent down with it.

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::OnceLock;
use std::time::Duration;

use serde::{Deserialize, Serialize};

const WORKER_TIMEOUT: Duration = Duration::from_secs(60);
pub(crate) const MAX_PDF_BYTES: u64 = 200 * 1024 * 1024;
/// Pages read when the caller names none.
pub(crate) const DEFAULT_PAGES: usize = 40;
/// The most pages one call decodes, however wide the requested range.
pub(crate) const MAX_PAGES: usize = 60;
/// Exit code the worker uses for an ordinary, explained failure.
const WORKER_FAILED: i32 = 2;
/// Below this many non-blank characters per page, a PDF has no text layer.
const MIN_CHARS_PER_PAGE: usize = 20;

#[derive(Debug, Serialize, Deserialize, PartialEq)]
pub(crate) struct PdfExtract {
    pub page_count: usize,
    pub pages: Vec<PdfPage>,
}

#[derive(Debug, Serialize, Deserialize, PartialEq)]
pub(crate) struct PdfPage {
    pub number: usize,
    pub text: String,
}

static WORKER: OnceLock<PathBuf> = OnceLock::new();

/// The host's own executable, started as `__pdf-worker`. Without one,
/// extraction runs in-process, which is only safe where panics unwind.
pub fn set_worker_exe(exe: PathBuf) {
    let _ = WORKER.set(exe);
}

/// Body of the host's hidden `__pdf-worker` subcommand. Returns its exit code.
pub fn run_worker(path: &Path, first: usize, last: usize, byte_budget: usize) -> i32 {
    use std::io::Write;
    match extract_in_process(path, first, last, byte_budget) {
        Ok(out) => {
            let mut stdout = std::io::stdout().lock();
            match serde_json::to_writer(&mut stdout, &out).map_err(|e| e.to_string()) {
                Ok(()) => stdout.flush().map_or(1, |()| 0),
                Err(_) => 1,
            }
        }
        Err(message) => {
            eprintln!("{message}");
            WORKER_FAILED
        }
    }
}

/// Reads pages `first..=last` (1-based), stopping early once `byte_budget`
/// is spent. The first page is always included, so one dense page can't
/// stall a read.
pub(crate) fn extract_in_process(
    path: &Path,
    first: usize,
    last: usize,
    byte_budget: usize,
) -> Result<PdfExtract, String> {
    let read = || -> Result<PdfExtract, String> {
        let doc = pdf_oxide::PdfDocument::open(path).map_err(|e| e.to_string())?;
        let page_count = doc.page_count().map_err(|e| e.to_string())?;
        if page_count == 0 {
            return Err("the PDF has no pages".into());
        }
        let mut pages: Vec<PdfPage> = Vec::new();
        let mut used = 0usize;
        for number in first.max(1)..=last.min(page_count) {
            // One damaged page shouldn't hide the rest of the document.
            let text = doc
                .extract_text(number - 1)
                .unwrap_or_else(|e| format!("[this page could not be read: {e}]"));
            used += text.len();
            if !pages.is_empty() && used > byte_budget {
                break;
            }
            pages.push(PdfPage { number, text });
        }
        Ok(PdfExtract { page_count, pages })
    };
    std::panic::catch_unwind(std::panic::AssertUnwindSafe(read))
        .unwrap_or_else(|_| Err("the PDF reader hit an internal error on this file".into()))
}

pub(crate) async fn extract(path: &Path, first: usize, last: usize) -> Result<PdfExtract, String> {
    let budget = crate::fs::MAX_READ_BYTES;
    match WORKER.get() {
        Some(exe) => run_child(exe, path, first, last, budget).await,
        None => {
            let path = path.to_path_buf();
            tokio::task::spawn_blocking(move || extract_in_process(&path, first, last, budget))
                .await
                .map_err(|e| e.to_string())?
        }
    }
}

async fn run_child(
    exe: &Path,
    path: &Path,
    first: usize,
    last: usize,
    budget: usize,
) -> Result<PdfExtract, String> {
    let child = tokio::process::Command::new(exe)
        .arg("__pdf-worker")
        .arg(path)
        .args([first, last, budget].map(|n| n.to_string()))
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .map_err(|e| format!("could not start the PDF reader: {e}"))?;
    let output = tokio::time::timeout(WORKER_TIMEOUT, child.wait_with_output())
        .await
        .map_err(|_| "reading this PDF took over a minute, so it was stopped".to_string())?
        .map_err(|e| e.to_string())?;
    if output.status.code() == Some(WORKER_FAILED) {
        return Err(String::from_utf8_lossy(&output.stderr).trim().to_string());
    }
    if !output.status.success() {
        return Err("the PDF reader crashed on this file, which may be damaged".into());
    }
    serde_json::from_slice(&output.stdout)
        .map_err(|_| "the PDF reader returned something unreadable".to_string())
}

/// A `pages` argument: `5`, or `3-9`. Both ends are 1-based and inclusive.
pub(crate) fn parse_page_range(spec: &str) -> Result<(usize, usize), String> {
    let bad = || format!("pages must be a page number or a range like 3-9, got {spec:?}");
    let spec = spec.trim();
    let (first, last) = match spec.split_once('-') {
        Some((a, b)) => (
            a.trim().parse().map_err(|_| bad())?,
            b.trim().parse().map_err(|_| bad())?,
        ),
        None => {
            let page = spec.parse().map_err(|_| bad())?;
            (page, page)
        }
    };
    if first == 0 || last < first {
        return Err(bad());
    }
    Ok((first, last))
}

/// What the model sees: each page under a marker, and a note saying which
/// pages these were and how to read on. Without the note a partial read
/// looks like the whole document.
pub(crate) fn render(extract: &PdfExtract, first: usize) -> String {
    let Some(last_page) = extract.pages.last().map(|p| p.number) else {
        return format!(
            "[page {first} is past the end of the PDF: {} page(s) total]",
            extract.page_count
        );
    };
    let first_page = extract.pages[0].number;
    let text_chars: usize = extract
        .pages
        .iter()
        .map(|p| p.text.chars().filter(|c| !c.is_whitespace()).count())
        .sum();
    let mut out = if text_chars < MIN_CHARS_PER_PAGE * extract.pages.len() {
        "[no text could be read from these pages. The PDF looks scanned (page images with no text \
         layer), so it needs OCR, which this tool does not do. Tell the user rather than guessing \
         what the pages say.]"
            .to_string()
    } else {
        extract
            .pages
            .iter()
            .map(|p| format!("--- page {} ---\n{}", p.number, p.text.trim()))
            .collect::<Vec<_>>()
            .join("\n\n")
    };
    out.push_str(&format!(
        "\n\n[pages {first_page}-{last_page} of {}",
        extract.page_count
    ));
    if last_page < extract.page_count {
        out.push_str(&format!("; read on with pages=\"{}\"", last_page + 1));
    }
    out.push(']');
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn page(number: usize, text: &str) -> PdfPage {
        PdfPage {
            number,
            text: text.into(),
        }
    }

    #[test]
    fn page_ranges_accept_one_page_or_a_span() {
        assert_eq!(parse_page_range("5"), Ok((5, 5)));
        assert_eq!(parse_page_range(" 3 - 9 "), Ok((3, 9)));
        for bad in ["", "0", "9-3", "a-b", "3-", "-4", "1-2-3", "2.5"] {
            assert!(parse_page_range(bad).is_err(), "{bad:?} should be rejected");
        }
    }

    #[test]
    fn pages_are_marked_and_the_note_says_how_to_read_on() {
        let extract = PdfExtract {
            page_count: 10,
            pages: vec![
                page(3, "  The first page of text that is long enough.  "),
                page(4, "Second page, also with real words in it."),
            ],
        };
        let out = render(&extract, 3);
        assert!(out.contains("--- page 3 ---\nThe first page of text that is long enough."));
        assert!(out.contains("--- page 4 ---\nSecond page"));
        assert!(
            out.ends_with("[pages 3-4 of 10; read on with pages=\"5\"]"),
            "{out}"
        );
    }

    #[test]
    fn the_last_page_has_no_read_on_hint() {
        let extract = PdfExtract {
            page_count: 2,
            pages: vec![page(1, &"word ".repeat(30)), page(2, &"more ".repeat(30))],
        };
        assert!(render(&extract, 1).ends_with("[pages 1-2 of 2]"));
    }

    #[test]
    fn a_page_past_the_end_says_how_long_the_pdf_is() {
        let extract = PdfExtract {
            page_count: 4,
            pages: Vec::new(),
        };
        assert_eq!(
            render(&extract, 9),
            "[page 9 is past the end of the PDF: 4 page(s) total]"
        );
    }

    #[test]
    fn pages_with_no_text_are_reported_as_scanned_not_as_blank() {
        let extract = PdfExtract {
            page_count: 3,
            pages: vec![page(1, " \n"), page(2, ""), page(3, "7")],
        };
        let out = render(&extract, 1);
        assert!(
            out.contains("looks scanned") && out.contains("needs OCR"),
            "{out}"
        );
        assert!(!out.contains("--- page"));
    }

    #[test]
    fn one_blank_page_among_real_ones_is_not_called_scanned() {
        let extract = PdfExtract {
            page_count: 2,
            pages: vec![page(1, ""), page(2, &"text ".repeat(40))],
        };
        let out = render(&extract, 1);
        assert!(!out.contains("looks scanned"), "{out}");
        assert!(out.contains("--- page 1 ---"));
    }

    #[tokio::test]
    async fn a_worker_that_dies_is_reported_as_a_crash_not_propagated() {
        // libtest exits with an error on an unknown flag, standing in for a
        // worker that aborts mid-parse.
        let exe = std::env::current_exe().unwrap();
        let err = run_child(&exe, Path::new("--no-such-flag"), 1, 2, 1_000)
            .await
            .unwrap_err();
        assert!(err.contains("crashed"), "{err}");
    }

    #[tokio::test]
    async fn a_missing_worker_binary_is_an_error() {
        let err = run_child(
            Path::new("/definitely/not/here"),
            Path::new("a.pdf"),
            1,
            2,
            1_000,
        )
        .await
        .unwrap_err();
        assert!(err.contains("could not start"), "{err}");
    }

    #[test]
    fn garbage_and_missing_files_are_errors_not_crashes() {
        let dir = std::env::temp_dir().join(format!("hivemind_pdf_garbage_{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let junk = dir.join("junk.pdf");
        std::fs::write(
            &junk,
            b"%PDF-1.4\nthis is not a real pdf \xff\xfe\x00 at all",
        )
        .unwrap();
        assert!(extract_in_process(&junk, 1, 5, 1_000).is_err());
        assert!(extract_in_process(&dir.join("absent.pdf"), 1, 5, 1_000).is_err());
        let empty = dir.join("empty.pdf");
        std::fs::write(&empty, b"").unwrap();
        assert!(extract_in_process(&empty, 1, 5, 1_000).is_err());
    }
}
