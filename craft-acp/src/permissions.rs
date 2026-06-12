//! Translate craft `PermissionRequest` events to ACP `session/request_permission`.
//!
//! Phase 3 maps craft's `PermissionAnswer` set to ACP `PermissionOption`s and
//! decodes the user's `SelectedPermissionOutcome` back into the encoded answer
//! the `PermissionManager` is waiting for on its response channel.

use agent_client_protocol::schema::PermissionOption;
use agent_client_protocol::schema::PermissionOptionId;
use agent_client_protocol::schema::PermissionOptionKind;
use agent_client_protocol::schema::RequestPermissionOutcome;
use agent_client_protocol::schema::ToolCallId;
use agent_client_protocol::schema::ToolCallUpdate;
use agent_client_protocol::schema::ToolCallUpdateFields;
use craft_agent::permissions::PermissionAnswer;

const ALLOW_ONCE: &str = "allow_once";
const ALLOW_SESSION: &str = "allow_session";
const ALLOW_ALWAYS: &str = "allow_always";
const REJECT_ONCE: &str = "reject_once";
const REJECT_ALWAYS: &str = "reject_always";

/// Standard permission options offered to ACP clients.
///
/// The four ACP-spec kinds map to a sensible subset of craft's answers:
/// `AllowAlways` → `AllowAlwaysGlobal`, `RejectAlways` → `DenyAlwaysGlobal`.
/// Session-scoped allow is exposed as a fifth `Other` option so power users
/// keep parity with the TUI.
pub fn options() -> Vec<PermissionOption> {
    vec![
        PermissionOption::new(
            PermissionOptionId::new(ALLOW_ONCE),
            "Allow once",
            PermissionOptionKind::AllowOnce,
        ),
        PermissionOption::new(
            PermissionOptionId::new(ALLOW_SESSION),
            "Allow for this session",
            PermissionOptionKind::AllowOnce,
        ),
        PermissionOption::new(
            PermissionOptionId::new(ALLOW_ALWAYS),
            "Allow always",
            PermissionOptionKind::AllowAlways,
        ),
        PermissionOption::new(
            PermissionOptionId::new(REJECT_ONCE),
            "Reject",
            PermissionOptionKind::RejectOnce,
        ),
        PermissionOption::new(
            PermissionOptionId::new(REJECT_ALWAYS),
            "Reject always",
            PermissionOptionKind::RejectAlways,
        ),
    ]
}

/// Build the `ToolCallUpdate` that accompanies a permission request, scoped
/// to the tool invocation that triggered the prompt.
pub fn tool_call_update(tool_call_id: &str, tool: &str, scopes: &[String]) -> ToolCallUpdate {
    let title = if scopes.is_empty() {
        tool.to_owned()
    } else {
        format!("{tool}: {}", scopes.join("; "))
    };
    ToolCallUpdate::new(
        ToolCallId::new(tool_call_id.to_owned()),
        ToolCallUpdateFields::new().title(Some(title)),
    )
}

/// Decode an ACP `RequestPermissionOutcome` into the wire-format string the
/// `PermissionManager` decodes via `PermissionAnswer::decode`. Cancellation
/// or unknown options map to a plain deny.
pub fn outcome_to_answer(outcome: &RequestPermissionOutcome) -> String {
    match outcome {
        RequestPermissionOutcome::Cancelled => PermissionAnswer::Deny.encode(),
        RequestPermissionOutcome::Selected(selected) => {
            answer_for_option_id(&selected.option_id.0).encode()
        }
        _ => PermissionAnswer::Deny.encode(),
    }
}

fn answer_for_option_id(id: &str) -> PermissionAnswer {
    match id {
        ALLOW_ONCE => PermissionAnswer::AllowOnce,
        ALLOW_SESSION => PermissionAnswer::AllowSession,
        ALLOW_ALWAYS => PermissionAnswer::AllowAlwaysGlobal,
        REJECT_ALWAYS => PermissionAnswer::DenyAlwaysGlobal,
        _ => PermissionAnswer::Deny,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent_client_protocol::schema::SelectedPermissionOutcome;
    use test_case::test_case;

    #[test]
    fn options_cover_all_four_acp_kinds() {
        let opts = options();
        let kinds: Vec<&PermissionOptionKind> = opts.iter().map(|o| &o.kind).collect();
        assert!(kinds.contains(&&PermissionOptionKind::AllowOnce));
        assert!(kinds.contains(&&PermissionOptionKind::AllowAlways));
        assert!(kinds.contains(&&PermissionOptionKind::RejectOnce));
        assert!(kinds.contains(&&PermissionOptionKind::RejectAlways));
    }

    #[test_case(ALLOW_ONCE, PermissionAnswer::AllowOnce ; "allow_once")]
    #[test_case(ALLOW_SESSION, PermissionAnswer::AllowSession ; "allow_session")]
    #[test_case(ALLOW_ALWAYS, PermissionAnswer::AllowAlwaysGlobal ; "allow_always_to_global")]
    #[test_case(REJECT_ONCE, PermissionAnswer::Deny ; "reject_once")]
    #[test_case(REJECT_ALWAYS, PermissionAnswer::DenyAlwaysGlobal ; "reject_always_to_global")]
    #[test_case("unknown_id", PermissionAnswer::Deny ; "unknown_id_denies")]
    fn option_id_mapping(id: &str, expected: PermissionAnswer) {
        assert_eq!(answer_for_option_id(id), expected);
    }

    #[test]
    fn cancelled_outcome_encodes_as_deny() {
        let encoded = outcome_to_answer(&RequestPermissionOutcome::Cancelled);
        assert_eq!(encoded, PermissionAnswer::Deny.encode());
    }

    #[test]
    fn selected_outcome_encodes_chosen_answer() {
        let outcome = RequestPermissionOutcome::Selected(SelectedPermissionOutcome::new(
            PermissionOptionId::new(ALLOW_SESSION),
        ));
        assert_eq!(outcome_to_answer(&outcome), PermissionAnswer::AllowSession.encode());
    }

    #[test]
    fn tool_call_update_carries_title() {
        let update = tool_call_update("tu_1", "bash", &["echo hi".into()]);
        assert_eq!(&*update.tool_call_id.0, "tu_1");
        assert_eq!(update.fields.title.as_deref(), Some("bash: echo hi"));
    }
}
