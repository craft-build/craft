//! `view_image`: return an image file to the model as vision input.
//!
//! Limits: 50 MiB input cap,
//! 3 MiB raw-output cap (~4 MiB base64, provider-safe), 1568 px max edge
//! (providers downscale beyond this anyway). Files are fully decoded even on
//! the pass-through path: a corrupt file shipped undecoded poisons message
//! history and fails every later request.

use std::io::Cursor;
use std::path::Path;

use animist::agent::tool::{
    ToolContent, ToolContext, ToolDefinition, ToolImpl, ToolOutput, ToolScheduling,
};
use async_trait::async_trait;
use base64::Engine;
use image::{DynamicImage, ImageFormat};
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::Value;

use super::error::{Error, Result, definition, failure, finish, invalid, io_error};
use super::{Workspace, claims};
use crate::history::ImageSource;

const MAX_INPUT_BYTES: usize = 50 * 1024 * 1024;
const MAX_RAW_BYTES: usize = 3 * 1024 * 1024;
const MAX_EDGE: u32 = 1568;

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ViewImageArgs {
    /// Workspace-relative path, or an absolute path inside the workspace.
    pub path: String,
}

#[derive(Debug)]
pub struct ViewImageOutput {
    pub caption: String,
    pub media: crate::history::ext::ImageMedia,
    /// Base64-encoded image bytes.
    pub data: String,
}

impl super::error::IntoToolResult for ViewImageOutput {
    fn into_tool_result(self) -> ToolOutput {
        super::error::contents_output(vec![
            ToolContent::Text { text: self.caption },
            ToolContent::Image {
                source: ImageSource::Base64 {
                    media_type: self.media.as_str().to_owned(),
                    data: self.data,
                },
            },
        ])
    }
}

#[derive(Clone)]
pub struct ViewImage(pub Workspace);

impl ViewImage {
    async fn execute(
        workspace: &Workspace,
        args: ViewImageArgs,
        cancel: &animist::agent::Cancellation,
    ) -> Result<ViewImageOutput> {
        let path = workspace.file(&args.path)?;
        let _guard = workspace
            .read_guard(std::slice::from_ref(&args.path), cancel)
            .await?;
        let bytes = tokio::fs::read(&path).await.map_err(io_error)?;
        if bytes.len() > MAX_INPUT_BYTES {
            return Err(invalid(format!(
                "{} is too large to view ({}, limit {})",
                path.display(),
                format_size(bytes.len()),
                format_size(MAX_INPUT_BYTES)
            )));
        }
        // Decode, downscale, and re-encode are CPU-bound image work.
        let display = workspace.display(&path);
        let decoded =
            tokio::task::spawn_blocking(move || decode_and_encode(&bytes, &path, &display))
                .await
                .map_err(|source| Error::Blocking {
                    source: Box::new(source),
                })??;
        let DecodedImage {
            caption,
            media,
            data,
        } = decoded;
        Ok(ViewImageOutput {
            caption,
            media,
            data,
        })
    }
}

/// The blocking decode path, separated so it never touches the workspace's
/// async helpers. Produces the model-visible output directly.
struct DecodedImage {
    caption: String,
    media: crate::history::ext::ImageMedia,
    data: String,
}

