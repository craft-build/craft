use std::fs;

use serde_json::json;

use super::{invoke, invoke_error, output_text, workspace};
use crate::tools::*;

#[tokio::test]
async fn read_pages_utf8_crlf_and_empty_files() {
    let (_dir, workspace) = workspace();
    fs::write(workspace.root().join("file.txt"), "first\r\nβeta\r\nlast").unwrap();
    let tool = Read(workspace.clone());
    let page = invoke(&tool, json!({"path":"file.txt","offset":2,"limit":1})).await;
    assert_eq!(
        output_text(&page),
        "2: βeta\n\n...\n\nTruncated lines: 3-3. Use offset=3 to read further."
    );
    let page = invoke(&tool, json!({"path":"file.txt","offset":4})).await;
    assert_eq!(output_text(&page), "");
    assert!(
        invoke_error(&tool, json!({"path":"file.txt","offset":0}))
            .await
            .contains("offset must be >= 1")
    );
    assert!(
        invoke_error(&tool, json!({"path":"file.txt","offset":5}))
            .await
            .contains("offset exceeds end of file")
    );
    fs::write(workspace.root().join("empty"), "").unwrap();
    let empty = invoke(&tool, json!({"path":"empty"})).await;
    assert_eq!(output_text(&empty), "");
}

#[tokio::test]
async fn read_reports_line_and_page_truncation() {
    let (_dir, workspace) = workspace();
    let content = format!("{}\n", "🦀".repeat(1024)).repeat(100);
    fs::write(workspace.root().join("long"), content).unwrap();
    let page = invoke(&Read(workspace), json!({"path":"long","limit":0})).await;
    let text = output_text(&page);
    assert!(
        text.starts_with(&format!("1: {}...\n2: ", "🦀".repeat(MAX_LINE_BYTES / 4))),
        "{text}"
    );
    let lines: Vec<&str> = text
        .split("\n\n...\n\nTruncated lines: ")
        .next()
        .unwrap()
        .lines()
        .collect();
    assert!(lines.iter().all(|line| line.ends_with("...")));
    let truncation = text
        .split("\n\n...\n\nTruncated lines: ")
        .nth(1)
        .expect("page truncation hint");
    assert!(
        truncation.starts_with(|c: char| c.is_ascii_digit()),
        "{text}"
    );
    assert!(
        text.contains(". Use offset=") && text.ends_with(" to read further."),
        "{text}"
    );
}

#[tokio::test]
async fn read_full_and_eof_pages_are_literal_numbered_text() {
    let (_dir, workspace) = workspace();
    fs::write(workspace.root().join("file"), "{\"lines\": []}\n\n  βeta\n").unwrap();
    let tool = Read(workspace);
    let full = invoke(&tool, json!({"path":"file"})).await;
    assert_eq!(output_text(&full), "1: {\"lines\": []}\n2: \n3:   βeta");
    let eof = invoke(&tool, json!({"path":"file", "offset":4})).await;
    assert_eq!(output_text(&eof), "");
}

#[tokio::test]
async fn read_rejects_binary_and_oversized_files() {
    let (_dir, workspace) = workspace();
    fs::write(workspace.root().join("binary"), [0, 255]).unwrap();
    fs::File::create(workspace.root().join("large"))
        .unwrap()
        .set_len((MAX_FILE_BYTES + 1) as u64)
        .unwrap();
    let binary = invoke_error(&Read(workspace.clone()), json!({"path":"binary"})).await;
    assert!(binary.contains("binary files"), "{binary}");
    let large = invoke_error(&Read(workspace.clone()), json!({"path":"large"})).await;
    assert!(large.contains("8 MiB"), "{large}");
}

#[tokio::test]
async fn read_injects_subdirectory_instructions_once() {
    let (dir, workspace) = workspace();
    fs::create_dir_all(dir.path().join("src/api")).unwrap();
    fs::write(dir.path().join("src/AGENTS.md"), "api rules").unwrap();
    fs::write(dir.path().join("src/api/handler.rs"), "fn main() {}").unwrap();

    let tool = Read(workspace.clone());
    let page = invoke(&tool, json!({"path":"src/api/handler.rs"})).await;
    let text = output_text(&page);
    assert!(text.contains("\n\n---\nInstructions from: "), "{text}");
    assert!(text.ends_with("api rules"));

    // Second read of a sibling file: the instruction file is not repeated.
    fs::write(dir.path().join("src/api/other.rs"), "fn other() {}").unwrap();
    let page = invoke(&tool, json!({"path":"src/api/other.rs"})).await;
    assert!(!output_text(&page).contains("Instructions from:"));
}

#[tokio::test]
async fn read_of_instruction_file_injects_nothing() {
    let (dir, workspace) = workspace();
    fs::write(dir.path().join("AGENTS.md"), "root rules").unwrap();

    let tool = Read(workspace.clone());
    let page = invoke(&tool, json!({"path":"AGENTS.md"})).await;
    assert!(!output_text(&page).contains("Instructions from:"));
}

#[tokio::test]
async fn read_files_at_workspace_root_inject_nothing() {
    let (dir, workspace) = workspace();
    fs::write(dir.path().join("AGENTS.md"), "root rules").unwrap();
    fs::write(dir.path().join("file.txt"), "content").unwrap();

    let tool = Read(workspace.clone());
    let page = invoke(&tool, json!({"path":"file.txt"})).await;
    assert!(!output_text(&page).contains("Instructions from:"));
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
    let page = invoke(&Read(workspace.clone()), json!({"path":"argosy://catalog"})).await;
    let text = output_text(&page);
    assert!(
        text.contains("Argosy catalog"),
        "expected the catalog markdown: {text}"
    );
}
