use std::fs;

use base64::Engine as _;
use rig_core::tool::IntoToolOutput;
use serde_json::json;

use super::{invoke, workspace};
use crate::history::ImageMedia;
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

#[tokio::test]
async fn view_image_passes_through_small_images() {
    let (dir, workspace) = workspace();
    let original = write_png(dir.path(), "shot.png", 32, 48);
    let out = invoke(&ViewImage(workspace.clone()), json!({"path":"shot.png"}))
        .await
        .unwrap();
    assert_eq!(out.media, ImageMedia::Png);
    assert_eq!(
        out.data,
        base64::engine::general_purpose::STANDARD.encode(&original)
    );
    assert_eq!(out.caption, "[image: shot.png 1KB 32x48]");

    let jpeg =
        image::DynamicImage::ImageRgb8(image::RgbImage::from_pixel(16, 16, image::Rgb([1, 2, 3])));
    let mut buf = Vec::new();
    jpeg.write_to(
        &mut std::io::Cursor::new(&mut buf),
        image::ImageFormat::Jpeg,
    )
    .unwrap();
    fs::write(dir.path().join("photo.jpeg"), &buf).unwrap();
    let out = invoke(&ViewImage(workspace), json!({"path":"photo.jpeg"}))
        .await
        .unwrap();
    assert_eq!(out.media, ImageMedia::Jpeg);
    assert!(out.caption.contains("16x16"));
}

#[tokio::test]
async fn view_image_downscales_oversized_dimensions() {
    let (dir, workspace) = workspace();
    write_png(dir.path(), "big.png", 2000, 1200);
    let out = invoke(&ViewImage(workspace), json!({"path":"big.png"}))
        .await
        .unwrap();
    assert!(
        out.caption.contains("downscaled from 2000x1200"),
        "{}",
        out.caption
    );
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(&out.data)
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
    let out = invoke(&ViewImage(workspace), json!({"path":"anim.gif"}))
        .await
        .unwrap();
    assert!(out.caption.contains("first frame only"), "{}", out.caption);
    assert!(out.caption.contains("downscaled from 2000x64"));
    // gif cannot be re-encoded; the payload becomes png
    assert_eq!(out.media, ImageMedia::Png);
}

#[tokio::test]
async fn view_image_rejects_corrupt_and_unsupported_files() {
    let (dir, workspace) = workspace();
    fs::write(dir.path().join("broken.png"), b"\x89PNG\r\n\x1a\ngarbage").unwrap();
    let error = invoke(&ViewImage(workspace.clone()), json!({"path":"broken.png"}))
        .await
        .unwrap_err();
    assert!(error.to_string().contains("decode"), "{error}");

    let bmp =
        image::DynamicImage::ImageRgb8(image::RgbImage::from_pixel(8, 8, image::Rgb([0, 0, 0])));
    let mut buf = Vec::new();
    bmp.write_to(&mut std::io::Cursor::new(&mut buf), image::ImageFormat::Bmp)
        .unwrap();
    fs::write(dir.path().join("x.bmp"), &buf).unwrap();
    let error = invoke(&ViewImage(workspace.clone()), json!({"path":"x.bmp"}))
        .await
        .unwrap_err();
    assert!(error.to_string().contains("unsupported"), "{error}");

    fs::write(dir.path().join("note.txt"), "hi").unwrap();
    assert!(
        invoke(&ViewImage(workspace.clone()), json!({"path":"note.txt"}))
            .await
            .is_err()
    );
    assert!(
        invoke(&ViewImage(workspace.clone()), json!({"path":"missing.png"}))
            .await
            .is_err()
    );
    assert!(
        invoke(&ViewImage(workspace.clone()), json!({"path":"."}))
            .await
            .is_err()
    );
    let error = invoke(
        &ViewImage(workspace.clone()),
        json!({"path":"../../etc/passwd"}),
    )
    .await
    .unwrap_err();
    assert!(
        error.to_string().contains("workspace") || error.to_string().contains("not allowed"),
        "{error}"
    );
}

#[tokio::test]
async fn view_image_output_carries_caption_and_image_blocks() {
    let (dir, workspace) = workspace();
    write_png(dir.path(), "ok.png", 10, 10);
    let out = invoke(&ViewImage(workspace), json!({"path":"ok.png"}))
        .await
        .unwrap();
    let output = out.into_tool_output().unwrap();
    assert!(output.as_text().is_none());
    let blocks = output.as_content();
    assert_eq!(blocks.len(), 2);
    assert!(blocks[0].as_text().is_some());
    assert!(matches!(
        &blocks[1],
        rig_core::completion::message::ToolResultContent::Image(image)
            if image.media_type
                == Some(rig_core::completion::message::ImageMediaType::PNG)
    ));
}
