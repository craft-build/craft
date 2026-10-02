use std::fs;

use rig_core::tool::IntoToolOutput;
use serde_json::json;

use super::{invoke, workspace};
use crate::tools::*;

#[tokio::test]
async fn read_pages_utf8_crlf_and_empty_files() {
    let (_dir, workspace) = workspace();
    fs::write(workspace.root().join("file.txt"), "first\r\nβeta\r\nlast").unwrap();
    let tool = Read(workspace.clone());
    let page = invoke(&tool, json!({"path":"file.txt","offset":2,"limit":1}))
        .await
        .unwrap();
    assert_eq!(page.total_lines, 3);
    assert_eq!(page.lines[0].number, 2);
    assert_eq!(page.lines[0].text, "βeta");
    assert_eq!(page.next_offset, Some(3));
    assert_eq!(
        page.into_tool_output().unwrap().as_text(),
        Some("2: βeta\n\n...\n\nTruncated lines: 3-3. Use offset=3 to read further.")
    );
    assert_eq!(
        invoke(&tool, json!({"path":"file.txt","offset":4}))
            .await
            .unwrap()
            .lines
            .len(),
        0
    );
    assert!(
        invoke(&tool, json!({"path":"file.txt","offset":0}))
            .await
            .is_err()
    );
    assert!(
        invoke(&tool, json!({"path":"file.txt","offset":5}))
            .await
            .is_err()
    );
    fs::write(workspace.root().join("empty"), "").unwrap();
    let empty = invoke(&tool, json!({"path":"empty"})).await.unwrap();
    assert_eq!(empty.total_lines, 0);
    assert_eq!(empty.next_offset, None);
    assert_eq!(empty.into_tool_output().unwrap().as_text(), Some(""));
}

#[tokio::test]
async fn read_reports_line_and_page_truncation() {
    let (_dir, workspace) = workspace();
    let content = format!("{}\n", "🦀".repeat(1024)).repeat(100);
    fs::write(workspace.root().join("long"), content).unwrap();
    let page = invoke(&Read(workspace), json!({"path":"long","limit":0}))
        .await
        .unwrap();
    assert!(
        page.lines
            .iter()
            .all(|line| line.truncated && line.text.len() <= MAX_LINE_BYTES)
    );
    assert!(page.lines.iter().map(|line| line.text.len()).sum::<usize>() <= MAX_OUTPUT_BYTES);
    assert_eq!(page.next_offset, Some(page.lines.len() + 1));
    let next = page.next_offset.unwrap();
    let rendered = page.into_tool_output().unwrap();
    let text = rendered.as_text().unwrap();
    assert!(text.starts_with(&format!("1: {}...\n2: ", "🦀".repeat(MAX_LINE_BYTES / 4))));
    assert!(text.ends_with(&format!(
        "Truncated lines: {next}-100. Use offset={next} to read further."
    )));
}

#[tokio::test]
async fn read_full_and_eof_pages_are_literal_numbered_text() {
    let (_dir, workspace) = workspace();
    fs::write(workspace.root().join("file"), "{\"lines\": []}\n\n  βeta\n").unwrap();
    let tool = Read(workspace);
    let full = invoke(&tool, json!({"path":"file"})).await.unwrap();
    assert_eq!(
        full.into_tool_output().unwrap().as_text(),
        Some("1: {\"lines\": []}\n2: \n3:   βeta")
    );
    let eof = invoke(&tool, json!({"path":"file", "offset":4}))
        .await
        .unwrap();
    assert_eq!(eof.into_tool_output().unwrap().as_text(), Some(""));
}

#[tokio::test]
async fn read_rejects_binary_and_oversized_files() {
    let (_dir, workspace) = workspace();
    fs::write(workspace.root().join("binary"), [0, 255]).unwrap();
    fs::File::create(workspace.root().join("large"))
        .unwrap()
        .set_len((MAX_FILE_BYTES + 1) as u64)
        .unwrap();
    for path in ["binary", "large"] {
        assert!(
            invoke(&Read(workspace.clone()), json!({"path":path}))
                .await
                .is_err()
        );
    }
}

#[tokio::test]
async fn read_injects_subdirectory_instructions_once() {
    let (dir, workspace) = workspace();
    fs::create_dir_all(dir.path().join("src/api")).unwrap();
    fs::write(dir.path().join("src/AGENTS.md"), "api rules").unwrap();
    fs::write(dir.path().join("src/api/handler.rs"), "fn main() {}").unwrap();

    let tool = Read(workspace.clone());
    let page = invoke(&tool, json!({"path":"src/api/handler.rs"}))
        .await
        .unwrap();
    assert_eq!(page.instructions.len(), 1);
    assert!(page.instructions[0].0.ends_with("AGENTS.md"));
    assert_eq!(page.instructions[0].1, "api rules");
    let text = page
        .into_tool_output()
        .unwrap()
        .as_text()
        .unwrap()
        .to_string();
    assert!(text.contains("\n\n---\nInstructions from: "));
    assert!(text.ends_with("api rules"));

    // Second read of a sibling file: the instruction file is not repeated.
    fs::write(dir.path().join("src/api/other.rs"), "fn other() {}").unwrap();
    let page = invoke(&tool, json!({"path":"src/api/other.rs"}))
        .await
        .unwrap();
    assert!(page.instructions.is_empty());
}

#[tokio::test]
async fn read_of_instruction_file_injects_nothing() {
    let (dir, workspace) = workspace();
    fs::write(dir.path().join("AGENTS.md"), "root rules").unwrap();

    let tool = Read(workspace.clone());
    let page = invoke(&tool, json!({"path":"AGENTS.md"})).await.unwrap();
    assert!(page.instructions.is_empty());
}

#[tokio::test]
async fn read_files_at_workspace_root_inject_nothing() {
    let (dir, workspace) = workspace();
    fs::write(dir.path().join("AGENTS.md"), "root rules").unwrap();
    fs::write(dir.path().join("file.txt"), "content").unwrap();

    let tool = Read(workspace.clone());
    let page = invoke(&tool, json!({"path":"file.txt"})).await.unwrap();
    assert!(page.instructions.is_empty());
}

/// Live smoke test of the builtin `argosy://` namespace: requires argosy
/// installed and configured locally, so it is ignored by default. Run with
/// `cargo test -p craft --lib -- tools::tests::read::live -- --include-ignored`.
/// Reads the global `catalog` pseudo-resource: unlike project-scoped URIs
/// it needs no project argosy, so it works on any machine with argosy.
#[tokio::test]
#[ignore = "requires a local argosy installation"]
async fn live_read_serves_the_builtin_argosy_namespace() {
    let (_dir, workspace) = workspace();
    let page = invoke(&Read(workspace.clone()), json!({"path":"argosy://catalog"}))
        .await
        .unwrap();
    assert_eq!(page.path, "argosy://catalog");
    assert!(
        page.lines.iter().any(|l| l.text.contains("Argosy catalog")),
        "expected the catalog markdown"
    );
}
