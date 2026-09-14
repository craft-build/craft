//! Compound-command permission scopes for the bash tool.
//!
//! A shell string like `git diff && rm -rf /` is one tool call but two
//! commands with very different risk. Splitting it with a tree-sitter bash
//! parse lets the permission engine judge each command on its own instead of
//! trusting the whole string. Anything the parser cannot confidently split —
//! substitutions, subshells, arithmetic, syntax errors — comes back as the
//! raw command with `force_prompt`, so allow rules cannot quietly cover it.

use tree_sitter::{Node, Parser};

/// The scopes a bash call is about, plus whether allow rules may be trusted
/// for them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BashScopes {
    pub scopes: Vec<String>,
    /// `true` when the command could not be split confidently: the caller
    /// must prompt even if a rule would otherwise allow the scope.
    pub force_prompt: bool,
}

/// Node types that hide what actually runs. A `$(...)` can execute anything,
/// so the enclosing command is not what it looks like.
const COMPLEX_TYPES: [&str; 4] = [
    "command_substitution",
    "process_substitution",
    "subshell",
    "arithmetic_expansion",
];

/// Redirects are collected instead of becoming their own scope; they ride
/// along with the command bash would apply them to.
const REDIRECT_TYPES: [&str; 3] = ["file_redirect", "heredoc_redirect", "herestring_redirect"];

/// Nodes we walk through instead of turning into a scope. `redirected_statement`
/// has to be one of them: tree-sitter hangs a trailing `2>&1` off the entire
/// `cd x && cargo test` chain rather than off `cargo test`, so treating it as
/// a leaf turns the whole chain into a single scope starting with `cd `, and a
/// `cd *` allow rule then quietly covers whatever runs after the `&&`.
const WALK_THROUGH_TYPES: [&str; 4] = ["program", "list", "pipeline", "redirected_statement"];

fn is_complex(node: Node) -> bool {
    COMPLEX_TYPES.contains(&node.kind())
        || (0..node.child_count() as u32).any(|i| node.child(i).is_some_and(is_complex))
}

fn node_text<'a>(node: Node, source: &'a str) -> &'a str {
    &source[node.byte_range()].trim()
}

/// Anything we don't walk through becomes one scope, its own text. That covers
/// plain commands and the block forms (`if`, `while`, subshells) we keep
/// whole, plus any node type we never thought of: an unknown node has to end
/// up in front of the user, not get dropped.
fn collect_commands(node: Node, source: &str, out: &mut Vec<String>) {
    if !WALK_THROUGH_TYPES.contains(&node.kind()) {
        let text = node_text(node, source);
        if !text.is_empty() {
            out.push(text.to_string());
        }
        return;
    }

    let level_start = out.len();
    let mut redirects: Vec<&str> = Vec::new();
    for i in 0..node.child_count() as u32 {
        let child = node.child(i).expect("index < child_count");
        if !child.is_named() || child.kind() == "comment" {
            continue;
        }
        if REDIRECT_TYPES.contains(&child.kind()) {
            redirects.push(node_text(child, source));
        } else {
            collect_commands(child, source, out);
        }
    }

    // The redirect belongs to the last command of the chain, the one bash
    // would actually apply it to. A bodiless `> log` has no such command and
    // still truncates the file, so it becomes a scope of its own instead of
    // vanishing.
    if !redirects.is_empty() {
        let text = redirects.join(" ");
        if out.len() > level_start {
            let last = out.last_mut().expect("len checked above");
            last.push(' ');
            last.push_str(&text);
        } else {
            out.push(text);
        }
    }
}

/// Split a bash command into per-command permission scopes.
///
/// Returns `None` for a missing/blank command — the caller's input validation
/// owns that failure. Unsplittable commands come back as one scope with
/// `force_prompt: true`.
pub fn permission_scopes(command: &str) -> Option<BashScopes> {
    if command.trim().is_empty() {
        return None;
    }

    let mut parser = Parser::new();
    parser
        .set_language(&tree_sitter_bash::LANGUAGE.into())
        .expect("tree-sitter-bash grammar matches the tree-sitter runtime");
    let tree = parser.parse(command, None)?;
    let root = tree.root_node();
    if root.has_error() || is_complex(root) {
        return Some(BashScopes {
            scopes: vec![command.to_string()],
            force_prompt: true,
        });
    }

    let mut scopes = Vec::new();
    collect_commands(root, command, &mut scopes);
    if scopes.is_empty() {
        scopes.push(command.to_string());
    }
    Some(BashScopes {
        scopes,
        force_prompt: false,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scopes(cmd: &str) -> BashScopes {
        permission_scopes(cmd).expect("non-empty command yields scopes")
    }

    #[test]
    fn blank_command_is_none() {
        assert!(permission_scopes("").is_none());
        assert!(permission_scopes("   \n\t ").is_none());
    }

    #[test]
    fn chained_commands_split_into_scopes() {
        let s = scopes("git diff && rm -rf /");
        assert_eq!(s.scopes, vec!["git diff", "rm -rf /"]);
        assert!(!s.force_prompt);
    }

    #[test]
    fn semicolon_lists_and_pipelines_split() {
        assert_eq!(
            scopes("cargo build; cargo test").scopes,
            vec!["cargo build", "cargo test"]
        );
        assert_eq!(
            scopes("cat foo.txt | grep bar | wc -l").scopes,
            vec!["cat foo.txt", "grep bar", "wc -l"]
        );
    }

    #[test]
    fn trailing_redirect_attaches_to_last_command() {
        // A `cd *` allow rule must not quietly cover `cargo test`.
        assert_eq!(
            scopes("cd agent && cargo test 2>&1").scopes,
            vec!["cd agent", "cargo test 2>&1"]
        );
    }

    #[test]
    fn bodiless_redirect_is_its_own_scope() {
        assert_eq!(scopes("> log").scopes, vec!["> log"]);
    }

    #[test]
    fn block_forms_stay_whole() {
        let if_cmd = "if [ -f x ]; then rm x; fi";
        assert_eq!(scopes(if_cmd).scopes, vec![if_cmd]);
        let loop_cmd = "for f in *.tmp; do rm $f; done";
        assert_eq!(scopes(loop_cmd).scopes, vec![loop_cmd]);
    }

    #[test]
    fn substitution_and_subshell_force_prompt() {
        for cmd in [
            "echo $(whoami)",
            "diff <(ls) <(ls -a)",
            "(cd / && rm x)",
            "echo $((1+2))",
        ] {
            let s = scopes(cmd);
            assert_eq!(s.scopes, vec![cmd.to_string()], "{cmd}");
            assert!(s.force_prompt, "{cmd}");
        }
    }

    #[test]
    fn parse_error_forces_prompt_on_raw_command() {
        let s = scopes("if [ x");
        assert_eq!(s.scopes, vec!["if [ x"]);
        assert!(s.force_prompt);
    }

    #[test]
    fn comment_only_command_falls_back_to_raw() {
        let s = scopes("# just a note");
        assert_eq!(s.scopes, vec!["# just a note"]);
        assert!(!s.force_prompt);
    }

    #[test]
    fn comments_do_not_leak_into_neighbor_scopes() {
        let s = scopes("cargo build # build it && rm -rf /");
        assert_eq!(s.scopes, vec!["cargo build"]);
    }
}
