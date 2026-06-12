//! Convert ACP `ContentBlock`s into the inputs needed by `Agent::run`.
//!
//! ACP supports `Text`, `Image`, `Audio`, `ResourceLink`, and `Resource`
//! (`EmbeddedResource` containing either `TextResourceContents` or
//! `BlobResourceContents`). Craft's prompt model is text + a list of
//! attached images, so we lower:
//!
//! - `Text` → appended to the message text.
//! - `Image` → decoded into a `craft_providers::ImageSource` (only the four
//!   media types Craft renders: png, jpeg, gif, webp). Other mime types are
//!   inlined as a text reference.
//! - `ResourceLink` → inlined as a "[resource: name (uri)]" line so the
//!   model can see what the user pointed at.
//! - `Resource(EmbeddedResource)` with `TextResourceContents` → inlined as
//!   a fenced code block tagged with the URI.
//! - `Resource(EmbeddedResource)` with `BlobResourceContents` → inlined as
//!   a "[binary resource: uri (mime, N bytes)]" reference.
//! - `Audio` → rejected; we did not advertise `audio` in
//!   `PromptCapabilities`, so receiving one is a client bug.

use agent_client_protocol::Error;
use agent_client_protocol::schema::ContentBlock;
use agent_client_protocol::schema::EmbeddedResourceResource;
use agent_client_protocol::util::internal_error;
use craft_providers::ImageMediaType;
use craft_providers::ImageSource;
use std::sync::Arc;

const AUDIO_REJECTED_MSG: &str = "audio prompts are not supported";

/// Lowered ACP prompt: free-text plus attached images.
#[derive(Debug)]
pub struct LoweredPrompt {
    pub text: String,
    pub images: Vec<ImageSource>,
}

/// Convert a slice of ACP `ContentBlock`s into a `LoweredPrompt`.
///
/// Returns `Err` if an unsupported variant (currently `Audio`) is present;
/// other variants are best-effort flattened into text.
pub fn lower(blocks: &[ContentBlock]) -> Result<LoweredPrompt, Error> {
    let mut text = String::new();
    let mut images = Vec::new();
    for block in blocks {
        match block {
            ContentBlock::Text(t) => append_line(&mut text, &t.text),
            ContentBlock::Image(img) => match parse_image(&img.mime_type, &img.data) {
                Some(src) => images.push(src),
                None => append_line(
                    &mut text,
                    &format!("[image: {} (unsupported by craft)]", img.mime_type),
                ),
            },
            ContentBlock::ResourceLink(link) => {
                append_line(&mut text, &format_link(&link.name, &link.uri));
            }
            ContentBlock::Resource(res) => match &res.resource {
                EmbeddedResourceResource::TextResourceContents(t) => {
                    append_line(&mut text, &format_text_resource(&t.uri, &t.text));
                }
                EmbeddedResourceResource::BlobResourceContents(b) => {
                    append_line(
                        &mut text,
                        &format_blob_resource(
                            &b.uri,
                            b.mime_type.as_deref(),
                            b.blob.len(),
                        ),
                    );
                }
                other => append_line(&mut text, &format!("[unsupported embedded resource: {other:?}]")),
            },
            ContentBlock::Audio(_) => return Err(internal_error(AUDIO_REJECTED_MSG)),
            other => append_line(&mut text, &format!("[unsupported content block: {other:?}]")),
        }
    }
    Ok(LoweredPrompt { text, images })
}

fn append_line(out: &mut String, line: &str) {
    if !out.is_empty() {
        out.push('\n');
    }
    out.push_str(line);
}

fn parse_image(mime: &str, data: &str) -> Option<ImageSource> {
    let media = match mime {
        "image/png" => ImageMediaType::Png,
        "image/jpeg" | "image/jpg" => ImageMediaType::Jpeg,
        "image/gif" => ImageMediaType::Gif,
        "image/webp" => ImageMediaType::Webp,
        _ => return None,
    };
    Some(ImageSource::new(media, Arc::from(data)))
}

