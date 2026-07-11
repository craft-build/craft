//! Bridge between the Python interpreter and the tool dispatch layer.
//!
//! [`flatten`] is the single place a [`ToolDoneEvent`] becomes the text a
//! Python tool call resolves to. Both the sync and async interpreter resolvers
//! go through it, so the two can never drift apart (they used to: the sync
//! path dropped the image-not-visible note the async path added).

use crate::ToolDoneEvent;

use super::code_execution::IMAGE_NOT_VISIBLE_NOTE;

/// The one place a [`ToolDoneEvent`] becomes text a caller reads.
/// An error becomes `Err(text)`; an image success gets the not-visible note
/// appended (its pixels are dropped here); anything else is plain text.
pub fn flatten(done: &ToolDoneEvent) -> Result<String, String> {
    let text = match &done.output {
        crate::ToolOutput::Image { caption, .. } if !done.is_error => {
            format!("{caption} ({IMAGE_NOT_VISIBLE_NOTE})")
        }
        out => out.as_text(),
    };
    if done.is_error { Err(text) } else { Ok(text) }
}
