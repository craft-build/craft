use craft_tool_macro::Tool;
use serde::Deserialize;

use argosy::codetools::outline::{self, OutlineParams};

use crate::ToolOutput;

#[derive(Tool, Debug, Clone, Deserialize)]
pub struct Outline {
    #[param(
        description = "Absolute path to a file or directory",
        alias = "file_path"
    )]
    path: String,
    #[param(
        description = "When path is a directory, return a flat file table instead of nested symbols"
    )]
    files: Option<bool>,
}

impl Outline {
    pub const NAME: &str = "outline";
    pub const DESCRIPTION: &str = include_str!("outline.md");
    pub const EXAMPLES: Option<&str> = Some(
        r#"[
  {"path": "/project/src/main.rs"},
  {"path": "/project/src/", "files": true}
]"#,
    );

    pub async fn execute(&self, ctx: &super::ToolContext) -> Result<ToolOutput, String> {
        let params = OutlineParams {
            path: self.path.clone(),
            files: self.files,
        };
        let report = outline::run(&ctx.code_tools, params).map_err(|e| e.to_string())?;
        let resolved = super::resolve_path(&self.path)?;
        if std::path::Path::new(&resolved).is_file() {
            ctx.record_read(std::path::Path::new(&resolved));
        }
        Ok(ToolOutput::Plain(report.text))
    }

    pub fn start_header(&self) -> String {
        super::relative_path(&self.path)
    }
}

super::impl_tool!(Outline, kind = "outline", tier = super::ToolTier::Core);

impl super::ToolInvocation for Outline {
    fn start_header(&self) -> super::HeaderFuture {
        super::HeaderFuture::Ready(super::HeaderResult::plain(Outline::start_header(self)))
    }
    fn execute<'a>(self: Box<Self>, ctx: &'a super::ToolContext) -> super::ExecFuture<'a> {
        Box::pin(async move { Outline::execute(&self, ctx).await.into() })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::AgentMode;
    use crate::tools::test_support::stub_ctx;

    const RUST_SRC: &str = r#"
use std::fs;

pub struct Config {
    name: String,
}

impl Config {
    pub fn new() -> Self {
        Self { name: String::new() }
    }
}

fn main() {
    let config = Config::new();
}
"#;

    #[tokio::test]
    async fn rust_outline_extracts_struct_and_fn() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("main.rs");
        std::fs::write(&path, RUST_SRC).unwrap();
        let tool = Outline {
            path: path.to_string_lossy().into_owned(),
            files: None,
        };
        let text = tool.execute(&stub_ctx(&AgentMode::Build)).await.unwrap();
        let text = text.as_text();
        assert!(text.contains("Config"), "got: {text}");
        assert!(text.contains("main"));
    }

    #[tokio::test]
    async fn unsupported_language_reports_cleanly() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("data.bin");
        std::fs::write(&path, "x").unwrap();
        let tool = Outline {
            path: path.to_string_lossy().into_owned(),
            files: None,
        };
        let text = tool.execute(&stub_ctx(&AgentMode::Build)).await.unwrap();
        assert!(text.as_text().contains("unsupported language"));
    }

    #[tokio::test]
    async fn directory_files_mode_lists_files() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.rs"), "fn a() {}").unwrap();
        std::fs::write(dir.path().join("b.rs"), "fn b() {}").unwrap();
        let tool = Outline {
            path: dir.path().to_string_lossy().into_owned(),
            files: Some(true),
        };
        let text = tool.execute(&stub_ctx(&AgentMode::Build)).await.unwrap();
        let text = text.as_text();
        assert!(text.contains("a.rs"), "got: {text}");
        assert!(text.contains("b.rs"));
    }

    #[tokio::test]
    async fn directory_outline_renders_symbols() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.rs"), "fn a() {}").unwrap();
        let tool = Outline {
            path: dir.path().to_string_lossy().into_owned(),
            files: Some(false),
        };
        let text = tool.execute(&stub_ctx(&AgentMode::Build)).await.unwrap();
        assert!(text.as_text().contains("a"));
    }

    #[tokio::test]
    async fn missing_path_errors() {
        let dir = tempfile::tempdir().unwrap();
        let tool = Outline {
            path: dir.path().join("nope.rs").to_string_lossy().into_owned(),
            files: None,
        };
        assert!(tool.execute(&stub_ctx(&AgentMode::Build)).await.is_err());
    }
}
