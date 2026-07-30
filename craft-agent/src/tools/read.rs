use std::fmt::Write;
use std::fs;
use std::path::Path;
use std::sync::Arc;

use crate::agent::{self, LoadedInstructions};
use crate::{ImageMediaType, ImageSource, InstructionBlock, ToolOutput};
use base64::Engine;
use craft_tool_macro::Tool;
use serde::Deserialize;

use super::{relative_path, truncate_bytes};

const MAX_IMAGE_BYTES: usize = 20 * 1024 * 1024;

const IMAGE_EXTENSIONS: &[(&str, ImageMediaType)] = &[
    ("png", ImageMediaType::Png),
    ("jpg", ImageMediaType::Jpeg),
    ("jpeg", ImageMediaType::Jpeg),
    ("gif", ImageMediaType::Gif),
    ("webp", ImageMediaType::Webp),
];

fn image_media_type(path: &Path) -> Option<ImageMediaType> {
    let ext = path.extension()?.to_str()?.to_ascii_lowercase();
    IMAGE_EXTENSIONS
        .iter()
        .find(|(e, _)| *e == ext)
        .map(|(_, mt)| *mt)
}

#[derive(Tool, Debug, Clone, Deserialize)]
pub struct Read {
    #[param(
        description = "Absolute path to the file or directory",
        alias = "file_path"
    )]
    path: String,
    #[param(description = "Line number to start from (1-indexed). Use 1 for the first line.")]
    offset: usize,
    #[param(
        description = "Max number of lines to read. Use 0 to read until end of file (capped at 2000 lines)."
    )]
    limit: usize,
}

fn to_instruction_blocks(found: Vec<(String, String)>) -> Option<Vec<InstructionBlock>> {
    if found.is_empty() {
        return None;
    }
    Some(
        found
            .into_iter()
            .map(|(path, content)| InstructionBlock { path, content })
            .collect(),
    )
}

impl Read {
    pub const NAME: &str = "read";
    pub const DESCRIPTION: &str = include_str!("read.md");
    pub const EXAMPLES: Option<&str> =
        Some(r#"[{"path": "/project/src/main.rs", "offset": 10, "limit": 20}]"#);

    pub async fn execute(&self, ctx: &super::ToolContext) -> Result<ToolOutput, String> {
        if super::internal_urls::handles(&self.path) {
            return super::internal_urls::resolve(&self.path, ctx).await;
        }
        let path = super::resolve_path(&self.path)?;
        let cwd = std::env::current_dir().ok();
        let p = Path::new(&path);
        if p.is_dir() {
            return Self::list_dir(&path, cwd.as_deref(), &ctx.loaded_instructions);
        }

        if let Some(media_type) = image_media_type(p) {
            return Self::read_image(&path, p, media_type);
        }

        let raw = ctx.fs.read_text_file(p).await?;
        let max_output_lines = ctx.config.max_output_lines;
        let max_line_bytes = ctx.config.max_line_bytes;
        let start = self.offset.saturating_sub(1);
        let limit = if self.limit == 0 {
            max_output_lines
        } else {
            self.limit.min(max_output_lines)
        };

        let total_lines = raw.lines().count();
        let prefix: String = if start == 0 {
            String::new()
        } else {
            raw.lines().take(start).map(|l| format!("{l}\n")).collect()
        };
        let lines: Vec<String> = raw
            .lines()
            .skip(start)
            .take(limit)
            .map(|l| truncate_bytes(l, max_line_bytes))
            .collect();

        let instructions = cwd.as_deref().and_then(|cwd| {
            if agent::is_instruction_file(p.file_name()?.to_str()?) {
                return None;
            }
            to_instruction_blocks(agent::find_subdirectory_instructions(
                p.parent()?,
                cwd,
                &ctx.loaded_instructions,
            ))
        });

        ctx.file_tracker.record_read(p);

        Ok(ToolOutput::ReadCode {
            path,
            start_line: start + 1,
            lines,
            prefix,
            total_lines,
            instructions,
            no_compress: true,
        })
    }

    fn read_image(
        display_path: &str,
        path: &Path,
        media_type: ImageMediaType,
    ) -> Result<ToolOutput, String> {
        let bytes = fs::read(path).map_err(|e| format!("read error: {e}"))?;
        if bytes.len() > MAX_IMAGE_BYTES {
            return Err(format!(
                "image file is {} bytes, exceeds {} byte limit",
                bytes.len(),
                MAX_IMAGE_BYTES
            ));
        }
        let data = base64::engine::general_purpose::STANDARD.encode(&bytes);
        let caption = format!("[image {display_path}]");
        Ok(ToolOutput::Image {
            caption,
            source: ImageSource::new(media_type, Arc::from(data)),
        })
    }

    fn list_dir(
        path: &str,
        cwd: Option<&Path>,
        loaded: &LoadedInstructions,
    ) -> Result<ToolOutput, String> {
        let entries = fs::read_dir(path).map_err(|e| format!("read error: {e}"))?;

        let mut dirs = Vec::new();
        let mut files = Vec::new();

        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().into_owned();
            if entry.file_type().is_ok_and(|ft| ft.is_dir()) {
                dirs.push(format!("{name}/"));
            } else {
                files.push(name);
            }
        }

        dirs.sort_unstable();
        files.sort_unstable();
        files.retain(|name| !agent::is_instruction_file(name));
        dirs.append(&mut files);
        let text = dirs.join("\n");

        let instructions = cwd.and_then(|cwd| {
            to_instruction_blocks(agent::find_subdirectory_instructions(
                Path::new(path),
                cwd,
                loaded,
            ))
        });

        Ok(ToolOutput::ReadDir { text, instructions })
    }

