//! Build a multi-resolution Windows icon from the same PNG used by packaging.
//! PNG-compressed ICO frames are supported by Windows Vista and later.
use image::{ImageFormat, imageops::FilterType};
use std::io::Cursor;

const SIZES: [u32; 7] = [16, 24, 32, 48, 64, 128, 256];

pub fn windows_ico(source: &[u8]) -> Result<Vec<u8>, image::ImageError> {
    let source = image::load_from_memory_with_format(source, ImageFormat::Png)?;
    let mut frames = Vec::with_capacity(SIZES.len());
    for size in SIZES {
        let mut png = Cursor::new(Vec::new());
        source
            .resize_exact(size, size, FilterType::Lanczos3)
            .write_to(&mut png, ImageFormat::Png)?;
        frames.push(png.into_inner());
    }
    // ICONDIR followed by seven ICONDIRENTRY records and their PNG payloads.
    let mut ico = Vec::new();
    ico.extend_from_slice(&[0, 0, 1, 0]);
    ico.extend_from_slice(&(SIZES.len() as u16).to_le_bytes());
    let mut offset = 6 + 16 * SIZES.len() as u32;
    for (size, frame) in SIZES.into_iter().zip(&frames) {
        // In an ICO directory, zero encodes a dimension of 256 pixels.
        let dimension = if size == 256 { 0 } else { size as u8 };
        ico.extend_from_slice(&[dimension, dimension, 0, 0]);
        ico.extend_from_slice(&1_u16.to_le_bytes());
        ico.extend_from_slice(&32_u16.to_le_bytes());
        ico.extend_from_slice(&(frame.len() as u32).to_le_bytes());
        ico.extend_from_slice(&offset.to_le_bytes());
        offset += frame.len() as u32;
    }
    for frame in frames {
        ico.extend_from_slice(&frame);
    }
    Ok(ico)
}

#[cfg(test)]
mod tests {
    use super::*;
    const SOURCE: &[u8] = include_bytes!("assets/xylo-icon.png");

    #[test]
    fn windows_icon_contains_all_shell_and_taskbar_sizes_with_valid_offsets() {
        let ico = windows_ico(SOURCE).unwrap();
        assert_eq!(&ico[..6], &[0, 0, 1, 0, 7, 0]);
        let mut previous_end = 6 + 16 * 7;
        for (index, size) in [16, 24, 32, 48, 64, 128, 256].into_iter().enumerate() {
            let entry = &ico[6 + index * 16..6 + (index + 1) * 16];
            assert_eq!(entry[0], if size == 256 { 0 } else { size as u8 });
            assert_eq!(entry[1], entry[0]);
            assert_eq!(&entry[2..8], &[0, 0, 1, 0, 32, 0]);
            let length = u32::from_le_bytes(entry[8..12].try_into().unwrap()) as usize;
            let offset = u32::from_le_bytes(entry[12..16].try_into().unwrap()) as usize;
            assert_eq!(offset, previous_end);
            let frame = image::load_from_memory_with_format(
                &ico[offset..offset + length],
                ImageFormat::Png,
            )
            .unwrap()
            .to_rgba8();
            assert_eq!(frame.dimensions(), (size, size));
            assert!(
                frame.pixels().any(|pixel| pixel.0[3] > 0),
                "frame must be visible"
            );
            previous_end = offset + length;
        }
        assert_eq!(previous_end, ico.len());
    }

    #[test]
    fn windows_icon_rejects_invalid_source_instead_of_building_an_empty_icon() {
        assert!(windows_ico(b"not a PNG").is_err());
    }
}
