//! Translate craft `TodoList` outputs to ACP plans.

use agent_client_protocol::schema::Plan;
use agent_client_protocol::schema::PlanEntry;
use agent_client_protocol::schema::PlanEntryPriority;
use agent_client_protocol::schema::PlanEntryStatus;
use craft_agent::TodoItem;
use craft_agent::TodoPriority;
use craft_agent::TodoStatus;

/// Build an ACP `Plan` from craft's todo list.
///
/// ACP plans are full replacements: each update sends the entire list with
/// the current status of every entry.
pub fn build_plan(items: &[TodoItem]) -> Plan {
    Plan::new(items.iter().map(entry_for).collect())
}

fn entry_for(item: &TodoItem) -> PlanEntry {
    PlanEntry::new(item.content.clone(), priority_for(item.priority), status_for(item.status))
}

fn status_for(status: TodoStatus) -> PlanEntryStatus {
    match status {
        TodoStatus::Pending | TodoStatus::Cancelled => PlanEntryStatus::Pending,
        TodoStatus::InProgress => PlanEntryStatus::InProgress,
        TodoStatus::Completed => PlanEntryStatus::Completed,
    }
}

fn priority_for(priority: TodoPriority) -> PlanEntryPriority {
    match priority {
        TodoPriority::High => PlanEntryPriority::High,
        TodoPriority::Medium => PlanEntryPriority::Medium,
        TodoPriority::Low => PlanEntryPriority::Low,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use test_case::test_case;

    #[test_case(TodoStatus::Pending, PlanEntryStatus::Pending ; "pending")]
    #[test_case(TodoStatus::InProgress, PlanEntryStatus::InProgress ; "in_progress")]
    #[test_case(TodoStatus::Completed, PlanEntryStatus::Completed ; "completed")]
    #[test_case(TodoStatus::Cancelled, PlanEntryStatus::Pending ; "cancelled_maps_to_pending")]
    fn status_mapping(input: TodoStatus, expected: PlanEntryStatus) {
        assert_eq!(status_for(input), expected);
    }

    #[test_case(TodoPriority::High, PlanEntryPriority::High ; "high")]
    #[test_case(TodoPriority::Medium, PlanEntryPriority::Medium ; "medium")]
    #[test_case(TodoPriority::Low, PlanEntryPriority::Low ; "low")]
    fn priority_mapping(input: TodoPriority, expected: PlanEntryPriority) {
        assert_eq!(priority_for(input), expected);
    }

    #[test]
    fn build_plan_includes_all_entries() {
        let items = vec![
            TodoItem {
                content: "first".into(),
                status: TodoStatus::InProgress,
                priority: TodoPriority::High,
            },
            TodoItem {
                content: "second".into(),
                status: TodoStatus::Pending,
                priority: TodoPriority::Low,
            },
        ];
        let plan = build_plan(&items);
        assert_eq!(plan.entries.len(), 2);
        assert_eq!(plan.entries[0].content, "first");
        assert_eq!(plan.entries[0].status, PlanEntryStatus::InProgress);
        assert_eq!(plan.entries[1].priority, PlanEntryPriority::Low);
    }
}
