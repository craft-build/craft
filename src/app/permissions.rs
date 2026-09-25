use agent_client_protocol::schema::v1::PermissionOptionKind;
use gpui::Context;

use crate::acp::PermissionDecision;

use super::App;

impl App {
    pub fn decide_permission(&mut self, allow: bool, cx: &mut Context<Self>) {
        let Some(permission) = self.pending_permission.take() else {
            return;
        };
        let option = permission.options.iter().find(|option| match option.kind {
            PermissionOptionKind::AllowOnce | PermissionOptionKind::AllowAlways => allow,
            PermissionOptionKind::RejectOnce | PermissionOptionKind::RejectAlways => !allow,
            _ => false,
        });
        let decision = option
            .map(|option| PermissionDecision::Select(option.option_id.to_string()))
            .unwrap_or(PermissionDecision::Cancel);
        permission.respond(decision);
        cx.notify();
    }
}
