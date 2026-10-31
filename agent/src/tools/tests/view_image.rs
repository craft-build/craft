use std::fs;

use base64::Engine as _;
use serde_json::json;

use super::{invoke, invoke_error, workspace};
use crate::history::ext::ImageMedia;
use crate::tools::*;

// ---------- view_image ----------

fn write_png(dir: &std::path::Path, name: &str, width: u32, height: u32) -> Vec<u8> {
    let img = image::DynamicImage::ImageRgb8(image::RgbImage::from_pixel(
        width,
        height,
        image::Rgb([80, 120, 200]),
    ));
    let mut buf = Vec::new();
    img.write_to(&mut std::io::Cursor::new(&mut buf), image::ImageFormat::Png)
        .unwrap();
    fs::write(dir.join(name), &buf).unwrap();
    buf
}

/// The (caption, media type, base64 data) of a successful view_image call.
fn image_parts(output: &animist::agent::tool::ToolOutput) -> (String, String, String) {
    use animist::agent::tool::ToolContent;
    use animist::history::ImageSource;
    let mut caption = None;
    let mut media = None;
    let mut data = None;
    for block in &output.content.0 {
        match block {
            ToolContent::Text { text } => caption = Some(text.clone()),
            ToolContent::Image {
                source:
                    ImageSource::Base64 {
                        media_type,
                        data: payload,
                    },
            } => {
                media = Some(media_type.clone());
                data = Some(payload.clone());
            }
            ToolContent::Image { .. } => {}
        }
    }
    (
        caption.expect("caption block"),
        media.expect("image block"),
        data.expect("image data"),
    )
}

#[tokio::test]
async fn view_image_passes_through_small_images() {
    let (dir, workspace) = workspace();
    let original = write_png(dir.path(), "shot.png", 32, 48);
    let out = invoke(&ViewImage(workspace.clone()), json!({"path":"shot.png"})).await;
    let (caption, media, data) = image_parts(&out);
    assert_eq!(media, "image/png");
    assert_eq!(
        data,
        base64::engine::general_purpose::STANDARD.encode(&original)
    );
    assert_eq!(caption, "[image: shot.png 1KB 32x48]");

    let jpeg =
        image::DynamicImage::ImageRgb8(image::RgbImage::from_pixel(16, 16, image::Rgb([1, 2, 3])));
    let mut buf = Vec::new();
    jpeg.write_to(
        &mut std::io::Cursor::new(&mut buf),
        image::ImageFormat::Jpeg,
    )
    .unwrap();
    fs::write(dir.path().join("photo.jpeg"), &buf).unwrap();
    let out = invoke(&ViewImage(workspace), json!({"path":"photo.jpeg"})).await;
    let (caption, media, _) = image_parts(&out);
    assert_eq!(media, "image/jpeg");
    assert!(caption.contains("16x16"));
    let _ = ImageMedia::Png;
}

#[tokio::test]
async fn view_image_downscales_oversized_dimensions() {
    let (dir, workspace) = workspace();
    write_png(dir.path(), "big.png", 2000, 1200);
    let out = invoke(&ViewImage(workspace), json!({"path":"big.png"})).await;
    let (caption, _, data) = image_parts(&out);
    assert!(caption.contains("downscaled from 2000x1200"), "{caption}");
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(&data)
        .unwrap();
    let decoded = image::load_from_memory(&bytes).unwrap();
    assert_eq!(decoded.width().max(decoded.height()), 1568);
}

#[tokio::test]
async fn view_image_gif_notes_first_frame_when_reencoded() {
    let (dir, workspace) = workspace();
    let img = image::DynamicImage::ImageRgb8(image::RgbImage::from_pixel(
        2000,
        64,
        image::Rgb([128, 128, 128]),
    ));
    let mut buf = Vec::new();
    img.write_to(&mut std::io::Cursor::new(&mut buf), image::ImageFormat::Gif)
        .unwrap();
    fs::write(dir.path().join("anim.gif"), &buf).unwrap();
    let out = invoke(&ViewImage(workspace), json!({"path":"anim.gif"})).await;
    let (caption, media, _) = image_parts(&out);
    assert!(caption.contains("first frame only"), "{caption}");
    assert!(caption.contains("downscaled from 2000x64"));
    // gif cannot be re-encoded; the payload becomes png
    assert_eq!(media, "image/png");
}

#[tokio::test]
async fn view_image_rejects_corrupt_and_unsupported_files() {
    let (dir, workspace) = workspace();
    fs::write(dir.path().join("broken.png"), b"\x89PNG\r\n\x1a\ngarbage").unwrap();
    let error = invoke_error(&ViewImage(workspace.clone()), json!({"path":"broken.png"})).await;
    assert!(error.contains("decode"), "{error}");

    let bmp =
        image::DynamicImage::ImageRgb8(image::RgbImage::from_pixel(8, 8, image::Rgb([0, 0, 0])));
    let mut buf = Vec::new();
    bmp.write_to(&mut std::io::Cursor::new(&mut buf), image::ImageFormat::Bmp)
        .unwrap();
    fs::write(dir.path().join("x.bmp"), &buf).unwrap();
    let error = invoke_error(&ViewImage(workspace.clone()), json!({"path":"x.bmp"})).await;
    assert!(error.contains("unsupported"), "{error}");

    fs::write(dir.path().join("note.txt"), "hi").unwrap();
    assert!(
        !invoke_error(&ViewImage(workspace.clone()), json!({"path":"note.txt"}))
            .await
            .is_empty()
    );
    assert!(
        !invoke_error(&ViewImage(workspace.clone()), json!({"path":"missing.png"}))
            .await
            .is_empty()
    );
    assert!(
        !invoke_error(&ViewImage(workspace.clone()), json!({"path":"."}))
            .await
            .is_empty()
    );
    let error = invoke_error(
        &ViewImage(workspace.clone()),
        json!({"path":"../../etc/passwd"}),
    )
    .await;
    assert!(
        error.contains("workspace") || error.contains("not allowed"),
        "{error}"
    );
}

#[tokio::test]
async fn view_image_output_carries_caption_and_image_blocks() {
    let (dir, workspace) = workspace();
    write_png(dir.path(), "ok.png", 10, 10);
    let out = invoke(&ViewImage(workspace), json!({"path":"ok.png"})).await;
    assert!(!out.is_error);
    // Two ordered blocks: the caption then the image.
    assert_eq!(out.content.0.len(), 2);
    assert!(matches!(
        out.content.0[0],
        animist::agent::tool::ToolContent::Text { .. }
    ));
    assert!(matches!(
        out.content.0[1],
        animist::agent::tool::ToolContent::Image { .. }
    ));
}
