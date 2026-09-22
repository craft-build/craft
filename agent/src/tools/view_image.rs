//! `view_image`: return an image file to the model as vision input.
//!
//! Ported from the reference `plugins/view_image`. Limits: 50 MiB input cap,
//! 3 MiB raw-output cap (~4 MiB base64, provider-safe), 1568 px max edge
//! (providers downscale beyond this anyway). Files are fully decoded even on
//! the pass-through path: a corrupt file shipped undecoded poisons message
//! history and fails every later request.

use std::io::Cursor;
use std::path::Path;

use base64::Engine;
use image::{DynamicImage, ImageFormat};
use rig_core::completion::message::{ImageMediaType, ToolResultContent as RigToolResultContent};
use rig_core::tool::{IntoToolOutput, ToolOutput};
use schemars::JsonSchema;
use serde::Deserialize;

use super::{Result, Workspace, failure, impl_tool, invalid};
use crate::history::ImageMedia;

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
    pub media: ImageMedia,
    /// Base64-encoded image bytes.
    pub data: String,
}

impl IntoToolOutput for ViewImageOutput {
    fn into_tool_output(
        self,
    ) -> std::result::Result<ToolOutput, rig_core::tool::ToolExecutionError> {
        let media = match self.media {
            ImageMedia::Png => ImageMediaType::PNG,
            ImageMedia::Jpeg => ImageMediaType::JPEG,
            ImageMedia::Gif => ImageMediaType::GIF,
            ImageMedia::Webp => ImageMediaType::WEBP,
        };
        ToolOutput::content(vec![
            RigToolResultContent::text(self.caption),
            RigToolResultContent::image_base64(self.data, Some(media), None),
        ])
    }
}

#[derive(Clone)]
pub struct ViewImage(pub Workspace);

impl ViewImage {
    fn execute(workspace: &Workspace, args: ViewImageArgs) -> Result<ViewImageOutput> {
        let path = workspace.file(&args.path)?;
        let bytes = std::fs::read(&path).map_err(super::io_error)?;
        if bytes.len() > MAX_INPUT_BYTES {
            return Err(invalid(format!(
                "{} is too large to view ({}, limit {})",
                path.display(),
                format_size(bytes.len()),
                format_size(MAX_INPUT_BYTES)
            )));
        }
        let format = image::guess_format(&bytes)
            .map_err(|error| invalid(format!("{} is not an image: {error}", path.display())))?;
        let media = media_of(format).ok_or_else(|| {
            invalid(format!(
                "unsupported image format {format:?}: only png, jpeg, gif, and webp can be viewed"
            ))
        })?;
        // Decode fully even on the pass-through path so corrupt files fail
        // here instead of poisoning every later request.
        let img = image::ImageReader::with_format(Cursor::new(&bytes), format)
            .decode()
            .map_err(|error| failure(format!("cannot decode {}: {error}", path.display())))?;
        let (orig_width, orig_height) = (img.width(), img.height());

        if bytes.len() <= MAX_RAW_BYTES && orig_width.max(orig_height) <= MAX_EDGE {
            return Ok(ViewImageOutput {
                caption: caption(workspace, &path, bytes.len(), orig_width, orig_height, ""),
                media,
                data: base64::engine::general_purpose::STANDARD.encode(&bytes),
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
        Ok(ViewImageOutput {
            caption: caption(
                workspace,
                &path,
                encoded.len(),
                img.width(),
                img.height(),
                &note,
            ),
            media,
            data: base64::engine::general_purpose::STANDARD.encode(&encoded),
        })
    }
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

fn media_of(format: ImageFormat) -> Option<ImageMedia> {
    match format {
        ImageFormat::Png => Some(ImageMedia::Png),
        ImageFormat::Jpeg => Some(ImageMedia::Jpeg),
        ImageFormat::Gif => Some(ImageMedia::Gif),
        ImageFormat::WebP => Some(ImageMedia::Webp),
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

fn caption(
    workspace: &Workspace,
    path: &Path,
    bytes: usize,
    width: u32,
    height: u32,
    note: &str,
) -> String {
    format!(
        "[image: {} {} {}x{}{}]",
        workspace.display(path),
        format_size(bytes),
        width,
        height,
        note
    )
}

impl_tool!(
    ViewImage,
    ViewImageArgs,
    ViewImageOutput,
    "view_image",
    "View an image file (png, jpeg, gif, webp) so you can actually see it; it is returned as vision input alongside the tool result. Use instead of `read` for images. Paths are workspace-relative or absolute inside the workspace. Oversized images are downscaled automatically (animated gif/webp keep only the first frame)."
);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn format_size_units() {
        assert_eq!(format_size(512), "1KB");
        assert_eq!(format_size(3 * 1024 * 1024), "3.0MB");
    }
}
