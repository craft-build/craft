//! Inline image rendering via `ratatui-image`.
//!
//! A `Picker` is built once at UI startup, querying the terminal for its
//! graphics protocol and font size. Kitty/Ghostty/iTerm2 get native
//! protocol sequences; everything else (and any multiplexer, since Kitty
//! escapes don't survive tmux/screen DCS passthrough) falls back to
//! unicode halfblocks so *something* always renders.

use std::sync::Arc;

use base64::Engine;
use ratatui::layout::Size;
use ratatui_image::picker::Picker;
use ratatui_image::protocol::Protocol;
use ratatui_image::{Image, Resize};

use craft_agent::ImageSource;

/// Cap on image height in terminal rows so a tall screenshot doesn't
/// consume the whole scrollback.
const MAX_IMAGE_ROWS: u16 = 30;

/// Resolved once at startup; cheaply cloneable for sharing across renders.
#[derive(Clone)]
pub(crate) struct ImagePicker {
    picker: Picker,
}

impl ImagePicker {
    /// Query the terminal for its graphics protocol and font size.
    /// Falls back to halfblocks on any failure (piped stdio, mux without
    /// passthrough, unsupported terminal). Inside a detected multiplexer
    /// we force halfblocks regardless of the query result, because
    /// Kitty/iTerm2 escape sequences don't survive tmux/screen DCS
    /// passthrough reliably.
    pub(crate) fn new() -> Self {
        if crate::terminal::is_muxed() {
            return Self {
                picker: Picker::halfblocks(),
            };
        }
        let picker = Picker::from_query_stdio().unwrap_or_else(|_| Picker::halfblocks());
        Self { picker }
    }

    /// Build a renderable image state from a base64-encoded `ImageSource`,
    /// scaled to fit `avail_width` columns. Returns `None` if the bytes
    /// can't be decoded.
    pub(crate) fn render_state(
        &self,
        source: &ImageSource,
        avail_width: u16,
    ) -> Option<ImageRenderState> {
        let raw = base64::engine::general_purpose::STANDARD
            .decode(source.data.as_bytes())
            .ok()?;
        let dyn_img = image::load_from_memory(&raw).ok()?;
        let img = dyn_img.to_rgba8();
        let (px_w, px_h) = img.dimensions();
        let font = self.picker.font_size();
        let fw = font.width.max(1) as u32;
        let fh = font.height.max(1) as u32;

        let natural_cols = px_w.div_ceil(fw);
        let natural_rows = px_h.div_ceil(fh);

        let avail_cols = avail_width.max(1) as u32;
        let scale = avail_cols.min(natural_cols) as f64 / natural_cols.max(1) as f64;
        let scaled_rows = ((natural_rows as f64) * scale).ceil() as u16;
        let target_rows = scaled_rows.clamp(1, MAX_IMAGE_ROWS);

        let area = Size {
            width: avail_width.max(1),
            height: target_rows,
        };
        let protocol = self
            .picker
            .new_protocol(dyn_img, area, Resize::Fit(None))
            .ok()?;
        Some(ImageRenderState {
            protocol: Arc::new(protocol),
            rows: target_rows,
        })
    }
}

/// A decoded image ready to render into a frame area, plus its computed
/// cell height for scroll/height math.
pub(crate) struct ImageRenderState {
    protocol: Arc<Protocol>,
    pub rows: u16,
}

impl ImageRenderState {
    /// Render the image widget into `area`. The area's height should match
    /// `self.rows`; width should be `<=` the available columns.
    pub(crate) fn render(&self, area: ratatui::layout::Rect, frame: &mut ratatui::Frame) {
        frame.render_widget(Image::new(&self.protocol), area);
    }
}

/// Decode-test helper exposed for unit tests: returns the cell dimensions
/// the image would occupy without building a full protocol.
#[cfg(test)]
pub(crate) fn cell_dims(source: &ImageSource, font_w: u16, font_h: u16) -> Option<(u16, u16)> {
    let raw = base64::engine::general_purpose::STANDARD
        .decode(source.data.as_bytes())
        .ok()?;
    let img = image::load_from_memory(&raw).ok()?.to_rgba8();
    let (px_w, px_h) = img.dimensions();
    Some((
        (px_w as u16).div_ceil(font_w.max(1)),
        (px_h as u16).div_ceil(font_h.max(1)),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use craft_agent::ImageMediaType;

    fn png_source(px_w: u32, px_h: u32) -> ImageSource {
        let img = image::RgbaImage::new(px_w, px_h);
        let mut buf = Vec::new();
        let mut cursor = std::io::Cursor::new(&mut buf);
        image::DynamicImage::ImageRgba8(img)
            .write_to(&mut cursor, image::ImageFormat::Png)
            .unwrap();
        let b64 = base64::engine::general_purpose::STANDARD.encode(&buf);
        ImageSource::new(ImageMediaType::Png, Arc::from(b64))
    }

    #[test]
    fn cell_dims_exact_multiple() {
        let src = png_source(26, 52);
        let (cols, rows) = cell_dims(&src, 13, 26).expect("dims");
        assert_eq!((cols, rows), (2, 2));
    }

    #[test]
    fn cell_dims_rounds_up() {
        let src = png_source(14, 27);
        let (cols, rows) = cell_dims(&src, 13, 26).expect("dims");
        assert_eq!((cols, rows), (2, 2));
    }

    #[test]
    fn cell_dims_handles_zero_font() {
        let src = png_source(26, 26);
        let (cols, rows) = cell_dims(&src, 0, 0).expect("dims");
        assert_eq!((cols, rows), (26, 26));
    }
}
