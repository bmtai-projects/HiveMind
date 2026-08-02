//! Real-`mmdc` end-to-end verification for `create_diagram`.
//!
//! `#[ignore]`d like the other real-tool harnesses here (`real_repo_smoke`,
//! `secret_scan_false_positives`): it depends on `mmdc` (mermaid-cli)
//! actually being installed and runnable, which CI and a fresh checkout
//! don't guarantee. The unit tests in `diagram.rs` cover validation and the
//! source-only path with no subprocess involved; this is what proves the
//! real render path — the part those tests can't reach — actually works
//! against the real tool, not just a mocked one.
//!
//! Run with:
//!   cargo test -p harness-tools --test diagram_real_mmdc -- --ignored --nocapture

use harness_tools::{CreateDiagram, Tool, Workspace};
use serde_json::value::RawValue;

fn ws(name: &str) -> Workspace {
    let dir = std::env::temp_dir().join(format!("hivemind_diagram_e2e_{name}"));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    Workspace::new(dir)
}

fn args(json: serde_json::Value) -> Box<RawValue> {
    RawValue::from_string(json.to_string()).unwrap()
}

#[tokio::test]
#[ignore = "needs a real mmdc install"]
async fn a_valid_flowchart_renders_to_a_real_svg() {
    let w = ws("svg");
    let out = CreateDiagram(w.clone())
        .execute(&args(serde_json::json!({
            "path": "flow.svg",
            "diagram": "flowchart LR\n  A[Start] --> B{Decide}\n  B -->|Yes| C[Do it]\n  B -->|No| D[Skip]"
        })))
        .await
        .expect("tool call itself must succeed");

    assert!(out.contains("rendered"), "got {out:?}");
    assert!(!out.contains("mmdc failed"), "got {out:?}");
    assert!(!out.contains("isn't installed"), "got {out:?}");

    let svg_path = w.root.join("flow.svg");
    let mmd_path = w.root.join("flow.mmd");
    assert!(svg_path.exists(), "svg was not written");
    assert!(
        mmd_path.exists(),
        "the .mmd sibling must always be written too"
    );

    let svg = std::fs::read_to_string(&svg_path).unwrap();
    assert!(svg.contains("<svg"), "not a real svg document: {svg:?}");
    // Node labels should actually appear in the rendered output, not just
    // in the source -- proves layout genuinely ran, not just a passthrough.
    assert!(svg.contains("Start"));
    assert!(svg.contains("Decide"));

    let mmd = std::fs::read_to_string(&mmd_path).unwrap();
    assert!(mmd.contains("flowchart LR"));
}

#[tokio::test]
#[ignore = "needs a real mmdc install"]
async fn a_valid_flowchart_renders_to_a_real_png() {
    let w = ws("png");
    let out = CreateDiagram(w.clone())
        .execute(&args(serde_json::json!({
            "path": "flow.png",
            "diagram": "flowchart LR\nA-->B"
        })))
        .await
        .expect("tool call itself must succeed");

    assert!(out.contains("rendered"), "got {out:?}");
    let png_path = w.root.join("flow.png");
    assert!(png_path.exists());
    let bytes = std::fs::read(&png_path).unwrap();
    // PNG magic number -- cheap, dependency-free proof this is a real,
    // well-formed image rather than an empty or garbage file.
    assert_eq!(
        &bytes[..8],
        &[0x89, b'P', b'N', b'G', b'\r', b'\n', 0x1a, b'\n']
    );
}

#[tokio::test]
#[ignore = "needs a real mmdc install"]
async fn different_diagram_types_all_render() {
    // One representative from a few different diagram families -- proves
    // the tool isn't accidentally coupled to flowchart-shaped output.
    let cases: &[(&str, &str)] = &[
        (
            "seq.svg",
            "sequenceDiagram\n  Alice->>Bob: Hello\n  Bob-->>Alice: Hi",
        ),
        (
            "pie.svg",
            "pie title Pets\n  \"Dogs\" : 40\n  \"Cats\" : 60",
        ),
        (
            "class.svg",
            "classDiagram\n  Animal <|-- Dog\n  Animal : +String name",
        ),
        (
            "state.svg",
            "stateDiagram-v2\n  [*] --> Idle\n  Idle --> Running",
        ),
    ];
    for (name, diagram) in cases {
        let w = ws(&format!("family_{name}"));
        let out = CreateDiagram(w.clone())
            .execute(&args(
                serde_json::json!({"path": *name, "diagram": *diagram}),
            ))
            .await
            .unwrap_or_else(|e| panic!("{name} failed: {e}"));
        assert!(out.contains("rendered"), "{name}: got {out:?}");
        assert!(w.root.join(name).exists(), "{name}: image missing");
    }
}

#[tokio::test]
#[ignore = "needs a real mmdc install"]
async fn a_syntactically_broken_diagram_degrades_gracefully_instead_of_failing_the_call() {
    let w = ws("broken");
    let out = CreateDiagram(w.clone())
        .execute(&args(serde_json::json!({
            "path": "broken.svg",
            // Deliberately malformed: an edge with no target, which mmdc's
            // real parser should reject.
            "diagram": "flowchart LR\n  A --> \n  ---> broken >>>"
        })))
        .await
        .expect("a render failure must not make the whole tool call fail");

    assert!(
        out.contains("wrote"),
        "the .mmd source must still be reported written: {out:?}"
    );
    assert!(
        out.contains("mmdc failed") || out.contains("syntax error"),
        "should explain the render failed: {out:?}"
    );
    assert!(
        w.root.join("broken.mmd").exists(),
        "source must be written even when rendering fails"
    );
    assert!(
        !w.root.join("broken.svg").exists(),
        "a failed render must not leave a partial/empty image file behind"
    );
}

#[tokio::test]
#[ignore = "needs a real mmdc install"]
async fn a_diagram_with_a_space_in_its_output_path_still_renders() {
    // Exercises the shell-quoting path for real -- this is exactly the case
    // `shell_quote_path` exists for.
    let w = ws("spaces");
    std::fs::create_dir_all(w.root.join("my diagrams")).unwrap();
    let out = CreateDiagram(w.clone())
        .execute(&args(serde_json::json!({
            "path": "my diagrams/flow chart.svg",
            "diagram": "flowchart LR\nA-->B"
        })))
        .await
        .expect("tool call must succeed");
    assert!(out.contains("rendered"), "got {out:?}");
    assert!(w.root.join("my diagrams/flow chart.svg").exists());
    assert!(w.root.join("my diagrams/flow chart.mmd").exists());
}
