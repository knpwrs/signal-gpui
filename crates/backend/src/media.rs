//! Preparing images to send: lossless optimization and metadata stripping.
//!
//! - PNG: oxipng at its maximum preset (`-o max`), keeping only chunks that
//!   affect how the image looks (colour profile, gamma, physical size).
//! - JPEG: mozjpeg's lossless transcode, as `jpegtran -optimize -progressive`
//!   does: the DCT coefficients are copied untouched, so the pixels are
//!   identical; only the entropy coding is re-optimized. The ICC profile and
//!   the EXIF orientation are kept (dropping either would change how the
//!   photo looks); everything else, including GPS, is dropped.
//! - WebP: the EXIF and XMP chunks are removed; the image data is untouched.
//! - GIF is sent as it is.

use std::panic::AssertUnwindSafe;

use anyhow::{Context as _, anyhow, bail};

/// An image ready to upload.
#[derive(Debug, Clone, PartialEq)]
pub struct PreparedImage {
    pub bytes: Vec<u8>,
    pub content_type: &'static str,
    pub extension: &'static str,
    /// As displayed (EXIF rotation applied).
    pub width: u32,
    pub height: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ImageKind {
    Png,
    Jpeg,
    Gif,
    Webp,
}

impl ImageKind {
    /// From the file's signature, not its name.
    pub fn sniff(bytes: &[u8]) -> Option<Self> {
        match bytes {
            [0x89, b'P', b'N', b'G', ..] => Some(Self::Png),
            [0xFF, 0xD8, 0xFF, ..] => Some(Self::Jpeg),
            [b'G', b'I', b'F', b'8', ..] => Some(Self::Gif),
            [b'R', b'I', b'F', b'F', _, _, _, _, b'W', b'E', b'B', b'P', ..] => Some(Self::Webp),
            _ => None,
        }
    }

    pub fn content_type(self) -> &'static str {
        match self {
            Self::Png => "image/png",
            Self::Jpeg => "image/jpeg",
            Self::Gif => "image/gif",
            Self::Webp => "image/webp",
        }
    }

    pub fn extension(self) -> &'static str {
        match self {
            Self::Png => "png",
            Self::Jpeg => "jpg",
            Self::Gif => "gif",
            Self::Webp => "webp",
        }
    }
}

/// Converts formats Signal clients can't show (TIFF, BMP — macOS often puts
/// a copied image on the clipboard as TIFF) to PNG, losslessly. Other
/// formats pass through untouched.
pub fn normalize(bytes: Vec<u8>) -> anyhow::Result<Vec<u8>> {
    let convertible = bytes.starts_with(b"II*\0") || bytes.starts_with(b"MM\0*") || bytes.starts_with(b"BM");
    if !convertible {
        return Ok(bytes);
    }
    let image = image::load_from_memory(&bytes).context("reading image")?;
    let mut png = Vec::new();
    image
        .write_to(&mut std::io::Cursor::new(&mut png), image::ImageFormat::Png)
        .context("converting image to PNG")?;
    Ok(png)
}

/// Cheap check before optimizing: the kind and displayed size, or `None` if
/// this isn't an image we can send.
pub fn probe(bytes: &[u8]) -> Option<(ImageKind, u32, u32)> {
    let kind = ImageKind::sniff(bytes)?;
    let size = imagesize::blob_size(bytes).ok()?;
    let (width, height) = (size.width as u32, size.height as u32);
    let quarter_turn = kind == ImageKind::Jpeg && matches!(jpeg_orientation(bytes), Some(5..=8));
    Some(if quarter_turn { (kind, height, width) } else { (kind, width, height) })
}

/// The EXIF orientation of a JPEG, read from its APP1 segment without
/// decoding anything.
fn jpeg_orientation(bytes: &[u8]) -> Option<u16> {
    let mut at = 2;
    while at + 4 <= bytes.len() && bytes[at] == 0xFF {
        let marker = bytes[at + 1];
        let len = u16::from_be_bytes([bytes[at + 2], bytes[at + 3]]) as usize;
        let data = bytes.get(at + 4..at + 2 + len)?;
        if marker == 0xE1 && data.starts_with(EXIF_SIGNATURE) {
            return exif_orientation(&data[EXIF_SIGNATURE.len()..]);
        }
        // Metadata comes before the image data; stop at start-of-scan.
        if marker == 0xDA {
            return None;
        }
        at += 2 + len;
    }
    None
}

