use crate::{TaskNode, ToolOutput};
use craft_tool_macro::Tool;
use serde::Deserialize;

#[derive(Tool, Debug, Clone, Deserialize)]
pub struct TodoWrite {
    #[param(description = "The updated task list (replace-all). Use hierarchical ids (T1, T1.1).")]
    todos: Vec<TaskNode>,
}

impl TodoWrite {
    pub const NAME: &str = "todo_write";
    pub const DESCRIPTION: &str = include_str!("todowrite.md");
    pub const EXAMPLES: Option<&str> = Some(
        r#"[{"todos": [{"id": "T1", "content": "Add error handling", "status": "pending"}, {"id": "T1.1", "parent": "T1", "content": "Wrap parse call", "status": "in_progress"}]}]"#,
    );

    pub async fn execute(&self, _ctx: &super::ToolContext) -> Result<ToolOutput, String> {
        for task in &self.todos {
            if !TaskNode::is_valid_id(&task.id) {
                return Err(format!(
                    "invalid task id '{}': use hierarchical ids like T1, T1.1, T1.1.2",
                    task.id
                ));
            }
        }
        Ok(ToolOutput::TodoList(self.todos.clone()))
    }
}

super::impl_tool!(
    TodoWrite,
    audience = super::ToolAudience::MAIN
        | super::ToolAudience::RESEARCH_SUB
        | super::ToolAudience::GENERAL_SUB,
    kind = "think",
    tier = super::ToolTier::Core,
);

impl super::ToolInvocation for TodoWrite {
    fn start_header(&self) -> super::HeaderFuture {
        super::HeaderFuture::Ready(super::HeaderResult::plain(format!(
            "{} todos",
            self.todos.len()
        )))
    }
    fn execute<'a>(self: Box<Self>, ctx: &'a super::ToolContext) -> super::ExecFuture<'a> {
        Box::pin(async move { TodoWrite::execute(&self, ctx).await.into() })
    }
}