fn decode_and_encode(bytes: &[u8], path: &Path, display: &str) -> Result<DecodedImage> {
    let format = image::guess_format(bytes)
        .map_err(|error| invalid(format!("{} is not an image: {error}", path.display())))?;
    let media = media_of(format).ok_or_else(|| {
        invalid(format!(
            "unsupported image format {format:?}: only png, jpeg, gif, and webp can be viewed"
        ))
    })?;
    // Decode fully even on the pass-through path so corrupt files fail
    // here instead of poisoning every later request.
    let img = image::ImageReader::with_format(Cursor::new(bytes), format)
        .decode()
        .map_err(|error| failure(format!("cannot decode {}: {error}", path.display())))?;
    let (orig_width, orig_height) = (img.width(), img.height());

    if bytes.len() <= MAX_RAW_BYTES && orig_width.max(orig_height) <= MAX_EDGE {
        return Ok(DecodedImage {
            caption: format!(
                "[image: {} {} {}x{}]",
                display,
                format_size(bytes.len()),
                orig_width,
                orig_height
            ),
            media,
            data: base64::engine::general_purpose::STANDARD.encode(bytes),
        });
    }

    // Too big for the API: downscale to fit MAX_EDGE and re-encode.
    // JPEG stays JPEG (photos recompress far smaller); everything else
    // becomes PNG since gif/webp encoding isn't supported by the image
    // crate.
    let resized = orig_width.max(orig_height) > MAX_EDGE;
    let img = if resized {
        img.resize(MAX_EDGE, MAX_EDGE, image::imageops::FilterType::Lanczos3)
    } else {
        img
    };
    let mut out_format = if format == ImageFormat::Jpeg {
        ImageFormat::Jpeg
    } else {
        ImageFormat::Png
    };
    let mut encoded = encode(&img, out_format)?;
    if encoded.len() > MAX_RAW_BYTES && out_format == ImageFormat::Png {
        // PNG can stay huge at 1568px (e.g. noisy screenshots); JPEG is
        // the only remaining lever.
        out_format = ImageFormat::Jpeg;
        encoded = encode(&img, out_format)?;
    }
    if encoded.len() > MAX_RAW_BYTES {
        return Err(invalid(format!(
            "{} is too large to view ({} after downscaling, limit {})",
            path.display(),
            format_size(encoded.len()),
            format_size(MAX_RAW_BYTES)
        )));
    }

    let mut note = if resized {
        format!(", downscaled from {orig_width}x{orig_height}")
    } else {
        ", re-encoded".to_string()
    };
    // Animated gif/webp lose their animation when re-encoded.
    if matches!(format, ImageFormat::Gif | ImageFormat::WebP) {
        note.push_str(", first frame only");
    }
    let media = media_of(out_format).unwrap_or(media);
    Ok(DecodedImage {
        caption: format!(
            "[image: {} {} {}x{}{}]",
            display,
            format_size(encoded.len()),
            img.width(),
            img.height(),
            note
        ),
        media,
        data: base64::engine::general_purpose::STANDARD.encode(&encoded),
    })
}

fn encode(img: &DynamicImage, format: ImageFormat) -> Result<Vec<u8>> {
    let mut buf = Vec::new();
    // The JPEG encoder takes RGB; alpha is meaningless there anyway.
    let flat = if format == ImageFormat::Jpeg {
        DynamicImage::ImageRgb8(img.to_rgb8())
    } else {
        img.clone()
    };
    flat.write_to(&mut Cursor::new(&mut buf), format)
        .map_err(|error| failure(format!("re-encode failed: {error}")))?;
    Ok(buf)
}

fn media_of(format: ImageFormat) -> Option<crate::history::ext::ImageMedia> {
    match format {
        ImageFormat::Png => Some(crate::history::ext::ImageMedia::Png),
        ImageFormat::Jpeg => Some(crate::history::ext::ImageMedia::Jpeg),
        ImageFormat::Gif => Some(crate::history::ext::ImageMedia::Gif),
        ImageFormat::WebP => Some(crate::history::ext::ImageMedia::Webp),
        _ => None,
    }
}

fn format_size(bytes: usize) -> String {
    if bytes >= 1024 * 1024 {
        format!("{:.1}MB", bytes as f64 / (1024.0 * 1024.0))
    } else {
        format!("{}KB", bytes.div_ceil(1024))
    }
}

#[async_trait]
impl ToolImpl for ViewImage {
    type Args = ViewImageArgs;
    type Output = ToolOutput;
    type Error = Error;

    fn definition(&self) -> ToolDefinition {
        definition::<ViewImageArgs>(
            "view_image",
            "View an image file (png, jpeg, gif, webp) so you can actually see it; it is returned as vision input alongside the tool result. Use instead of `read` for images. Paths are workspace-relative or absolute inside the workspace. Oversized images are downscaled automatically (animated gif/webp keep only the first frame).",
        )
    }

    fn scheduling(&self, input: &Value) -> ToolScheduling {
        match input.get("path").and_then(Value::as_str) {
            Some(path) if !path.is_empty() => {
                claims::reads(self.0.root_key().as_deref(), &[path.to_owned()])
            }
            _ => ToolScheduling::Exclusive,
        }
    }

    async fn call(&self, context: &ToolContext<'_>, args: ViewImageArgs) -> Result<ToolOutput> {
        finish(Self::execute(&self.0, args, &context.run.cancellation).await)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn format_size_units() {
        assert_eq!(format_size(512), "1KB");
        assert_eq!(format_size(3 * 1024 * 1024), "3.0MB");
    }
}
