use std::path::Path;

use craft_tool_macro::Tool;
use serde::Deserialize;

use crate::ToolOutput;

use super::relative_path;

#[derive(Tool, Debug, Clone, Deserialize)]
pub struct List {
    #[param(description = "Absolute path to the directory", alias = "file_path")]
    path: String,
}

impl List {
    pub const NAME: &str = "list";
    pub const DESCRIPTION: &str = include_str!("list.md");
    pub const EXAMPLES: Option<&str> = Some(r#"[{"path": "/project/src"}]"#);

    pub async fn execute(&self, ctx: &super::ToolContext) -> Result<ToolOutput, String> {
        let path = super::resolve_path(&self.path)?;
        let cwd = std::env::current_dir().ok();
        let p = Path::new(&path);

        if !p.exists() {
            return Err(format!("error: path not found: {}", relative_path(&path)));
        }
        if !p.is_dir() {
            return Err(format!(
                "error: path is not a directory: {}",
                relative_path(&path)
            ));
        }

        let (text, instructions) =
            super::list_directory(&path, cwd.as_deref(), &ctx.loaded_instructions)?;
        Ok(ToolOutput::ReadDir { text, instructions })
    }

    pub fn start_header(&self) -> String {
        relative_path(&self.path)
    }
}

super::impl_tool!(List, kind = "read", tier = super::ToolTier::Core);

impl super::ToolInvocation for List {
    fn start_header(&self) -> super::HeaderFuture {
        super::HeaderFuture::Ready(super::HeaderResult::plain(List::start_header(self)))
    }
    fn execute<'a>(self: Box<Self>, ctx: &'a super::ToolContext) -> super::ExecFuture<'a> {
        Box::pin(async move { List::execute(&self, ctx).await.into() })
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn list_directory_sorts_dirs_first_and_hides_instruction_files() {
        let dir = tempfile::TempDir::new().unwrap();
        let dir_path = dir.path().to_string_lossy().to_string();

        std::fs::write(dir.path().join("b.txt"), "").unwrap();
        std::fs::write(dir.path().join("a.rs"), "").unwrap();
        std::fs::create_dir(dir.path().join("zdir")).unwrap();
        std::fs::create_dir(dir.path().join("adir")).unwrap();
        std::fs::write(dir.path().join("AGENTS.md"), "").unwrap();

        let (text, instructions) =
            super::super::list_directory(&dir_path, None, &crate::agent::LoadedInstructions::new())
                .unwrap();
        let entries: Vec<&str> = text.lines().collect();
        assert_eq!(entries, vec!["adir/", "zdir/", "a.rs", "b.txt"]);
        assert!(instructions.is_none());
    }

    #[test]
    fn list_directory_discovers_subdirectory_instructions() {
        let root = tempfile::TempDir::new().unwrap();
        let sub = root.path().join("src");
        std::fs::create_dir_all(&sub).unwrap();
        std::fs::write(sub.join("AGENTS.md"), "sub rules").unwrap();
        std::fs::write(sub.join("lib.rs"), "").unwrap();

        let sub_path = sub.to_string_lossy().to_string();
        let loaded = crate::agent::LoadedInstructions::new();
        let (text, instructions) =
            super::super::list_directory(&sub_path, Some(root.path()), &loaded).unwrap();
        assert!(!text.is_empty());
        let blocks = instructions.expect("should have instructions");
        assert_eq!(blocks.len(), 1);
        assert!(blocks[0].path.ends_with("AGENTS.md"));
        assert_eq!(blocks[0].content, "sub rules");
    }
}
