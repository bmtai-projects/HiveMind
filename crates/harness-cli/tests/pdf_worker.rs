//! `read_file` on a PDF, through the real `hivemind __pdf-worker` child process.
#![cfg(feature = "pdf")]

use harness_tools::{CreatePdf, ReadFile, Tool, Workspace};
use serde_json::value::RawValue;

fn args(v: serde_json::Value) -> Box<RawValue> {
    RawValue::from_string(v.to_string()).unwrap()
}

fn workspace() -> Workspace {
    let dir = std::env::temp_dir().join(format!("hivemind_pdf_worker_{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    Workspace::new(dir)
}

#[tokio::test]
async fn pdf_text_comes_back_through_the_worker_and_damage_cannot_crash_the_caller() {
    harness_tools::set_pdf_worker_exe(env!("CARGO_BIN_EXE_hivemind").into());
    let w = workspace();

    let markdown: String = (1..=150)
        .map(|i| format!("Paragraph {i} about photosynthesis in green plants.\n\n"))
        .collect();
    std::fs::write(w.root.join("source.md"), markdown).unwrap();
    CreatePdf(w.clone())
        .execute(&args(
            serde_json::json!({"path": "notes.pdf", "from_file": "source.md", "title": "Notes"}),
        ))
        .await
        .unwrap();

    let read = |path: &'static str, pages: Option<&'static str>| {
        let w = w.clone();
        async move {
            let mut call = serde_json::json!({ "path": path });
            if let Some(pages) = pages {
                call["pages"] = pages.into();
            }
            ReadFile(w).execute(&args(call)).await
        }
    };

    let whole = read("notes.pdf", None).await.unwrap().summary;
    assert!(whole.contains("--- page 1 ---"), "{whole}");
    assert!(
        whole.contains("Paragraph 1 about photosynthesis"),
        "{whole}"
    );

    let second = read("notes.pdf", Some("2")).await.unwrap().summary;
    assert!(
        second.contains("--- page 2 ---") && !second.contains("--- page 1 ---"),
        "{second}"
    );

    let bytes = std::fs::read(w.root.join("notes.pdf")).unwrap();
    for (name, damaged) in [
        ("half.pdf", bytes[..bytes.len() / 2].to_vec()),
        ("tail.pdf", bytes[bytes.len() / 2..].to_vec()),
        (
            "noise.pdf",
            b"%PDF-1.7\n\xff\xfe\x00\x01 broken \x00".to_vec(),
        ),
    ] {
        std::fs::write(w.root.join(name), damaged).unwrap();
        let name: &'static str = Box::leak(name.to_string().into_boxed_str());
        match read(name, None).await {
            Err(why) => assert!(why.to_string().contains("could not read"), "{name}: {why}"),
            Ok(text) => assert!(
                !text.summary.contains('\u{fffd}'),
                "{name}: {}",
                text.summary
            ),
        }
    }

    // Still alive, and still able to read the good file afterwards.
    assert!(read("notes.pdf", Some("1")).await.is_ok());
}
