//! Inline image rendering via `ratatui-image`.
//!
//! A `Picker` is built once at UI startup, querying the terminal for its
//! graphics protocol and font size. Kitty/Ghostty/iTerm2 get native protocol
//! sequences; everything else (and any multiplexer, since kitty escapes
//! don't survive tmux/screen DCS passthrough) falls back to unicode
//! halfblocks so *something* always renders.
//!
//! Ported from the reference `craft-ui/src/image_render.rs`. Where the
//! reference carries a `craft_agent::ImageSource`, this repo's images are
//! the `history::ImageBlock` base64 payloads produced by `view_image` and
//! composer attachments.

use std::sync::{Arc, OnceLock};

use base64::Engine;
use ratatui::layout::Size;
use ratatui_image::picker::Picker;
use ratatui_image::protocol::Protocol;
use ratatui_image::{Image, Resize};

/// Cap on image height in terminal rows so a tall screenshot doesn't
/// consume the whole scrollback.
const MAX_IMAGE_ROWS: u16 = 30;

/// Pixel budget for a decoded image (`MAX_IMAGE_PIXELS`, matching the
/// attachment cap below): anything past it — including a `u32` overflow
/// in `w * h` — is refused before/after decode.
fn exceeds_pixel_cap(w: u32, h: u32) -> bool {
    w.checked_mul(h)
        .map_or(true, |p| p as usize > MAX_IMAGE_PIXELS)
}

/// The single probed `Picker`. `Picker::from_query_stdio` sends kitty/DA/DSR
/// queries and reads the replies straight from stdin, so it must run before
/// the input reader thread starts (see [`probe`]); probing later races the
/// reader and leaks the `Gi=31;OK` graphics reply into the input bar. The
/// protocol and font size never change for the life of the process, so probe
/// once and clone.
static PICKER: OnceLock<Picker> = OnceLock::new();

/// Probe the terminal's graphics protocol and font size. Must be called
/// before the input reader thread starts; under test (or a muxed terminal)
/// halfblocks are forced without touching stdio.
pub fn probe() {
    let _ = shared_picker();
}

fn shared_picker() -> Picker {
    PICKER
        .get_or_init(|| {
            if cfg!(test) || crate::tui::hyperlink::is_muxed() {
                return Picker::halfblocks();
            }
            Picker::from_query_stdio().unwrap_or_else(|_| Picker::halfblocks())
        })
        .clone()
}

/// Resolved once at startup; cheaply cloneable for sharing across renders.
#[derive(Clone)]
pub struct ImagePicker {
    picker: Picker,
}

impl ImagePicker {
    /// Use the shared probed picker (halfblocks until [`probe`] ran).
    pub fn new() -> Self {
        Self {
            picker: shared_picker(),
        }
    }

