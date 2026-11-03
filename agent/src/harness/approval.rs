//! The approval seam: the narrow interaction trait surfaces implement
//! so `harness::PolicyLayer` can ask a human or a client about a tool call.
//!
//! The policy itself — permission rules, auto-review, the plan-mode gate —
//! lives in the policy layer, so every surface gets identical decisions.
//! A responder only conducts the interaction: park on the TUI overlay, send
//! an ACP `session/request_permission`, or fail closed headlessly.

use crate::permissions::PermissionAnswer;

use super::BoxFuture;

/// One gated call the policy layer wants a decision on. Everything the
/// interaction needs to display (and nothing about how the answer is
/// applied, which is the layer's job).
#[derive(Debug)]
pub struct ApprovalAsk<'a> {
    /// The call's id, so the surface can pair the ask with its tool card.
    pub id: &'a str,
    /// The call's wire name (`server__tool` for MCP tools).
    pub name: &'a str,
    /// The call's decoded input, for display.
    pub input: &'a serde_json::Value,
    /// The scopes the rules could not decide, already resolved against the
    /// project root.
    pub scopes: &'a [String],
    /// The call carries a server-declared read-only hint and no destructive
    /// hint: the interaction may phrase the ask in a low-risk tone.
    pub low_risk: bool,
}

/// What the interaction produced: allow, deny or stop, plus the "remember"
/// scope riding the answer. The layer records `remember` exactly as the
/// interactive gates did (session rules, `permissions.bml` for "always").
#[derive(Debug)]
pub enum Approval {
    /// Run the call.
    Allow {
        /// Record this answer as rules before running.
        remember: Option<PermissionAnswer>,
    },
    /// Do not run the call; `message` is model-visible. Unlike a hard
    /// denial it is reported as a normal (non-error) result, the shape a
    /// permission rejection always had.
    Deny {
        message: String,
        /// Record this answer as rules before reporting the denial.
        remember: Option<PermissionAnswer>,
    },
    /// The interaction was cut short (the turn was cancelled); the run
    /// stops with `reason`.
    Stop { reason: String },
}

/// The surface's half of the approval interaction. Implementations must be
/// safe to call concurrently (parallel tool waves): interactive responders
/// serialize their modal on an async mutex inside the implementation.
pub trait ApprovalResponder: Send + Sync {
    fn ask(&self, ask: ApprovalAsk<'_>) -> BoxFuture<Approval>;
}
