//! Turning a dropped image into a sticker, as Desktop's sticker pack creator
//! does (sticker-creator/src/util/processImage.ts, util/apng.ts): an animated
//! PNG is kept as it is if it fits the rules; anything else is drawn onto a
//! 512px transparent square and encoded as lossy WebP at quality 80.

use signal_backend::sticker_upload::{MAX_STICKER_BYTES, StickerImage};

/// Desktop's `STICKER_SIZE`.
const STICKER_SIZE: u32 = 512;
/// Desktop's `MIN_IMAGE_SIZE`, for animated PNGs.
const MIN_APNG_SIZE: u32 = 10;
/// How far into a PNG Desktop looks for `acTL`.
const APNG_SCAN: usize = 1024 * 1024;
const PNG_SIGNATURE: &[u8] = b"\x89PNG\r\n\x1a\n";

/// Extensions the creator accepts (Desktop: PNG, APNG, WebP).
pub const EXTENSIONS: [&str; 3] = ["png", "apng", "webp"];

pub fn accepts(path: &std::path::Path) -> bool {
    path.extension().and_then(|e| e.to_str()).is_some_and(|e| EXTENSIONS.contains(&e.to_ascii_lowercase().as_str()))
}

/// Why an image can't be a sticker, in Desktop's words.
#[derive(Debug, Clone, PartialEq)]
pub enum Rejected {
    Unsupported,
    Unreadable,
    TooLarge,
    ApngNotSquare,
    ApngTooSmall,
    ApngTooBig,
    ApngDoesNotLoop,
}

impl Rejected {
    pub fn message(&self) -> &'static str {
        match self {
            Rejected::Unsupported => "Can't add this image because the format is not supported",
            Rejected::Unreadable => "Error processing image",
            Rejected::TooLarge => "Dropped image is too large",
            Rejected::ApngNotSquare => "Animated PNG images must be square",
            Rejected::ApngTooSmall => "Animated PNG image dimensions are too small",
            Rejected::ApngTooBig => "Animated PNG image dimensions are too large",
            Rejected::ApngDoesNotLoop => "Animated images must loop forever",
        }
    }
}

pub fn process(path: &std::path::Path, bytes: &[u8]) -> Result<StickerImage, Rejected> {
    if !accepts(path) {
        return Err(Rejected::Unsupported);
    }
    if let Some(apng) = apng(bytes) {
        return check_apng(&apng, bytes.len()).map(|()| StickerImage { bytes: bytes.to_vec(), content_type: "image/png" });
    }
    let image = image::load_from_memory(bytes).map_err(|_| Rejected::Unreadable)?;
    let webp = fit(&image)?;
    if webp.len() > MAX_STICKER_BYTES {
        return Err(Rejected::TooLarge);
    }
    Ok(StickerImage { bytes: webp, content_type: "image/webp" })
}

/// Scaled so the longer side is 512 (up or down), centered on a transparent
/// 512px square, as lossy WebP.
fn fit(image: &image::DynamicImage) -> Result<Vec<u8>, Rejected> {
    let (width, height) = (image.width(), image.height());
    if width == 0 || height == 0 {
        return Err(Rejected::Unreadable);
    }
    let scale = STICKER_SIZE as f64 / width.max(height) as f64;
    let (w, h) = (
        ((width as f64 * scale).round() as u32).clamp(1, STICKER_SIZE),
        ((height as f64 * scale).round() as u32).clamp(1, STICKER_SIZE),
    );
    let resized = image::imageops::resize(&image.to_rgba8(), w, h, image::imageops::FilterType::Lanczos3);
    let mut canvas = image::RgbaImage::new(STICKER_SIZE, STICKER_SIZE);
    image::imageops::overlay(&mut canvas, &resized, ((STICKER_SIZE - w) / 2).into(), ((STICKER_SIZE - h) / 2).into());
    Ok(webp::Encoder::from_rgba(canvas.as_raw(), STICKER_SIZE, STICKER_SIZE).encode(80.).to_vec())
}

/// An animated PNG's width, height and play count, from `IHDR` and an
/// `acTL` that comes before the first `IDAT` within the first megabyte.
#[derive(Debug, PartialEq)]
struct Apng {
    width: u32,
    height: u32,
    plays: u32,
}

