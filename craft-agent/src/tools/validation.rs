use std::path::Path;

use super::outline::LangId;

pub(super) struct ValidationResult {
    pub syntax_valid: bool,
    pub introduced_errors: bool,
    pub error_count: usize,
}

pub(super) fn validate_edit(path: &Path, before: &str, after: &str) -> ValidationResult {
    let lang = LangId::from_path(path);
    let Some(lang) = lang else {
        return ValidationResult {
            syntax_valid: true,
            introduced_errors: false,
            error_count: 0,
        };
    };

    let mut parser = tree_sitter::Parser::new();
    if parser.set_language(&lang.ts_language()).is_err() {
        return ValidationResult {
            syntax_valid: true,
            introduced_errors: false,
            error_count: 0,
        };
    }

    let before_errors = count_errors(&mut parser, before);
    let after_errors = count_errors(&mut parser, after);

    let introduced_errors = after_errors > before_errors;

    ValidationResult {
        syntax_valid: after_errors == 0,
        introduced_errors,
        error_count: after_errors,
    }
}

fn count_errors(parser: &mut tree_sitter::Parser, source: &str) -> usize {
    let Some(tree) = parser.parse(source, None) else {
        return usize::MAX;
    };

    let root = tree.root_node();
    let mut count = 0;
    let mut cursor = root.walk();

    loop {
        let node = cursor.node();
        if node.is_error() || node.is_missing() {
            count += 1;
        }
        if !cursor.goto_first_child() {
            while !cursor.goto_next_sibling() {
                if !cursor.goto_parent() {
                    return count;
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn valid_rust_passes() {
        let result = validate_edit(
            Path::new("test.rs"),
            "",
            "fn main() { println!(\"hello\"); }",
        );
        assert!(result.syntax_valid);
        assert!(!result.introduced_errors);
    }

    #[test]
    fn broken_rust_detected() {
        let result = validate_edit(
            Path::new("test.rs"),
            "fn main() {}",
            "fn main() { println!(\"hello\"",
        );
        assert!(!result.syntax_valid);
        assert!(result.introduced_errors);
    }

    #[test]
    fn pre_existing_errors_not_flagged() {
        let result = validate_edit(
            Path::new("test.rs"),
            "fn main() { println!(\"hello\"",
            "fn main() { println!(\"hello\"",
        );
        assert!(!result.introduced_errors);
    }

    #[test]
    fn unknown_lang_passes() {
        let result = validate_edit(Path::new("test.txt"), "before", "after");
        assert!(result.syntax_valid);
        assert!(!result.introduced_errors);
    }
}