/// How hard to work on images before sending. Every level is lossless and
/// removes metadata; only the time spent shrinking the file differs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Optimization {
    /// oxipng `-o max`; mozjpeg with searched progressive scans.
    #[default]
    Max,
    /// oxipng preset 2; mozjpeg with optimized Huffman tables only.
    Fast,
    /// Rewrite only as far as needed to drop metadata.
    Off,
}

impl Optimization {
    pub fn as_setting(self) -> &'static str {
        match self {
            Self::Max => "max",
            Self::Fast => "fast",
            Self::Off => "off",
        }
    }

    /// Unknown or missing values mean the default.
    pub fn from_setting(value: Option<&str>) -> Self {
        match value {
            Some("fast") => Self::Fast,
            Some("off") => Self::Off,
            _ => Self::Max,
        }
    }
}

/// Optimizes `bytes` losslessly. CPU-heavy (seconds for large PNGs at
/// `Max`): call it off the async runtime.
pub fn prepare(bytes: &[u8], level: Optimization) -> anyhow::Result<PreparedImage> {
    let kind = ImageKind::sniff(bytes).context("not a PNG, JPEG, GIF or WebP image")?;
    let size = imagesize::blob_size(bytes).map_err(|e| anyhow!("can't read image size: {e}"))?;
    let (width, height) = (size.width as u32, size.height as u32);
    let (bytes, rotated) = match kind {
        ImageKind::Png => (optimize_png(bytes, level)?, false),
        ImageKind::Jpeg => {
            let (bytes, orientation) = optimize_jpeg(bytes, level)?;
            // Orientations 5–8 turn the image a quarter turn.
            (bytes, matches!(orientation, Some(5..=8)))
        }
        ImageKind::Webp => (strip_webp_metadata(bytes)?, false),
        ImageKind::Gif => (bytes.to_vec(), false),
    };
    let (width, height) = if rotated { (height, width) } else { (width, height) };
    Ok(PreparedImage { bytes, content_type: kind.content_type(), extension: kind.extension(), width, height })
}

/// Drops a WebP's `EXIF` and `XMP ` chunks (which can hold GPS), clearing
/// their flags in the `VP8X` header. Every other chunk is copied as is.
fn strip_webp_metadata(bytes: &[u8]) -> anyhow::Result<Vec<u8>> {
    const EXIF_FLAG: u8 = 0x08;
    const XMP_FLAG: u8 = 0x04;
    if bytes.len() < 12 || &bytes[..4] != b"RIFF" || &bytes[8..12] != b"WEBP" {
        bail!("not a WebP file");
    }
    let mut out = bytes[..12].to_vec();
    let mut rest = &bytes[12..];
    while !rest.is_empty() {
        if rest.len() < 8 {
            bail!("truncated WebP chunk");
        }
        let size = u32::from_le_bytes(rest[4..8].try_into().expect("4 bytes")) as usize;
        // Chunks are padded to an even length.
        let end = 8 + size + (size & 1);
        let chunk = rest.get(..end).or_else(|| rest.get(..8 + size)).context("truncated WebP chunk")?;
        match &chunk[..4] {
            b"EXIF" | b"XMP " => {}
            b"VP8X" if size >= 1 => {
                let start = out.len();
                out.extend_from_slice(chunk);
                out[start + 8] &= !(EXIF_FLAG | XMP_FLAG);
            }
            _ => out.extend_from_slice(chunk),
        }
        rest = &rest[chunk.len()..];
    }
    let riff_size = u32::try_from(out.len() - 8).context("WebP too large")?;
    out[4..8].copy_from_slice(&riff_size.to_le_bytes());
    Ok(out)
}

fn optimize_png(bytes: &[u8], level: Optimization) -> anyhow::Result<Vec<u8>> {
    let mut options = match level {
        Optimization::Max => oxipng::Options::max_compression(),
        Optimization::Fast => oxipng::Options::from_preset(2),
        Optimization::Off => oxipng::Options::from_preset(0),
    };
    options.strip = oxipng::StripChunks::Safe;
    // Keep the result even when it isn't smaller, so metadata is always gone.
    options.force = true;
    oxipng::optimize_from_memory(bytes, &options).map_err(|e| anyhow!("optimizing PNG: {e}"))
}

