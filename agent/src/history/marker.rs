//! Synthetic-turn markers: cancel, end, failure, grace and nudge messages
//! the harness appends to history carry their identity in a
//! [`StoredMessage`] tag instead of their text, so detection never depends
//! on string equality, and a user who types a marker's text verbatim is
//! not misclassified.

use jiff::Timestamp;

use super::{ContentBlock, Message, MessageKind, Role, StoredMessage};

/// The tag key every marker carries.
pub const TAG: &str = "craft.marker";

pub const CANCEL: &str = "cancel";
pub const END: &str = "end";
pub const FAILED: &str = "failed";
pub const GRACE: &str = "grace";
pub const NUDGE: &str = "nudge";
pub const CONTINUATION: &str = "continuation";

/// A marker observation: user role (the model must see it), `Observation`
/// kind (the user did not author it), tagged with its marker identity.
pub fn observation(marker: &str, text: impl Into<String>) -> StoredMessage {
    let message = Message {
        role: Role::User,
        content: vec![ContentBlock::Text { text: text.into() }],
        display_text: None,
        kind: MessageKind::Observation,
    };
    let mut stored = StoredMessage::builder()
        .timestamp(Timestamp::now())
        .message(message)
        .build();
    stored.tags_mut().insert(TAG.to_owned(), marker.to_owned());
    stored
}

/// The message's marker identity, if it is a marker.
pub fn of(stored: &StoredMessage) -> Option<&str> {
    stored.tags().get(TAG).map(String::as_str)
}

/// Whether the message is the given marker.
pub fn is(stored: &StoredMessage, marker: &str) -> bool {
    of(stored) == Some(marker)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::history::ext;

    #[test]
    fn a_untagged_user_message_with_marker_text_is_not_a_marker() {
        let verbatim = ext::stored_user_text("[Cancelled by user]");
        assert_eq!(of(&verbatim), None);
        assert!(!is(&verbatim, CANCEL));

        let real = observation(CANCEL, "[Cancelled by user]");
        assert_eq!(of(&real), Some(CANCEL));
        assert_eq!(ext::message_text(&real), "[Cancelled by user]");
    }

    #[test]
    fn markers_survive_serialization() {
        let marker = observation(GRACE, "please summarize");
        let json = serde_json::to_string(&marker).unwrap();
        let back: StoredMessage = serde_json::from_str(&json).unwrap();
        assert_eq!(of(&back), Some(GRACE));
    }
}
