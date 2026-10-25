//! Developer-only format conversion of the committed original Xylo artwork.
//! No drawing, tracing, external image runtime, or account access is involved.
use anyhow::{Context, Result, bail, ensure};
use image::{
    ImageFormat, Rgba, RgbaImage,
    imageops::{self, FilterType},
};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{
    io::{Cursor, Write},
    path::{Path, PathBuf},
};

const MASTER: &[u8] = include_bytes!("../../../packaging/branding/xylo-mark.png");
const MASTER_PATH: &str = "packaging/branding/xylo-mark.png";
const BACKGROUND: [u8; 4] = [250, 248, 244, 255];
const ICNS_SIZES: [(u32, &[u8; 4]); 7] = [
    (16, b"icp4"),
    (32, b"icp5"),
    (64, b"icp6"),
    (128, b"ic07"),
    (256, b"ic08"),
    (512, b"ic09"),
    (1024, b"ic10"),
];

fn hash(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}
fn master() -> Result<RgbaImage> {
    let image = image::load_from_memory_with_format(MASTER, ImageFormat::Png)?.into_rgba8();
    ensure!(
        image.width() == image.height() && image.width() >= 512,
        "Xylo master must be a square high-resolution RGBA PNG"
    );
    Ok(image)
}
fn resized(master: &RgbaImage, size: u32) -> Result<RgbaImage> {
    ensure!(
        (1..=2048).contains(&size),
        "Icon size must be between 1 and 2048 pixels"
    );
    Ok(imageops::resize(master, size, size, FilterType::Lanczos3))
}
fn app_image(master: &RgbaImage, size: u32) -> Result<RgbaImage> {
    let mark = resized(master, size)?;
    let mut canvas = RgbaImage::from_pixel(size, size, Rgba(BACKGROUND));
    imageops::overlay(&mut canvas, &mark, 0, 0);
    Ok(canvas)
}
fn template(master: &RgbaImage) -> Result<RgbaImage> {
    let mut pixels = resized(master, 24)?;
    for pixel in pixels.pixels_mut() {
        pixel.0 = [0, 0, 0, pixel.0[3]];
    }
    Ok(pixels)
}
fn png(image: &RgbaImage) -> Result<Vec<u8>> {
    let mut output = Cursor::new(Vec::new());
    image.write_to(&mut output, ImageFormat::Png)?;
    Ok(output.into_inner())
}
fn ico(png: &[u8]) -> Result<Vec<u8>> {
    let mut output = Vec::with_capacity(22 + png.len());
    output.extend([0, 0, 1, 0, 1, 0]); // One PNG-backed 256 px RGBA image.
    output.extend([0, 0, 0, 0]);
    output.extend(1u16.to_le_bytes());
    output.extend(32u16.to_le_bytes());
    output.extend(u32::try_from(png.len())?.to_le_bytes());
    output.extend(22u32.to_le_bytes());
    output.extend(png);
    Ok(output)
}
fn icns(master: &RgbaImage) -> Result<Vec<u8>> {
    let mut chunks = Vec::new();
    for (size, kind) in ICNS_SIZES {
        let bytes = png(&app_image(master, size)?)?;
        chunks.extend(kind);
        chunks.extend(u32::try_from(bytes.len() + 8)?.to_be_bytes());
        chunks.extend(bytes);
    }
    let mut output = Vec::with_capacity(chunks.len() + 8);
    output.extend(b"icns");
    output.extend(u32::try_from(chunks.len() + 8)?.to_be_bytes());
    output.extend(chunks);
    Ok(output)
}
fn export(directory: &Path) -> Result<Value> {
    let master = master()?;
    std::fs::create_dir_all(directory)?;
    let variants = [
        ("app.png", "png", vec![512], png(&app_image(&master, 512)?)?),
        (
            "app.ico",
            "ico",
            vec![256],
            ico(&png(&app_image(&master, 256)?)?)?,
        ),
        (
            "app.icns",
            "icns",
            ICNS_SIZES.iter().map(|(size, _)| *size).collect(),
            icns(&master)?,
        ),
        ("sidebar.png", "png", vec![64], png(&resized(&master, 64)?)?),
        ("tray.png", "png", vec![24], png(&template(&master)?)?),
    ];
    let mut outputs = Vec::new();
    for (path, format, sizes, bytes) in variants {
        std::fs::write(directory.join(path), &bytes).with_context(|| format!("Write {path}"))?;
        outputs.push(json!({
            "path": path,
            "format": format,
            "sizes": sizes,
            "bytes": bytes.len(),
            "sha256": hash(&bytes),
        }));
    }
    let manifest = json!({
        "schema": 1,
        "master": {
            "path": MASTER_PATH,
            "sha256": hash(MASTER),
            "bytes": MASTER.len(),
            "width": master.width(),
            "height": master.height(),
        },
        "conversion": {
            "filter": "Lanczos3",
            "codec": "image 0.25.10 PNG",
            "app_background": "warm white",
            "tray": "master alpha, monochrome template",
            "master_changed": false,
        },
        "outputs": outputs,
    });
    let mut bytes = serde_json::to_vec_pretty(&manifest)?;
    bytes.push(b'\n');
    std::fs::write(directory.join("manifest.json"), bytes)?;
    Ok(manifest)
}
fn main() -> Result<()> {
    let mut args = std::env::args_os().skip(1);
    let mode = args
        .next()
        .context("Use --master-sha256, --png SIZE, or --output DIRECTORY")?;
    if mode == "--master-sha256" {
        ensure!(args.next().is_none(), "Unexpected icon arguments");
        println!("{}", hash(MASTER));
        return Ok(());
    }
    let value = args.next().context("Icon option requires a value")?;
    ensure!(args.next().is_none(), "Unexpected icon arguments");
    if mode == "--png" {
        let size = value
            .to_str()
            .context("Icon size must be an integer")?
            .parse()?;
        std::io::stdout()
            .lock()
            .write_all(&png(&app_image(&master()?, size)?)?)?;
    } else if mode == "--output" {
        println!(
            "{}",
            serde_json::to_string_pretty(&export(&PathBuf::from(value))?)?
        );
    } else {
        bail!("Unknown icon option; use --master-sha256, --png SIZE, or --output DIRECTORY");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn committed_master_has_transparency_and_native_size() {
        let original = master().unwrap();
        assert_eq!(original.dimensions(), (1254, 1254));
        assert!(original.pixels().any(|pixel| pixel.0[3] == 0));
        assert!(original.pixels().any(|pixel| pixel.0[3] == 255));
    }
    #[test]
    fn tray_preserves_the_scaled_master_alpha_and_sidebar_stays_transparent() {
        let original = master().unwrap();
        let scaled = resized(&original, 24).unwrap();
        let tray = template(&original).unwrap();
        for (expected, actual) in scaled.pixels().zip(tray.pixels()) {
            assert_eq!(expected.0[3], actual.0[3]);
            assert_eq!(actual.0[..3], [0, 0, 0]);
        }
        let sidebar = resized(&original, 64).unwrap();
        assert_eq!(sidebar.get_pixel(0, 0).0[3], 0);
    }
    #[test]
    fn deterministic_png_ico_and_icns_containers_decode_original_derivatives() {
        let original = master().unwrap();
        let small = png(&app_image(&original, 256).unwrap()).unwrap();
        assert_eq!(small, png(&app_image(&original, 256).unwrap()).unwrap());
        let windows = ico(&small).unwrap();
        assert_eq!(&windows[..6], &[0, 0, 1, 0, 1, 0]);
        assert_eq!(&windows[22..], small);
        assert_eq!(
            image::load_from_memory(&windows[22..]).unwrap().width(),
            256
        );
        let mac = icns(&original).unwrap();
        assert_eq!(&mac[..4], b"icns");
        assert_eq!(
            u32::from_be_bytes(mac[4..8].try_into().unwrap()) as usize,
            mac.len()
        );
        let mut offset = 8;
        for (size, kind) in ICNS_SIZES {
            assert_eq!(&mac[offset..offset + 4], kind);
            let length =
                u32::from_be_bytes(mac[offset + 4..offset + 8].try_into().unwrap()) as usize;
            let image = image::load_from_memory(&mac[offset + 8..offset + length]).unwrap();
            assert_eq!((image.width(), image.height()), (size, size));
            offset += length;
        }
        assert_eq!(offset, mac.len());
    }
}