/// Returns the transcoded JPEG and the source's EXIF orientation.
fn optimize_jpeg(bytes: &[u8], level: Optimization) -> anyhow::Result<(Vec<u8>, Option<u16>)> {
    // libjpeg reports errors through `error_exit`, which must not return; it
    // panics instead and the panic is caught here (mozjpeg-sys is built with
    // unwinding support for exactly this).
    match std::panic::catch_unwind(AssertUnwindSafe(|| unsafe { transcode_jpeg(bytes, level) })) {
        Ok(result) => Ok(result),
        Err(panic) => {
            let message = panic
                .downcast_ref::<String>()
                .cloned()
                .or_else(|| panic.downcast_ref::<&str>().map(|s| s.to_string()))
                .unwrap_or_else(|| "unknown error".into());
            bail!("optimizing JPEG: {message}")
        }
    }
}

unsafe extern "C-unwind" fn jpeg_error_exit(cinfo: &mut mozjpeg_sys::jpeg_common_struct) {
    let mut buffer = [0u8; 80]; // JMSG_LENGTH_MAX
    unsafe {
        if let Some(format) = (*cinfo.err).format_message {
            // libjpeg writes the message into the buffer, which mozjpeg-sys
            // declares as a shared reference; call it with a pointer instead.
            type Format = unsafe extern "C-unwind" fn(&mut mozjpeg_sys::jpeg_common_struct, *mut [u8; 80]);
            let format: Format = std::mem::transmute(format);
            format(cinfo, &mut buffer);
        }
    }
    let message = std::ffi::CStr::from_bytes_until_nul(&buffer)
        .map(|m| m.to_string_lossy().into_owned())
        .unwrap_or_default();
    std::panic::panic_any(message);
}

unsafe extern "C-unwind" fn jpeg_quiet(_: &mut mozjpeg_sys::jpeg_common_struct) {}

const APP1: i32 = 0xE1;
const APP2: i32 = 0xE2;
const ICC_SIGNATURE: &[u8] = b"ICC_PROFILE\0";
const EXIF_SIGNATURE: &[u8] = b"Exif\0\0";

/// libjpeg state for one transcode. Boxed so the pointers libjpeg keeps
/// between its structs stay valid; dropped (and destroyed) even when an error
/// unwinds out of libjpeg.
struct JpegSession {
    src: Box<mozjpeg_sys::jpeg_decompress_struct>,
    dst: Box<mozjpeg_sys::jpeg_compress_struct>,
    _src_err: Box<mozjpeg_sys::jpeg_error_mgr>,
    _dst_err: Box<mozjpeg_sys::jpeg_error_mgr>,
    out: *mut u8,
    out_len: std::os::raw::c_ulong,
    /// Only after `jpeg_finish_compress` does `out` own the final buffer;
    /// before that libjpeg may already have freed it. An error midway leaks
    /// the partial buffer rather than risk a double free.
    finished: bool,
}

impl JpegSession {
    unsafe fn new() -> Self {
        use mozjpeg_sys::*;
        unsafe {
            let mut src_err: Box<jpeg_error_mgr> = Box::new(std::mem::zeroed());
            let mut dst_err: Box<jpeg_error_mgr> = Box::new(std::mem::zeroed());
            let mut src: Box<jpeg_decompress_struct> = Box::new(std::mem::zeroed());
            let mut dst: Box<jpeg_compress_struct> = Box::new(std::mem::zeroed());
            src.common.err = jpeg_std_error(&mut src_err);
            src_err.error_exit = Some(jpeg_error_exit);
            src_err.output_message = Some(jpeg_quiet);
            dst.common.err = jpeg_std_error(&mut dst_err);
            dst_err.error_exit = Some(jpeg_error_exit);
            dst_err.output_message = Some(jpeg_quiet);
            jpeg_create_decompress(&mut *src);
            jpeg_create_compress(&mut *dst);
            Self { src, dst, _src_err: src_err, _dst_err: dst_err, out: std::ptr::null_mut(), out_len: 0, finished: false }
        }
    }
}

impl Drop for JpegSession {
    fn drop(&mut self) {
        unsafe {
            mozjpeg_sys::jpeg_destroy_compress(&mut self.dst);
            mozjpeg_sys::jpeg_destroy_decompress(&mut self.src);
            if self.finished && !self.out.is_null() {
                libc_free(self.out.cast());
            }
        }
    }
}