fn format_link(name: &str, uri: &str) -> String {
    format!("[resource: {name} ({uri})]")
}

fn format_text_resource(uri: &str, text: &str) -> String {
    format!("[resource: {uri}]\n```\n{text}\n```")
}

fn format_blob_resource(uri: &str, mime: Option<&str>, byte_len: usize) -> String {
    let mime = mime.unwrap_or("application/octet-stream");
    format!("[binary resource: {uri} ({mime}, {byte_len} bytes base64)]")
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent_client_protocol::schema::AudioContent;
    use agent_client_protocol::schema::EmbeddedResource;
    use agent_client_protocol::schema::ImageContent;
    use agent_client_protocol::schema::ResourceLink;
    use agent_client_protocol::schema::TextContent;
    use agent_client_protocol::schema::TextResourceContents;
    use agent_client_protocol::schema::BlobResourceContents;

    #[test]
    fn text_blocks_concatenate_with_newlines() {
        let blocks = vec![
            ContentBlock::Text(TextContent::new("hello")),
            ContentBlock::Text(TextContent::new("world")),
        ];
        let lowered = lower(&blocks).unwrap();
        assert_eq!(lowered.text, "hello\nworld");
        assert!(lowered.images.is_empty());
    }

    #[test]
    fn image_png_passes_through_as_image_source() {
        let blocks = vec![ContentBlock::Image(ImageContent::new("abc123", "image/png"))];
        let lowered = lower(&blocks).unwrap();
        assert!(lowered.text.is_empty());
        assert_eq!(lowered.images.len(), 1);
        assert_eq!(lowered.images[0].media_type, ImageMediaType::Png);
        assert_eq!(&*lowered.images[0].data, "abc123");
    }

    #[test]
    fn image_unsupported_mime_falls_back_to_text() {
        let blocks = vec![ContentBlock::Image(ImageContent::new("xx", "image/svg+xml"))];
        let lowered = lower(&blocks).unwrap();
        assert!(lowered.images.is_empty());
        assert!(lowered.text.contains("image/svg+xml"));
    }

    #[test]
    fn resource_link_inlined_as_text() {
        let blocks = vec![ContentBlock::ResourceLink(ResourceLink::new(
            "main.rs",
            "file:///x/main.rs",
        ))];
        let lowered = lower(&blocks).unwrap();
        assert_eq!(lowered.text, "[resource: main.rs (file:///x/main.rs)]");
    }

    #[test]
    fn embedded_text_resource_inlined_as_fenced_block() {
        let blocks = vec![ContentBlock::Resource(EmbeddedResource::new(
            EmbeddedResourceResource::TextResourceContents(TextResourceContents::new(
                "fn main() {}",
                "file:///x/main.rs",
            )),
        ))];
        let lowered = lower(&blocks).unwrap();
        assert!(lowered.text.contains("file:///x/main.rs"));
        assert!(lowered.text.contains("fn main()"));
    }

    #[test]
    fn embedded_blob_resource_inlined_as_text_reference() {
        let blocks = vec![ContentBlock::Resource(EmbeddedResource::new(
            EmbeddedResourceResource::BlobResourceContents(
                BlobResourceContents::new("AAAA", "file:///x.pdf")
                    .mime_type(Some("application/pdf".to_string())),
            ),
        ))];
        let lowered = lower(&blocks).unwrap();
        assert!(lowered.text.contains("application/pdf"));
        assert!(lowered.text.contains("file:///x.pdf"));
    }

    #[test]
    fn audio_returns_error() {
        let blocks = vec![ContentBlock::Audio(AudioContent::new("xx", "audio/mp3"))];
        let err = lower(&blocks).unwrap_err();
        let data = err.data.expect("data attached").to_string();
        assert!(data.contains(AUDIO_REJECTED_MSG), "data was: {data}");
    }

    #[test]
    fn empty_input_yields_empty_lowered() {
        let lowered = lower(&[]).unwrap();
        assert!(lowered.text.is_empty());
        assert!(lowered.images.is_empty());
    }
}
