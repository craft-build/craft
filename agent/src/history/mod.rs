//! The crate's conversation history vocabulary: animist's history types,
//! re-exported, plus the craft-side helpers in [`ext`] and the synthetic
//! marker tags in [`marker`].
//!
//! The run loop, the surfaces and the session store speak
//! [`StoredMessage`] (a [`Message`] plus storage metadata), so craft-local
//! tags — marker identity above all — live on the stored wrapper and survive
//! persistence. Provider calls take plain [`Message`] slices, mapped at the
//! call site through the request-view functions in [`crate::run::view`].

pub mod ext;
pub mod marker;

pub use animist::history::{
    ContentBlock, HistoryEntry, ImageSource, Message, MessageKind, Role, StoredMessage,
    ToolContent, ToolContents,
};