    pub fn start_header(&self) -> String {
        let mut s = relative_path(&self.path);
        let start = self.offset;
        if self.limit > 0 {
            let _ = write!(s, ":{start}-{}", start + self.limit - 1);
        } else {
            let _ = write!(s, ":{start}");
        }
        s
    }
}

super::impl_tool!(Read, kind = "read", tier = super::ToolTier::Core);

impl super::ToolInvocation for Read {
    fn start_header(&self) -> super::HeaderFuture {
        super::HeaderFuture::Ready(super::HeaderResult::plain(Read::start_header(self)))
    }
    fn execute<'a>(self: Box<Self>, ctx: &'a super::ToolContext) -> super::ExecFuture<'a> {
        Box::pin(async move { Read::execute(&self, ctx).await.into() })
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;
    use test_case::test_case;

    use super::*;

    #[test_case(1,  0,  "/a/b.rs:1"     ; "limit_zero_shows_offset_only")]
    #[test_case(10, 0,  "/a/b.rs:10"    ; "offset_only_limit_zero")]
    #[test_case(1,  25, "/a/b.rs:1-25"  ; "offset_one_with_limit")]
    #[test_case(50, 51, "/a/b.rs:50-100" ; "offset_and_limit")]
    fn start_header_cases(offset: usize, limit: usize, expected: &str) {
        let r = Read {
            path: "/a/b.rs".into(),
            offset,
            limit,
        };
        assert_eq!(r.start_header(), expected);
    }

    #[test]
    fn list_dir_sorts_dirs_first_and_hides_instruction_files() {
        let dir = tempfile::TempDir::new().unwrap();
        let dir_path = dir.path().to_string_lossy().to_string();

        std::fs::write(dir.path().join("b.txt"), "").unwrap();
        std::fs::write(dir.path().join("a.rs"), "").unwrap();
        std::fs::create_dir(dir.path().join("zdir")).unwrap();
        std::fs::create_dir(dir.path().join("adir")).unwrap();
        std::fs::write(dir.path().join("AGENTS.md"), "").unwrap();

        let result =
            Read::list_dir(&dir_path, None, &crate::agent::LoadedInstructions::new()).unwrap();
        match &result {
            ToolOutput::ReadDir { text, instructions } => {
                let entries: Vec<&str> = text.lines().collect();
                assert_eq!(entries, vec!["adir/", "zdir/", "a.rs", "b.txt"]);
                assert!(instructions.is_none());
            }
            other => panic!("expected ReadDir, got {other:?}"),
        }
    }

    #[test]
    fn list_dir_discovers_subdirectory_instructions() {
        let root = tempfile::TempDir::new().unwrap();
        let sub = root.path().join("src");
        std::fs::create_dir_all(&sub).unwrap();
        std::fs::write(sub.join("AGENTS.md"), "sub rules").unwrap();
        std::fs::write(sub.join("lib.rs"), "").unwrap();

        let sub_path = sub.to_string_lossy().to_string();
        let loaded = crate::agent::LoadedInstructions::new();
        let result = Read::list_dir(&sub_path, Some(root.path()), &loaded).unwrap();
        match &result {
            ToolOutput::ReadDir { instructions, .. } => {
                let blocks = instructions.as_ref().expect("should have instructions");
                assert_eq!(blocks.len(), 1);
                assert!(blocks[0].path.ends_with("AGENTS.md"));
                assert_eq!(blocks[0].content, "sub rules");
            }
            other => panic!("expected ReadDir, got {other:?}"),
        }
    }

    const EXPECTED_INTEGER: &str = "expected integer";
    const MISSING_OFFSET_ERR: &str = "invalid parameter 'offset': required";
    const MISSING_LIMIT_ERR: &str = "invalid parameter 'limit': required";

    #[test]
    fn parse_input_bad_coercion_returns_error() {
        let err = Read::parse_input(&json!({"path": "x", "offset": 1, "limit": "not_a_number"}))
            .unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("limit"), "should mention field: {msg}");
        assert!(msg.contains(EXPECTED_INTEGER), "should mention type: {msg}");
    }

    #[test]
    fn missing_offset_fails_parse() {
        let err = Read::parse_input(&json!({"path": "/tmp/foo.txt", "limit": 10})).unwrap_err();
        assert!(err.to_string().contains(MISSING_OFFSET_ERR), "got: {err}");
    }

    #[test]
    fn missing_limit_fails_parse() {
        let err = Read::parse_input(&json!({"path": "/tmp/foo.txt", "offset": 1})).unwrap_err();
        assert!(err.to_string().contains(MISSING_LIMIT_ERR), "got: {err}");
    }

    #[test_case("test.png",  ImageMediaType::Png  ; "png")]
    #[test_case("test.jpg",  ImageMediaType::Jpeg ; "jpg")]
    #[test_case("test.jpeg", ImageMediaType::Jpeg ; "jpeg")]
    #[test_case("test.gif",  ImageMediaType::Gif  ; "gif")]
    #[test_case("test.webp", ImageMediaType::Webp ; "webp")]
    #[test_case("TEST.PNG",  ImageMediaType::Png  ; "uppercase_ext")]
    fn image_media_type_detects_extensions(name: &str, expected: ImageMediaType) {
        assert_eq!(image_media_type(Path::new(name)), Some(expected));
    }

    #[test_case("readme.md"   ; "non_image")]
    #[test_case("noext"       ; "no_extension")]
    fn image_media_type_none(name: &str) {
        assert_eq!(image_media_type(Path::new(name)), None);
    }

    #[test]
    fn read_image_returns_image_source() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("photo.png");
        std::fs::write(&path, b"fake png bytes").unwrap();
        let display = path.to_string_lossy().to_string();

        let output = Read::read_image(&display, &path, ImageMediaType::Png).unwrap();
        match output {
            ToolOutput::Image { caption, source } => {
                assert!(caption.contains("photo.png"), "caption: {caption}");
                assert_eq!(source.media_type, ImageMediaType::Png);
                assert!(
                    source.data.len() > b"fake png bytes".len(),
                    "should be base64"
                );
            }
            other => panic!("expected Image, got {other:?}"),
        }
    }

    #[test]
    fn read_image_rejects_oversized() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("big.png");
        std::fs::write(&path, vec![0u8; MAX_IMAGE_BYTES + 1]).unwrap();
        let err = Read::read_image("/big.png", &path, ImageMediaType::Png).unwrap_err();
        assert!(err.contains("exceeds"), "got: {err}");
    }
}
