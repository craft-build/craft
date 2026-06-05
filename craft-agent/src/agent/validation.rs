use std::path::Path;
use std::process::Stdio;

use craft_config::ValidationConfig;
use tokio::process::Command;
use tracing::{debug, warn};

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum ProjectType {
    Rust,
    TypeScript,
    Go,
    Python,
}

impl ProjectType {
    pub fn detect(workdir: &Path) -> Option<Self> {
        if workdir.join("Cargo.toml").exists() {
            return Some(Self::Rust);
        }
        if workdir.join("tsconfig.json").exists() {
            return Some(Self::TypeScript);
        }
        if workdir.join("go.mod").exists() {
            return Some(Self::Go);
        }
        if workdir.join("pyproject.toml").exists() || workdir.join("setup.py").exists() {
            return Some(Self::Python);
        }
        None
    }

    fn validation_command(&self) -> (&str, &[&str]) {
        match self {
            Self::Rust => ("cargo", &["check", "--message-format=short"]),
            Self::TypeScript => ("npx", &["tsc", "--noEmit"]),
            Self::Go => ("go", &["build", "./..."]),
            Self::Python => ("python", &["-m", "compileall", "-q", "."]),
        }
    }

    fn relevant_extension(&self) -> &str {
        match self {
            Self::Rust => "rs",
            Self::TypeScript => "ts",
            Self::Go => "go",
            Self::Python => "py",
        }
    }
}

pub struct Validator {
    project_type: Option<ProjectType>,
    config: ValidationConfig,
    workdir: std::path::PathBuf,
}

impl Validator {
    pub fn new(workdir: std::path::PathBuf, config: ValidationConfig) -> Self {
        let project_type = ProjectType::detect(&workdir);
        if let Some(pt) = &project_type {
            debug!(project_type = ?pt, "detected project type for validation");
        }
        Self {
            project_type,
            config,
            workdir,
        }
    }

    pub fn should_validate(&self, edited_path: &Path) -> bool {
        if !self.config.enabled {
            return false;
        }
        let Some(pt) = &self.project_type else {
            return false;
        };
        edited_path
            .extension()
            .is_some_and(|ext| ext == pt.relevant_extension())
    }

    pub async fn validate(&self) -> ValidationResult {
        let Some(pt) = &self.project_type else {
            return ValidationResult::Skipped;
        };

        let (cmd, args) = if let Some(ref custom) = self.config.command {
            let parts: Vec<&str> = custom.split_whitespace().collect();
            (parts[0], parts[1..].to_vec())
        } else {
            let (c, a) = pt.validation_command();
            (c, a.to_vec())
        };

        debug!(command = cmd, args = ?args, "running validation");

        let output = Command::new(cmd)
            .args(&args)
            .current_dir(&self.workdir)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .output()
            .await;

        match output {
            Ok(out) if out.status.success() => ValidationResult::Clean,
            Ok(out) => {
                let stderr = String::from_utf8_lossy(&out.stderr);
                let stdout = String::from_utf8_lossy(&out.stdout);
                let combined = if stderr.is_empty() {
                    stdout.into_owned()
                } else if stdout.is_empty() {
                    stderr.into_owned()
                } else {
                    format!("{stdout}\n{stderr}")
                };
                debug!(errors = %combined, "validation failed");
                ValidationResult::Errors(combined)
            }
            Err(e) => {
                warn!(error = %e, "validation command failed to execute");
                ValidationResult::Skipped
            }
        }
    }
}

#[derive(Debug)]
pub enum ValidationResult {
    Clean,
    Errors(String),
    Skipped,
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use test_case::test_case;

    fn tmp_dir() -> tempfile::TempDir {
        tempfile::tempdir().unwrap()
    }

    #[test]
    fn detect_rust_project() {
        let dir = tmp_dir();
        fs::write(dir.path().join("Cargo.toml"), "").unwrap();
        assert_eq!(ProjectType::detect(dir.path()), Some(ProjectType::Rust));
    }

    #[test]
    fn detect_typescript_project() {
        let dir = tmp_dir();
        fs::write(dir.path().join("tsconfig.json"), "").unwrap();
        assert_eq!(
            ProjectType::detect(dir.path()),
            Some(ProjectType::TypeScript)
        );
    }

    #[test]
    fn detect_go_project() {
        let dir = tmp_dir();
        fs::write(dir.path().join("go.mod"), "").unwrap();
        assert_eq!(ProjectType::detect(dir.path()), Some(ProjectType::Go));
    }

    #[test]
    fn detect_no_project() {
        let dir = tmp_dir();
        assert_eq!(ProjectType::detect(dir.path()), None);
    }

    #[test]
    fn should_validate_matching_extension() {
        let dir = tmp_dir();
        fs::write(dir.path().join("Cargo.toml"), "").unwrap();
        let validator = Validator::new(
            dir.path().to_owned(),
            ValidationConfig {
                enabled: true,
                ..Default::default()
            },
        );
        assert!(validator.should_validate(Path::new("src/main.rs")));
        assert!(!validator.should_validate(Path::new("src/main.ts")));
    }

    #[test]
    fn should_not_validate_when_disabled() {
        let dir = tmp_dir();
        fs::write(dir.path().join("Cargo.toml"), "").unwrap();
        let validator = Validator::new(dir.path().to_owned(), ValidationConfig::default());
        assert!(!validator.should_validate(Path::new("src/main.rs")));
    }

    #[test_case("foo.rs", true ; "rust_file")]
    #[test_case("foo.ts", false ; "ts_file")]
    fn relevant_extension_rust(path: &str, expected: bool) {
        let dir = tmp_dir();
        fs::write(dir.path().join("Cargo.toml"), "").unwrap();
        let validator = Validator::new(
            dir.path().to_owned(),
            ValidationConfig {
                enabled: true,
                ..Default::default()
            },
        );
        assert_eq!(validator.should_validate(Path::new(path)), expected);
    }
}