    /// Build a renderable image state from a base64-encoded image payload,
    /// scaled to fit `avail_width` columns. Returns `None` if the bytes
    /// can't be decoded or the image exceeds the pixel cap.
    pub fn render_state(&self, data_b64: &str, avail_width: u16) -> Option<ImageRenderState> {
        let raw = base64::engine::general_purpose::STANDARD
            .decode(data_b64.as_bytes())
            .ok()?;
        // Pixel cap BEFORE any full decode: a hostile multi-hundred-MP
        // PNG decodes to gigabytes and OOMs the process. Read just the
        // headers for the dimensions and refuse past the cap; formats
        // without a readable header fall through to the post-decode
        // check below.
        if let Ok(reader) =
            image::ImageReader::new(std::io::Cursor::new(&raw)).with_guessed_format()
            && let Ok((w, h)) = reader.into_dimensions()
            && exceeds_pixel_cap(w, h)
        {
            return None;
        }
        let dyn_img = image::load_from_memory(&raw).ok()?;
        let (px_w, px_h) = image::GenericImageView::dimensions(&dyn_img);
        if exceeds_pixel_cap(px_w, px_h) {
            return None;
        }
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
pub struct ImageRenderState {
    protocol: Arc<Protocol>,
    pub rows: u16,
}

impl ImageRenderState {
    /// Render the image widget into `area`. The area's height should be
    /// `<= self.rows`; width should be `<=` the available columns.
    pub fn render(&self, area: ratatui::layout::Rect, frame: &mut ratatui::Frame) {
        frame.render_widget(Image::new(&self.protocol), area);
    }
}

/// Decode-test helper exposed for unit tests: returns the cell dimensions
/// the image would occupy without building a full protocol.
#[cfg(test)]
pub(crate) fn cell_dims(data_b64: &str, font_w: u16, font_h: u16) -> Option<(u16, u16)> {
    let raw = base64::engine::general_purpose::STANDARD
        .decode(data_b64.as_bytes())
        .ok()?;
    let img = image::load_from_memory(&raw).ok()?.to_rgba8();
    let (px_w, px_h) = img.dimensions();
    Some((
        (px_w as u16).div_ceil(font_w.max(1)),
        (px_h as u16).div_ceil(font_h.max(1)),
    ))
}

// ---------------------------------------------------------------------------
// Attachment loading (path picks and clipboard pastes)
// ---------------------------------------------------------------------------

const MAX_IMAGE_PIXELS: usize = 8_000_000;
const MAX_IMAGE_BYTES: usize = 20 * 1024 * 1024;

const IMAGE_EXTENSIONS: &[(&str, crate::history::ImageMedia)] = &[
    ("png", crate::history::ImageMedia::Png),
    ("jpg", crate::history::ImageMedia::Jpeg),
    ("jpeg", crate::history::ImageMedia::Jpeg),
    ("gif", crate::history::ImageMedia::Gif),
    ("webp", crate::history::ImageMedia::Webp),
];

fn media_type_for(path: &std::path::Path) -> Option<crate::history::ImageMedia> {
    let ext = path.extension()?.to_str()?.to_ascii_lowercase();
    IMAGE_EXTENSIONS
        .iter()
        .find(|(e, _)| *e == ext)
        .map(|&(_, mt)| mt)
}

/// A pasted blob that is nothing but an image path: absolute, `~/`, or a
/// `file://` URI, with an image extension. Anything else is text.
pub fn try_parse_image_path(
    text: &str,
) -> Option<(std::path::PathBuf, crate::history::ImageMedia)> {
    let trimmed = text.trim().trim_matches('\'');
    let (path_str, was_file_uri) = match trimmed.strip_prefix("file://") {
        Some(rest) => (rest.replace("\\ ", " "), true),
        None => (trimmed.replace("\\ ", " "), false),
    };
    if path_str.contains("://") {
        return None;
    }
    let is_absolute = path_str.starts_with('/');
    if !was_file_uri && !is_absolute && !path_str.starts_with("~/") {
        return None;
    }
    let path = if let Some(rest) = path_str.strip_prefix("~/") {
        env_home()?.join(rest)
    } else {
        std::path::PathBuf::from(&path_str)
    };
    let media_type = media_type_for(&path)?;
    Some((path, media_type))
}

fn env_home() -> Option<std::path::PathBuf> {
    std::env::var_os("HOME")
        .filter(|h| !h.is_empty())
        .map(std::path::PathBuf::from)
}

/// Read an image file into a base64 attachment block.
pub fn load_file_image(
    path: &std::path::Path,
    media_type: crate::history::ImageMedia,
) -> Result<crate::history::ImageBlock, String> {
    let bytes = std::fs::read(path).map_err(|e| format!("{}: {e}", path.display()))?;
    if bytes.len() > MAX_IMAGE_BYTES {
        return Err("Image file exceeds 20MB limit".into());
    }
    let b64 = base64::engine::general_purpose::STANDARD.encode(&bytes);
    Ok(crate::history::ImageBlock {
        media_type,
        data: b64,
        caption: format!("[image: {}]", path.display()),
    })
}

/// Read the clipboard image (Ctrl+V) into a base64 PNG attachment block.
pub fn load_clipboard_image() -> Result<crate::history::ImageBlock, String> {
    let mut cb = arboard::Clipboard::new().map_err(|e| e.to_string())?;
    let img = cb.get_image().map_err(|e| e.to_string())?;
    let pixels = img
        .width
        .checked_mul(img.height)
        .ok_or_else(|| format!("Image dimensions overflow ({}x{})", img.width, img.height))?;
    if pixels > MAX_IMAGE_PIXELS {
        return Err(format!("Image too large ({}x{})", img.width, img.height));
    }
    let png_bytes = encode_rgba_to_png(img.width as u32, img.height as u32, &img.bytes)?;
    if png_bytes.len() > MAX_IMAGE_BYTES {
        return Err("Encoded image exceeds 20MB limit".into());
    }
    let b64 = base64::engine::general_purpose::STANDARD.encode(&png_bytes);
    Ok(crate::history::ImageBlock {
        media_type: crate::history::ImageMedia::Png,
        data: b64,
        caption: "[pasted image]".into(),
    })
}

fn encode_rgba_to_png(width: u32, height: u32, rgba: &[u8]) -> Result<Vec<u8>, String> {
    let img = image::RgbaImage::from_raw(width, height, rgba.to_vec())
        .ok_or("Invalid image dimensions")?;
    let mut buf = Vec::new();
    img.write_to(&mut std::io::Cursor::new(&mut buf), image::ImageFormat::Png)
        .map_err(|e| e.to_string())?;
    Ok(buf)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn png_source(px_w: u32, px_h: u32) -> String {
        let img = image::RgbaImage::new(px_w, px_h);
        let mut buf = Vec::new();
        let mut cursor = std::io::Cursor::new(&mut buf);
        image::DynamicImage::ImageRgba8(img)
            .write_to(&mut cursor, image::ImageFormat::Png)
            .unwrap();
        base64::engine::general_purpose::STANDARD.encode(&buf)
    }

    #[test]
    fn parse_image_path_accepts_file_uris_and_absolute_paths() {
        let (p, m) = try_parse_image_path("file:///home/user/photo.png").unwrap();
        assert_eq!(p.to_str().unwrap(), "/home/user/photo.png");
        assert_eq!(m, crate::history::ImageMedia::Png);
        let (p, _) = try_parse_image_path("  '/home/user/photo.jpg'\n").unwrap();
        assert_eq!(p.to_str().unwrap(), "/home/user/photo.jpg");
    }

    #[test]
    fn parse_image_path_rejects_text_and_relative_paths() {
        assert!(try_parse_image_path("hello world").is_none());
        assert!(try_parse_image_path("/home/user/readme.txt").is_none());
        assert!(try_parse_image_path("https://example.com/a.png").is_none());
        assert!(try_parse_image_path("relative.png").is_none());
        assert!(try_parse_image_path("look at /home/user/photo.png").is_none());
    }

    #[test]
    fn load_file_image_reports_missing_files() {
        assert!(
            load_file_image(
                std::path::Path::new("/nonexistent/image.png"),
                crate::history::ImageMedia::Png
            )
            .is_err()
        );
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

    #[test]
    fn render_state_decodes_and_caps_rows() {
        let picker = ImagePicker::new(); // halfblocks under test
        let src = png_source(80, 40); // 2 rows at halfblock default font
        let state = picker.render_state(&src, 40).expect("state");
        assert!(state.rows >= 1);
        assert!(state.rows <= MAX_IMAGE_ROWS);
    }

    #[test]
    fn render_state_rejects_garbage() {
        let picker = ImagePicker::new();
        assert!(picker.render_state("not base64!!", 40).is_none());
        let empty = base64::engine::general_purpose::STANDARD.encode(b"");
        assert!(picker.render_state(&empty, 40).is_none());
    }

    /// The cap predicate itself: 8 MP is the boundary, overflow counts
    /// as over the cap.
    #[test]
    fn pixel_cap_boundary_and_overflow() {
        assert!(!exceeds_pixel_cap(2000, 4000)); // exactly 8 MP
        assert!(exceeds_pixel_cap(3000, 3000)); // 9 MP
        assert!(exceeds_pixel_cap(u32::MAX, 2)); // w*h overflow
        assert!(!exceeds_pixel_cap(0, 0));
    }

    /// PNG chunk CRC (IEEE, poly 0xEDB88320), table-free: only used to
    /// forge test fixtures.
    fn crc32(bytes: &[u8]) -> u32 {
        let mut crc = !0u32;
        for &b in bytes {
            crc ^= u32::from(b);
            for _ in 0..8 {
                crc = (crc >> 1) ^ ((crc & 1).wrapping_neg() & 0xEDB8_8320);
            }
        }
        !crc
    }

    fn png_chunk(kind: &[u8; 4], data: &[u8]) -> Vec<u8> {
        let mut out = (data.len() as u32).to_be_bytes().to_vec();
        out.extend_from_slice(kind);
        out.extend_from_slice(data);
        let mut signed = kind.to_vec();
        signed.extend_from_slice(data);
        out.extend_from_slice(&crc32(&signed).to_be_bytes());
        out
    }

    /// A tiny PNG whose IHDR alone declares `w x h` (8-bit RGBA, empty
    /// IDAT — headers are all the decoder needs for `into_dimensions`).
    fn header_only_png_b64(w: u32, h: u32) -> String {
        let mut png = b"\x89PNG\r\n\x1a\n".to_vec();
        let mut ihdr = w.to_be_bytes().to_vec();
        ihdr.extend_from_slice(&h.to_be_bytes());
        ihdr.extend_from_slice(&[8, 6, 0, 0, 0]);
        png.extend_from_slice(&png_chunk(b"IHDR", &ihdr));
        png.extend_from_slice(&png_chunk(b"IDAT", &[]));
        base64::engine::general_purpose::STANDARD.encode(&png)
    }

    /// A hostile PNG header declaring 400 Mpx is rejected from its
    /// headers alone — no multi-GB buffer is ever decoded (finding 62).
    #[test]
    fn render_state_rejects_oversized_dimensions_from_header() {
        let picker = ImagePicker::new();
        let tiny_file = header_only_png_b64(20_000, 20_000);
        assert!(
            tiny_file.len() < 256,
            "fixture stays tiny: {}",
            tiny_file.len()
        );
        assert!(picker.render_state(&tiny_file, 40).is_none());
    }

    /// An image at exactly the cap still loads (the guard is not
    /// trigger-happy). Gray 8-bit keeps the fixture buffer small.
    #[test]
    fn render_state_accepts_dimensions_at_the_cap() {
        let picker = ImagePicker::new();
        let img = image::GrayImage::new(4000, 2000); // == MAX_IMAGE_PIXELS
        let mut buf = Vec::new();
        image::DynamicImage::ImageLuma8(img)
            .write_to(&mut std::io::Cursor::new(&mut buf), image::ImageFormat::Png)
            .unwrap();
        let b64 = base64::engine::general_purpose::STANDARD.encode(&buf);
        assert!(picker.render_state(&b64, 40).is_some());
    }

    /// Just over the cap is refused even for a fully valid file.
    #[test]
    fn render_state_rejects_dimensions_just_over_the_cap() {
        let picker = ImagePicker::new();
        let img = image::GrayImage::new(4002, 2000); // > 8_000_000 px
        let mut buf = Vec::new();
        image::DynamicImage::ImageLuma8(img)
            .write_to(&mut std::io::Cursor::new(&mut buf), image::ImageFormat::Png)
            .unwrap();
        let b64 = base64::engine::general_purpose::STANDARD.encode(&buf);
        assert!(picker.render_state(&b64, 40).is_none());
    }
}