unsafe fn transcode_jpeg(bytes: &[u8], level: Optimization) -> (Vec<u8>, Option<u16>) {
    use mozjpeg_sys::*;
    unsafe {
        let mut s = JpegSession::new();
        jpeg_mem_src(&mut s.src, bytes.as_ptr(), bytes.len() as _);
        jpeg_save_markers(&mut s.src, APP1, 0xFFFF);
        jpeg_save_markers(&mut s.src, APP2, 0xFFFF);
        jpeg_read_header(&mut s.src, 1);
        let coefficients = jpeg_read_coefficients(&mut s.src);

        // At Max, mozjpeg's strongest settings: optimized Huffman tables and
        // searched progressive scans. None of the levels touch the
        // coefficients.
        let profile = if level == Optimization::Max { JCP_MAX_COMPRESSION } else { JCP_FASTEST };
        jpeg_c_set_int_param(&mut s.dst, J_INT_PARAM::JINT_COMPRESS_PROFILE, profile as i32);
        jpeg_copy_critical_parameters(&s.src, &mut s.dst);
        match level {
            Optimization::Max => {
                s.dst.optimize_coding = 1;
                jpeg_c_set_bool_param(&mut s.dst, J_BOOLEAN_PARAM::JBOOLEAN_OPTIMIZE_SCANS, 1);
                jpeg_simple_progression(&mut s.dst);
            }
            Optimization::Fast => s.dst.optimize_coding = 1,
            Optimization::Off => {}
        }
        let (out, out_len) = (&raw mut s.out, &raw mut s.out_len);
        jpeg_mem_dest(&mut s.dst, out, out_len);
        jpeg_write_coefficients(&mut s.dst, coefficients);

        let mut orientation = None;
        let mut marker = s.src.marker_list;
        while !marker.is_null() {
            let m = &*marker;
            let data = std::slice::from_raw_parts(m.data, m.data_length as usize);
            match i32::from(m.marker) {
                APP1 if data.starts_with(EXIF_SIGNATURE) => {
                    orientation = exif_orientation(&data[EXIF_SIGNATURE.len()..]);
                }
                APP2 if data.starts_with(ICC_SIGNATURE) => {
                    jpeg_write_marker(&mut s.dst, APP2, data.as_ptr(), data.len() as _);
                }
                _ => {}
            }
            marker = m.next;
        }
        if let Some(orientation) = orientation.filter(|o| *o != 1) {
            let exif = minimal_exif(orientation);
            jpeg_write_marker(&mut s.dst, APP1, exif.as_ptr(), exif.len() as _);
        }

        jpeg_finish_compress(&mut s.dst);
        s.finished = true;
        jpeg_finish_decompress(&mut s.src);
        (std::slice::from_raw_parts(s.out, s.out_len as usize).to_vec(), orientation)
    }
}

unsafe extern "C" {
    #[link_name = "free"]
    fn libc_free(ptr: *mut std::ffi::c_void);
}

/// The orientation tag (0x0112) from IFD0 of a TIFF-structured EXIF block.
fn exif_orientation(tiff: &[u8]) -> Option<u16> {
    let big_endian = match tiff.get(..2)? {
        b"MM" => true,
        b"II" => false,
        _ => return None,
    };
    let u16_at = |at: usize| -> Option<u16> {
        let b: [u8; 2] = tiff.get(at..at + 2)?.try_into().ok()?;
        Some(if big_endian { u16::from_be_bytes(b) } else { u16::from_le_bytes(b) })
    };
    let u32_at = |at: usize| -> Option<u32> {
        let b: [u8; 4] = tiff.get(at..at + 4)?.try_into().ok()?;
        Some(if big_endian { u32::from_be_bytes(b) } else { u32::from_le_bytes(b) })
    };
    let ifd = u32_at(4)? as usize;
    let count = u16_at(ifd)? as usize;
    (0..count).find_map(|i| {
        let entry = ifd + 2 + i * 12;
        (u16_at(entry)? == 0x0112).then(|| u16_at(entry + 8)).flatten()
    })
}

