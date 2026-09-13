use std::sync::{Arc, Mutex};

use rig_core::tool::{IntoToolOutput, ToolOutput};
use schemars::JsonSchema;
use serde::Deserialize;

use super::{Result, Workspace, impl_tool, invalid};

/// Per-workspace todo store, replaced wholesale by each call.
pub(crate) type TodoStore = Arc<Mutex<Vec<Todo>>>;

#[derive(Debug, Clone, PartialEq, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Todo {
    /// Hierarchical task id, e.g. T1 or T1.1.
    pub id: String,
    /// Parent task id; omit or leave empty for top-level tasks.
    pub parent: Option<String>,
    pub content: String,
    /// One of: pending, in_progress, completed, cancelled.
    pub status: String,
    /// Subagent name owning this task (optional).
    pub owner: Option<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct TodoWriteArgs {
    /// Full replacement list of tasks. An empty array clears all todos.
    pub todos: Vec<Todo>,
}

#[derive(Debug)]
pub struct TodoWriteOutput {
    pub text: String,
}

impl IntoToolOutput for TodoWriteOutput {
    fn into_tool_output(self) -> Result<ToolOutput> {
        Ok(ToolOutput::text(self.text))
    }
}

#[derive(Clone)]
pub struct TodoWrite(pub Workspace);

impl TodoWrite {
    fn execute(workspace: &Workspace, args: TodoWriteArgs) -> Result<TodoWriteOutput> {
        for todo in &args.todos {
            if !matches!(
                todo.status.as_str(),
                "pending" | "in_progress" | "completed" | "cancelled"
            ) {
                return Err(invalid(format!(
                    "status must be pending, in_progress, completed, or cancelled (got {:?} on {})",
                    todo.status, todo.id
                )));
            }
        }
        let mut store = workspace
            .todos
            .lock()
            .map_err(|_| super::failure("todo store was poisoned"))?;
        if args.todos.is_empty() {
            store.clear();
            return Ok(TodoWriteOutput {
                text: "Todos cleared".into(),
            });
        }
        *store = args.todos.clone();
        Ok(TodoWriteOutput {
            text: render_todos(&args.todos),
        })
    }
}

impl_tool!(
    TodoWrite,
    TodoWriteArgs,
    TodoWriteOutput,
    "todo_write",
    "Track and update progress on multi-step tasks (3+ steps). Update after EACH completed step, not only all at once. Each task needs an id (e.g. T1, T1.1), content, and status; parent-child nesting is supported via the parent field. Sending the full list replaces the previous state; an empty array clears it."
);

const STATUS_MARKERS: [(&str, &str); 4] = [
    ("pending", "[ ]"),
    ("in_progress", "[•]"),
    ("completed", "[✓]"),
    ("cancelled", "[x]"),
];

fn marker(status: &str) -> &'static str {
    STATUS_MARKERS
        .iter()
        .find(|(name, _)| *name == status)
        .map(|(_, marker)| *marker)
        .unwrap_or("[ ]")
}

/// Flatten the todo list parent-first, depth-indented. Tasks whose parent is
/// missing or empty sit at depth 0; unvisited leftovers (cycles, shared
/// parents) are appended at the end.
fn flatten_todos(todos: &[Todo]) -> Vec<(&Todo, usize)> {
    let ids: Vec<&str> = todos.iter().map(|todo| todo.id.as_str()).collect();
    let mut visited = vec![false; todos.len()];
    let mut out = Vec::new();

    fn visit<'a>(
        todos: &'a [Todo],
        ids: &[&str],
        visited: &mut Vec<bool>,
        out: &mut Vec<(&'a Todo, usize)>,
        parent: Option<&str>,
        depth: usize,
    ) {
        for (index, todo) in todos.iter().enumerate() {
            if visited[index] {
                continue;
            }
            let mine = match parent {
                None => todo
                    .parent
                    .as_deref()
                    .is_none_or(|p| p.is_empty() || !ids.contains(&p)),
                Some(parent) => todo.parent.as_deref() == Some(parent),
            };
            if mine {
                visited[index] = true;
                out.push((todo, depth));
                if !todo.id.is_empty() {
                    visit(todos, ids, visited, out, Some(&todo.id), depth + 1);
                }
            }
        }
    }

    visit(todos, &ids, &mut visited, &mut out, None, 0);
    for (index, todo) in todos.iter().enumerate() {
        if !visited[index] {
            out.push((todo, 0));
        }
    }
    out
}

fn render_todos(todos: &[Todo]) -> String {
    flatten_todos(todos)
        .into_iter()
        .map(|(todo, depth)| {
            let indent = "  ".repeat(depth);
            let id = if todo.id.is_empty() {
                String::new()
            } else {
                format!("{} ", todo.id)
            };
            let owner = todo
                .owner
                .as_deref()
                .filter(|owner| !owner.is_empty())
                .map(|owner| format!(" (@{owner})"))
                .unwrap_or_default();
            format!(
                "{indent}{id}{} {}{owner}",
                marker(&todo.status),
                todo.content
            )
        })
        .collect::<Vec<_>>()
        .join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn todo(id: &str, parent: Option<&str>, status: &str) -> Todo {
        Todo {
            id: id.into(),
            parent: parent.map(str::to_string),
            content: format!("content {id}"),
            status: status.into(),
            owner: None,
        }
    }

    #[test]
    fn renders_tree_with_markers_and_owners() {
        let mut parent = todo("T1", None, "completed");
        parent.owner = Some("scout".into());
        let todos = vec![
            parent,
            todo("T1.1", Some("T1"), "in_progress"),
            todo("T2", None, "pending"),
        ];
        assert_eq!(
            render_todos(&todos),
            "T1 [✓] content T1 (@scout)\n  T1.1 [•] content T1.1\nT2 [ ] content T2"
        );
    }

    #[test]
    fn unknown_parent_and_cycles_fall_back_to_root() {
        let todos = vec![
            todo("A", Some("missing"), "pending"),
            todo("B", Some("A"), "pending"),
        ];
        assert_eq!(render_todos(&todos), "A [ ] content A\n  B [ ] content B");
    }
}
