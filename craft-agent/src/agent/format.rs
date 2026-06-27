use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use craft_config::FormatConfig;
use tokio::process::Command;
use tracing::{debug, warn};

pub(crate) const FORMAT_TOOL_NAME: &str = "format";

const RUSTFMT_ARGS: &[&str] = &[];
const PRETTIER_ARGS: &[&str] = &["--write"];
const BLACK_ARGS: &[&str] = &[];
const GOFMT_ARGS: &[&str] = &["-w"];
const SHFMT_ARGS: &[&str] = &["-w"];
const CLANG_FORMAT_ARGS: &[&str] = &["-i"];
const STYLUA_ARGS: &[&str] = &[];

const FORMAT_NO_OUTPUT_MSG: &str = "formatter exited with non-zero status";

fn formatter_for_extension(ext: &str) -> Option<(&'static str, &'static [&'static str])> {
    Some(match ext {
        "rs" => ("rustfmt", RUSTFMT_ARGS),
        "ts" | "tsx" | "js" | "jsx" | "mjs" | "cjs" | "json" | "css" | "scss" | "html" | "md"
        | "yml" | "yaml" => ("prettier", PRETTIER_ARGS),
        "py" => ("black", BLACK_ARGS),
        "go" => ("gofmt", GOFMT_ARGS),
        "sh" | "bash" => ("shfmt", SHFMT_ARGS),
        "c" | "h" | "cpp" | "cc" | "hpp" => ("clang-format", CLANG_FORMAT_ARGS),
        "lua" => ("stylua", STYLUA_ARGS),
        _ => return None,
    })
}

pub struct Formatter {
    workdir: PathBuf,
    config: FormatConfig,
}

impl Formatter {
    pub fn new(workdir: PathBuf, config: FormatConfig) -> Self {
        Self { workdir, config }
    }

    pub fn should_format(&self, path: &Path) -> bool {
        self.config.enabled
            && path
                .extension()
                .and_then(|e| e.to_str())
                .is_some_and(|ext| formatter_for_extension(ext).is_some())
    }

    pub async fn format(&self, path: &Path) -> FormatResult {
        let resolved = if path.is_absolute() {
            path.to_path_buf()
        } else {
            self.workdir.join(path)
        };

        let (bin, mut args): (String, Vec<String>) = if let Some(custom) = &self.config.command {
            let parts: Vec<&str> = custom.split_whitespace().collect();
            let Some(&first) = parts.first() else {
                warn!(command = %custom, "empty custom format command");
                return FormatResult::Skipped;
            };
            (
                first.to_string(),
                parts[1..].iter().map(|s| s.to_string()).collect(),
            )
        } else {
            let Some(ext) = path.extension().and_then(|e| e.to_str()) else {
                return FormatResult::Skipped;
            };
            let Some((bin, base)) = formatter_for_extension(ext) else {
                return FormatResult::Skipped;
            };
            (
                bin.to_string(),
                base.iter().map(|s| s.to_string()).collect(),
            )
        };

        args.push(resolved.to_string_lossy().into_owned());

        let original = std::fs::read(&resolved).ok();

        debug!(command = %bin, args = ?args, "running formatter");

        let timeout = Duration::from_secs(self.config.timeout_secs);
        let output = tokio::time::timeout(
            timeout,
            Command::new(&bin)
                .args(&args)
                .current_dir(&self.workdir)
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .output(),
        )
        .await;

        match output {
            Ok(Ok(out)) if out.status.success() => {
                let changed = original.is_some_and(|orig| {
                    std::fs::read(&resolved)
                        .map(|now| now != orig)
                        .unwrap_or(false)
                });
                if changed {
                    FormatResult::Reformatted
                } else {
                    FormatResult::Clean
                }
            }
            Ok(Ok(out)) => {
                let stderr = String::from_utf8_lossy(&out.stderr);
                let stdout = String::from_utf8_lossy(&out.stdout);
                let combined = if !stderr.is_empty() {
                    stderr.into_owned()
                } else if !stdout.is_empty() {
                    stdout.into_owned()
                } else {
                    FORMAT_NO_OUTPUT_MSG.to_string()
                };
                warn!(errors = %combined, "formatter failed");
                FormatResult::Errors(combined)
            }
            Ok(Err(e)) if e.kind() == std::io::ErrorKind::NotFound => {
                debug!(command = %bin, "formatter not found on PATH");
                FormatResult::Skipped
            }
            Ok(Err(e)) => {
                warn!(error = %e, "formatter command failed to execute");
                FormatResult::Errors(e.to_string())
            }
            Err(_) => {
                warn!(
                    timeout_secs = self.config.timeout_secs,
                    "formatter timed out"
                );
                FormatResult::Skipped
            }
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum FormatResult {
    Skipped,
    Clean,
    Reformatted,
    Errors(String),
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use test_case::test_case;

    fn tmp_dir() -> tempfile::TempDir {
        tempfile::tempdir().unwrap()
    }

    fn enabled_config() -> FormatConfig {
        FormatConfig {
            enabled: true,
            ..Default::default()
        }
    }

    fn rustfmt_available() -> bool {
        std::process::Command::new("rustfmt")
            .arg("--version")
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false)
    }

    #[test_case("foo.rs", true ; "rust")]
    #[test_case("foo.ts", true ; "typescript")]
    #[test_case("foo.tsx", true ; "tsx")]
    #[test_case("foo.js", true ; "js")]
    #[test_case("foo.json", true ; "json")]
    #[test_case("foo.py", true ; "python")]
    #[test_case("foo.go", true ; "go")]
    #[test_case("foo.sh", true ; "shell")]
    #[test_case("foo.bash", true ; "bash")]
    #[test_case("foo.c", true ; "c")]
    #[test_case("foo.hpp", true ; "hpp")]
    #[test_case("foo.lua", true ; "lua")]
    #[test_case("foo.txt", false ; "unknown_ext")]
    #[test_case("foo", false ; "no_extension")]
    #[test_case("foo.MD", false ; "case_sensitive")]
    fn should_format_by_extension(path: &str, expected: bool) {
        let dir = tmp_dir();
        let formatter = Formatter::new(dir.path().to_path_buf(), enabled_config());
        assert_eq!(formatter.should_format(Path::new(path)), expected);
    }

    #[test]
    fn should_not_format_when_disabled() {
        let dir = tmp_dir();
        let formatter = Formatter::new(dir.path().to_path_buf(), FormatConfig::default());
        assert!(!formatter.should_format(Path::new("foo.rs")));
    }

    #[tokio::test]
    async fn format_clean_when_unchanged() {
        let dir = tmp_dir();
        let path = dir.path().join("f.rs");
        fs::write(&path, "fn main() {}\n").unwrap();
        let formatter = Formatter::new(
            dir.path().to_path_buf(),
            FormatConfig {
                enabled: true,
                command: Some("true".into()),
                ..Default::default()
            },
        );
        assert_eq!(formatter.format(&path).await, FormatResult::Clean);
    }

    #[tokio::test]
    async fn format_skipped_when_binary_missing() {
        let dir = tmp_dir();
        let path = dir.path().join("f.rs");
        fs::write(&path, "fn main() {}\n").unwrap();
        let formatter = Formatter::new(
            dir.path().to_path_buf(),
            FormatConfig {
                enabled: true,
                command: Some("definitely_not_a_binary_xyz".into()),
                ..Default::default()
            },
        );
        assert_eq!(formatter.format(&path).await, FormatResult::Skipped);
    }

    #[tokio::test]
    async fn format_errors_on_nonzero_exit() {
        let dir = tmp_dir();
        let path = dir.path().join("f.rs");
        fs::write(&path, "fn main() {}\n").unwrap();
        let formatter = Formatter::new(
            dir.path().to_path_buf(),
            FormatConfig {
                enabled: true,
                command: Some("false".into()),
                ..Default::default()
            },
        );
        match formatter.format(&path).await {
            FormatResult::Errors(msg) => assert_eq!(msg, FORMAT_NO_OUTPUT_MSG),
            other => panic!("expected Errors, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn format_reformats_rust_with_rustfmt() {
        if !rustfmt_available() {
            eprintln!("skipping: rustfmt unavailable");
            return;
        }
        let dir = tmp_dir();
        let path = dir.path().join("bad.rs");
        fs::write(&path, "fn main ( ) { }").unwrap();
        let formatter = Formatter::new(dir.path().to_path_buf(), enabled_config());
        assert_eq!(formatter.format(&path).await, FormatResult::Reformatted);
        assert_ne!(fs::read_to_string(&path).unwrap(), "fn main ( ) { }");
    }

    #[tokio::test]
    async fn format_skips_unknown_extension_without_custom_command() {
        let dir = tmp_dir();
        let path = dir.path().join("f.txt");
        fs::write(&path, "x").unwrap();
        let formatter = Formatter::new(dir.path().to_path_buf(), enabled_config());
        assert_eq!(formatter.format(&path).await, FormatResult::Skipped);
    }

    #[tokio::test]
    async fn format_skips_when_custom_command_empty() {
        let dir = tmp_dir();
        let path = dir.path().join("f.rs");
        fs::write(&path, "x").unwrap();
        let formatter = Formatter::new(
            dir.path().to_path_buf(),
            FormatConfig {
                enabled: true,
                command: Some("   ".into()),
                ..Default::default()
            },
        );
        assert_eq!(formatter.format(&path).await, FormatResult::Skipped);
    }

    #[test]
    fn formatter_table_covers_all_documented_extensions() {
        for ext in [
            "rs", "ts", "tsx", "js", "jsx", "mjs", "cjs", "json", "css", "scss", "html", "md",
            "yml", "yaml", "py", "go", "sh", "bash", "c", "h", "cpp", "cc", "hpp", "lua",
        ] {
            assert!(formatter_for_extension(ext).is_some(), "missing {ext}");
        }
        assert!(formatter_for_extension("txt").is_none());
        assert!(formatter_for_extension("").is_none());
    }
}