/// An EXIF block holding only the orientation tag.
fn minimal_exif(orientation: u16) -> Vec<u8> {
    let mut exif = EXIF_SIGNATURE.to_vec();
    exif.extend_from_slice(b"MM\0\x2a\0\0\0\x08"); // big-endian TIFF, IFD0 at 8
    exif.extend_from_slice(&1u16.to_be_bytes()); // one entry
    exif.extend_from_slice(&0x0112u16.to_be_bytes()); // Orientation
    exif.extend_from_slice(&3u16.to_be_bytes()); // SHORT
    exif.extend_from_slice(&1u32.to_be_bytes()); // count
    exif.extend_from_slice(&orientation.to_be_bytes());
    exif.extend_from_slice(&[0, 0]); // padding to 4 bytes
    exif.extend_from_slice(&0u32.to_be_bytes()); // no next IFD
    exif
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn webp_metadata_is_removed() {
        fn chunk(fourcc: &[u8; 4], data: &[u8]) -> Vec<u8> {
            let mut c = fourcc.to_vec();
            c.extend_from_slice(&(data.len() as u32).to_le_bytes());
            c.extend_from_slice(data);
            if data.len() % 2 == 1 {
                c.push(0);
            }
            c
        }
        let vp8x = chunk(b"VP8X", &[0x08 | 0x04 | 0x10, 0, 0, 0, 9, 0, 0, 9, 0, 0]);
        let image = chunk(b"VP8L", &[1, 2, 3]);
        let body = [vp8x.clone(), image.clone(), chunk(b"EXIF", b"GPS here"), chunk(b"XMP ", b"<x/>")].concat();
        let mut webp = b"RIFF".to_vec();
        webp.extend_from_slice(&((body.len() + 4) as u32).to_le_bytes());
        webp.extend_from_slice(b"WEBP");
        webp.extend_from_slice(&body);

        let out = strip_webp_metadata(&webp).unwrap();
        let mut expected_vp8x = vp8x;
        expected_vp8x[8] = 0x10; // alpha flag kept
        assert_eq!(&out[12..], [expected_vp8x, image].concat().as_slice());
        assert_eq!(u32::from_le_bytes(out[4..8].try_into().unwrap()) as usize, out.len() - 8);
        assert!(!out.windows(3).any(|w| w == b"GPS"));
        assert!(strip_webp_metadata(&webp[..20]).is_err());
    }

    fn gradient(width: u32, height: u32) -> Vec<u8> {
        (0..height)
            .flat_map(|y| (0..width).flat_map(move |x| [(x * 255 / width) as u8, (y * 255 / height) as u8, 128]))
            .collect()
    }

    fn encode_png(width: u32, height: u32, text: Option<&str>) -> Vec<u8> {
        let mut out = Vec::new();
        let mut encoder = png::Encoder::new(&mut out, width, height);
        encoder.set_color(png::ColorType::Rgb);
        encoder.set_depth(png::BitDepth::Eight);
        encoder.set_compression(png::Compression::Fast);
        if let Some(text) = text {
            encoder.add_text_chunk("Comment".into(), text.into()).unwrap();
        }
        encoder.write_header().unwrap().write_image_data(&gradient(width, height)).unwrap();
        out
    }

    fn decode_png(bytes: &[u8]) -> Vec<u8> {
        // oxipng may switch to a palette or lower bit depth; expand back to
        // 8-bit samples so pixels compare directly.
        let mut decoder = png::Decoder::new(std::io::Cursor::new(bytes));
        decoder.set_transformations(png::Transformations::EXPAND);
        let mut reader = decoder.read_info().unwrap();
        let mut pixels = vec![0; reader.output_buffer_size().unwrap()];
        let info = reader.next_frame(&mut pixels).unwrap();
        pixels.truncate(info.buffer_size());
        pixels
    }

    fn contains(haystack: &[u8], needle: &[u8]) -> bool {
        haystack.windows(needle.len()).any(|w| w == needle)
    }

    #[test]
    fn png_is_smaller_identical_and_stripped() {
        let original = encode_png(64, 48, Some("taken at 47.6N 122.3W"));
        let prepared = prepare(&original, Optimization::Max).unwrap();
        assert_eq!((prepared.content_type, prepared.width, prepared.height), ("image/png", 64, 48));
        assert!(prepared.bytes.len() < original.len(), "{} >= {}", prepared.bytes.len(), original.len());
        assert!(!contains(&prepared.bytes, b"47.6N"));
        assert_eq!(decode_png(&prepared.bytes), gradient(64, 48));
    }

    fn encode_jpeg(width: u16, height: u16) -> Vec<u8> {
        let mut out = Vec::new();
        jpeg_encoder::Encoder::new(&mut out, 90)
            .encode(&gradient(width.into(), height.into()), width, height, jpeg_encoder::ColorType::Rgb)
            .unwrap();
        out
    }

    /// Splices an APP1 EXIF block (orientation + a fake GPS note) and an
    /// ICC-style APP2 block in after SOI.
    fn with_metadata(jpeg: &[u8], orientation: u16) -> Vec<u8> {
        let mut exif = minimal_exif(orientation);
        exif.extend_from_slice(b"GPS 47.6N 122.3W");
        let mut icc = ICC_SIGNATURE.to_vec();
        icc.extend_from_slice(&[1, 1]);
        icc.extend_from_slice(b"fake profile");
        let mut out = jpeg[..2].to_vec();
        for (marker, data) in [(0xE1u8, exif), (0xE2, icc)] {
            out.extend_from_slice(&[0xFF, marker]);
            out.extend_from_slice(&((data.len() + 2) as u16).to_be_bytes());
            out.extend_from_slice(&data);
        }
        out.extend_from_slice(&jpeg[2..]);
        out
    }

    fn decode_jpeg(bytes: &[u8]) -> Vec<u8> {
        jpeg_decoder::Decoder::new(bytes).decode().unwrap()
    }

    #[test]
    fn jpeg_is_lossless_and_keeps_only_icc_and_orientation() {
        let original = with_metadata(&encode_jpeg(64, 48), 6);
        let prepared = prepare(&original, Optimization::Max).unwrap();
        assert_eq!(prepared.content_type, "image/jpeg");
        // Orientation 6 is a quarter turn: displayed portrait.
        assert_eq!((prepared.width, prepared.height), (48, 64));
        assert_eq!(probe(&original), Some((ImageKind::Jpeg, 48, 64)));
        assert_eq!(decode_jpeg(&prepared.bytes), decode_jpeg(&original));
        assert!(!contains(&prepared.bytes, b"47.6N"));
        assert!(contains(&prepared.bytes, b"fake profile"));
        let tiff = prepared.bytes.windows(6).position(|w| w == EXIF_SIGNATURE).unwrap() + 6;
        assert_eq!(exif_orientation(&prepared.bytes[tiff..]), Some(6));
        // Progressive (SOF2) now.
        assert!(contains(&prepared.bytes, &[0xFF, 0xC2]));
    }

    #[test]
    fn every_level_is_lossless_and_strips_metadata() {
        let png = encode_png(64, 48, Some("taken at 47.6N 122.3W"));
        let jpeg = with_metadata(&encode_jpeg(64, 48), 6);
        for level in [Optimization::Max, Optimization::Fast, Optimization::Off] {
            let p = prepare(&png, level).unwrap();
            assert_eq!(decode_png(&p.bytes), gradient(64, 48), "{level:?}");
            assert!(!contains(&p.bytes, b"47.6N"), "{level:?}");
            let j = prepare(&jpeg, level).unwrap();
            assert_eq!(decode_jpeg(&j.bytes), decode_jpeg(&jpeg), "{level:?}");
            assert!(!contains(&j.bytes, b"47.6N") && contains(&j.bytes, b"fake profile"), "{level:?}");
        }
        assert_eq!(Optimization::from_setting(None), Optimization::Max);
        assert_eq!(Optimization::from_setting(Some(Optimization::Off.as_setting())), Optimization::Off);
    }

    #[test]
    fn corrupt_jpeg_is_an_error_not_a_crash() {
        let mut broken = encode_jpeg(32, 32);
        broken.truncate(40);
        assert!(prepare(&broken, Optimization::Max).is_err());
    }

    #[test]
    fn tiff_becomes_identical_png() {
        let pixels = gradient(16, 8);
        let mut tiff = Vec::new();
        image::RgbImage::from_raw(16, 8, pixels.clone())
            .unwrap()
            .write_to(&mut std::io::Cursor::new(&mut tiff), image::ImageFormat::Tiff)
            .unwrap();
        let png = normalize(tiff).unwrap();
        assert_eq!(ImageKind::sniff(&png), Some(ImageKind::Png));
        assert_eq!(decode_png(&prepare(&png, Optimization::Max).unwrap().bytes), pixels);
        // Already sendable: untouched.
        let original = encode_png(4, 4, None);
        assert_eq!(normalize(original.clone()).unwrap(), original);
    }

    #[test]
    fn other_files_are_rejected() {
        assert!(prepare(b"%PDF-1.7", Optimization::Max).is_err());
        assert_eq!(ImageKind::sniff(b"GIF89a...."), Some(ImageKind::Gif));
    }
}