fn apng(bytes: &[u8]) -> Option<Apng> {
    let bytes = &bytes[..bytes.len().min(APNG_SCAN)];
    let mut rest = bytes.strip_prefix(PNG_SIGNATURE)?;
    let u32_at = |b: &[u8], at: usize| b.get(at..at + 4).map(|s| u32::from_be_bytes(s.try_into().unwrap()));
    let mut size = None;
    while rest.len() >= 8 {
        let len = u32_at(rest, 0)? as usize;
        let (kind, data) = (&rest[4..8], rest.get(8..8usize.checked_add(len)?)?);
        match kind {
            b"IHDR" => size = Some((u32_at(data, 0)?, u32_at(data, 4)?)),
            b"acTL" => {
                let (width, height) = size?;
                return Some(Apng { width, height, plays: u32_at(data, 4)? });
            }
            b"IDAT" => return None,
            _ => {}
        }
        // Length, type, data, CRC.
        rest = rest.get(12usize.checked_add(len)?..)?;
    }
    None
}

fn check_apng(apng: &Apng, size: usize) -> Result<(), Rejected> {
    if apng.width != apng.height {
        return Err(Rejected::ApngNotSquare);
    }
    if size > MAX_STICKER_BYTES {
        return Err(Rejected::TooLarge);
    }
    if apng.width < MIN_APNG_SIZE {
        return Err(Rejected::ApngTooSmall);
    }
    if apng.width > STICKER_SIZE {
        return Err(Rejected::ApngTooBig);
    }
    if apng.plays != 0 {
        return Err(Rejected::ApngDoesNotLoop);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::path::Path;

    use super::*;

    fn chunk(kind: &[u8], data: &[u8]) -> Vec<u8> {
        let mut out = (data.len() as u32).to_be_bytes().to_vec();
        out.extend(kind);
        out.extend(data);
        out.extend([0; 4]);
        out
    }

    fn png_chunks(width: u32, height: u32, actl: Option<u32>) -> Vec<u8> {
        let mut ihdr = width.to_be_bytes().to_vec();
        ihdr.extend(height.to_be_bytes());
        ihdr.extend([8, 6, 0, 0, 0]);
        let mut out = PNG_SIGNATURE.to_vec();
        out.extend(chunk(b"IHDR", &ihdr));
        if let Some(plays) = actl {
            let mut data = 2u32.to_be_bytes().to_vec();
            data.extend(plays.to_be_bytes());
            out.extend(chunk(b"acTL", &data));
        }
        out.extend(chunk(b"IDAT", &[]));
        out
    }

    #[test]
    fn animated_pngs_are_found_by_actl_before_idat() {
        assert_eq!(apng(&png_chunks(64, 64, Some(0))), Some(Apng { width: 64, height: 64, plays: 0 }));
        assert_eq!(apng(&png_chunks(64, 64, None)), None, "a still PNG");
        assert_eq!(apng(b"RIFF....WEBP"), None);
    }

    #[test]
    fn animated_pngs_follow_desktops_rules() {
        let check = |w, h, plays, size| check_apng(&Apng { width: w, height: h, plays }, size);
        assert_eq!(check(512, 512, 0, 1000), Ok(()));
        assert_eq!(check(512, 500, 0, 1000), Err(Rejected::ApngNotSquare));
        assert_eq!(check(9, 9, 0, 1000), Err(Rejected::ApngTooSmall));
        assert_eq!(check(600, 600, 0, 1000), Err(Rejected::ApngTooBig));
        assert_eq!(check(64, 64, 3, 1000), Err(Rejected::ApngDoesNotLoop));
        assert_eq!(check(64, 64, 0, MAX_STICKER_BYTES + 1), Err(Rejected::TooLarge));
    }

    #[test]
    fn still_images_become_centered_512px_webp() {
        // A wide red bar: scaled up to 512 across, centered top to bottom.
        let image = image::DynamicImage::ImageRgba8(image::RgbaImage::from_pixel(100, 50, image::Rgba([255, 0, 0, 255])));
        let mut png = Vec::new();
        image.write_to(&mut std::io::Cursor::new(&mut png), image::ImageFormat::Png).unwrap();
        let sticker = process(Path::new("bar.png"), &png).unwrap();
        assert_eq!(sticker.content_type, "image/webp");
        let decoded = image::load_from_memory(&sticker.bytes).unwrap().to_rgba8();
        assert_eq!(decoded.dimensions(), (512, 512));
        assert_eq!(decoded.get_pixel(256, 10)[3], 0, "transparent above the bar");
        let middle = decoded.get_pixel(256, 256);
        assert!(middle[0] > 200 && middle[3] > 200, "{middle:?}");
    }

    #[test]
    fn other_formats_are_turned_away() {
        assert_eq!(process(Path::new("photo.jpg"), b"..."), Err(Rejected::Unsupported));
        assert_eq!(process(Path::new("broken.png"), b"not a png"), Err(Rejected::Unreadable));
        assert!(accepts(Path::new("A.PNG")) && accepts(Path::new("a.apng")) && accepts(Path::new("a.webp")));
    }
}
