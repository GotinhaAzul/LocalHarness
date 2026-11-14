//! Image validation, dimensions, and metadata stripping for PNG, JPEG, and
//! WebP, without decoding pixels.
//!
//! Files are checked in a fixed order so that nothing large or untrusted is
//! read before it has to be: the size comes from metadata, the format from a
//! small header read (magic bytes, not the extension), and only then is the
//! body loaded. Metadata stripping rewrites the container and never touches
//! compressed image data.

use std::io::Read as _;
use std::path::Path;

/// Default cap on an attached image, in bytes.
pub const DEFAULT_MAX_BYTES: u64 = 5 * 1024 * 1024;

/// Bytes read to recognise a format and, for most files, its dimensions.
const HEADER_BYTES: usize = 64;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Format {
    Png,
    Jpeg,
    Webp,
}

impl Format {
    pub fn mime(self) -> &'static str {
        match self {
            Format::Png => "image/png",
            Format::Jpeg => "image/jpeg",
            Format::Webp => "image/webp",
        }
    }

    pub fn extension(self) -> &'static str {
        match self {
            Format::Png => "png",
            Format::Jpeg => "jpg",
            Format::Webp => "webp",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Format::Png => "PNG",
            Format::Jpeg => "JPEG",
            Format::Webp => "WebP",
        }
    }

    pub fn from_mime(mime: &str) -> Option<Self> {
        match mime {
            "image/png" => Some(Format::Png),
            "image/jpeg" => Some(Format::Jpeg),
            "image/webp" => Some(Format::Webp),
            _ => None,
        }
    }
}

/// Recognise a supported image from its first bytes.
pub fn sniff(header: &[u8]) -> Option<Format> {
    if header.starts_with(b"\x89PNG\r\n\x1a\n") {
        Some(Format::Png)
    } else if header.starts_with(&[0xFF, 0xD8, 0xFF]) {
        Some(Format::Jpeg)
    } else if header.len() >= 12 && &header[..4] == b"RIFF" && &header[8..12] == b"WEBP" {
        Some(Format::Webp)
    } else {
        None
    }
}

/// True when the path names a supported image by its contents. Reads only
/// the header.
pub fn is_image_file(path: &Path) -> bool {
    read_header(path).is_ok_and(|header| sniff(&header).is_some())
}

fn read_header(path: &Path) -> std::io::Result<Vec<u8>> {
    let mut file = std::fs::File::open(path)?;
    let mut header = vec![0u8; HEADER_BYTES];
    let mut filled = 0;
    while filled < header.len() {
        let read = file.read(&mut header[filled..])?;
        if read == 0 {
            break;
        }
        filled += read;
    }
    header.truncate(filled);
    Ok(header)
}

/// A validated image ready to store and send.
#[derive(Debug, Clone)]
pub struct LoadedImage {
    pub format: Format,
    pub width: u32,
    pub height: u32,
    /// The bytes to store and send (metadata stripped unless kept).
    pub bytes: Vec<u8>,
    /// Bytes removed by metadata stripping.
    pub stripped: usize,
}

impl LoadedImage {
    /// `PNG 1280×720 · 84 KB`, for notes and placeholders.
    pub fn summary(&self) -> String {
        format!(
            "{} {}×{} · {}",
            self.format.label(),
            self.width,
            self.height,
            human_bytes(self.bytes.len() as u64)
        )
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ImageError {
    Io(String),
    TooLarge { bytes: u64, max: u64 },
    NotAnImage,
    Malformed(&'static str),
}

impl std::fmt::Display for ImageError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ImageError::Io(error) => write!(f, "cannot read the image: {error}"),
            ImageError::TooLarge { bytes, max } => write!(
                f,
                "the image is {}, over the {} limit ([vision] max_bytes)",
                human_bytes(*bytes),
                human_bytes(*max)
            ),
            ImageError::NotAnImage => {
                write!(f, "not a PNG, JPEG, or WebP image (checked by content)")
            }
            ImageError::Malformed(what) => write!(f, "the image is malformed: {what}"),
        }
    }
}

impl ImageError {
    /// Stable error code for tool results.
    pub fn code(&self) -> &'static str {
        match self {
            ImageError::Io(_) => "READ_FAILED",
            ImageError::TooLarge { .. } => "FILE_TOO_LARGE",
            ImageError::NotAnImage => "NOT_AN_IMAGE",
            ImageError::Malformed(_) => "IMAGE_MALFORMED",
        }
    }
}

/// Load an image: size check from metadata, format check from the header,
/// then the body, dimensions, and (unless `keep_metadata`) EXIF/XMP/text
/// metadata removal. An oversized or non-image file is refused before its
/// body is read.
pub fn load(path: &Path, max_bytes: u64, keep_metadata: bool) -> Result<LoadedImage, ImageError> {
    let meta = std::fs::metadata(path).map_err(|error| ImageError::Io(error.to_string()))?;
    if !meta.is_file() {
        return Err(ImageError::Io("not a regular file".into()));
    }
    if meta.len() > max_bytes {
        return Err(ImageError::TooLarge {
            bytes: meta.len(),
            max: max_bytes,
        });
    }
    let header = read_header(path).map_err(|error| ImageError::Io(error.to_string()))?;
    let format = sniff(&header).ok_or(ImageError::NotAnImage)?;
    let raw = std::fs::read(path).map_err(|error| ImageError::Io(error.to_string()))?;
    // The file may have grown between the checks.
    if raw.len() as u64 > max_bytes {
        return Err(ImageError::TooLarge {
            bytes: raw.len() as u64,
            max: max_bytes,
        });
    }
    from_bytes(format, raw, keep_metadata)
}

/// Validate bytes already in memory.
pub fn from_bytes(
    format: Format,
    raw: Vec<u8>,
    keep_metadata: bool,
) -> Result<LoadedImage, ImageError> {
    let (width, height) = dimensions(format, &raw)?;
    if width == 0 || height == 0 {
        return Err(ImageError::Malformed("zero width or height"));
    }
    let original = raw.len();
    let bytes = if keep_metadata {
        raw
    } else {
        strip_metadata(format, &raw)?
    };
    Ok(LoadedImage {
        format,
        width,
        height,
        stripped: original.saturating_sub(bytes.len()),
        bytes,
    })
}

/// Width and height from the container headers.
pub fn dimensions(format: Format, bytes: &[u8]) -> Result<(u32, u32), ImageError> {
    match format {
        Format::Png => png_dimensions(bytes),
        Format::Jpeg => jpeg_dimensions(bytes),
        Format::Webp => webp_dimensions(bytes),
    }
}

fn be32(bytes: &[u8], at: usize) -> Option<u32> {
    bytes
        .get(at..at + 4)
        .map(|b| u32::from_be_bytes([b[0], b[1], b[2], b[3]]))
}

fn le32(bytes: &[u8], at: usize) -> Option<u32> {
    bytes
        .get(at..at + 4)
        .map(|b| u32::from_le_bytes([b[0], b[1], b[2], b[3]]))
}

fn le24(bytes: &[u8], at: usize) -> Option<u32> {
    bytes
        .get(at..at + 3)
        .map(|b| u32::from(b[0]) | u32::from(b[1]) << 8 | u32::from(b[2]) << 16)
}

fn png_dimensions(bytes: &[u8]) -> Result<(u32, u32), ImageError> {
    // Signature (8) + IHDR length (4) + "IHDR" (4) + width + height.
    if bytes.get(12..16) != Some(b"IHDR") {
        return Err(ImageError::Malformed("PNG without a leading IHDR chunk"));
    }
    match (be32(bytes, 16), be32(bytes, 20)) {
        (Some(width), Some(height)) => Ok((width, height)),
        _ => Err(ImageError::Malformed("truncated PNG header")),
    }
}

fn jpeg_dimensions(bytes: &[u8]) -> Result<(u32, u32), ImageError> {
    let mut at = 2;
    while at + 4 <= bytes.len() {
        if bytes[at] != 0xFF {
            return Err(ImageError::Malformed("JPEG marker expected"));
        }
        let marker = bytes[at + 1];
        if marker == 0xFF {
            at += 1;
            continue;
        }
        // Standalone markers carry no length.
        if marker == 0x01 || (0xD0..=0xD7).contains(&marker) {
            at += 2;
            continue;
        }
        let length = usize::from(u16::from_be_bytes([bytes[at + 2], bytes[at + 3]]));
        if length < 2 {
            return Err(ImageError::Malformed("bad JPEG segment length"));
        }
        // SOF0..SOF15, excluding DHT (C4), JPG (C8), and DAC (CC).
        let is_sof = (0xC0..=0xCF).contains(&marker) && !matches!(marker, 0xC4 | 0xC8 | 0xCC);
        if is_sof {
            let height = bytes.get(at + 5..at + 7);
            let width = bytes.get(at + 7..at + 9);
            return match (width, height) {
                (Some(w), Some(h)) => Ok((
                    u32::from(u16::from_be_bytes([w[0], w[1]])),
                    u32::from(u16::from_be_bytes([h[0], h[1]])),
                )),
                _ => Err(ImageError::Malformed("truncated JPEG frame header")),
            };
        }
        if marker == 0xDA {
            break;
        }
        at += 2 + length;
    }
    Err(ImageError::Malformed("JPEG without a frame header"))
}

fn webp_dimensions(bytes: &[u8]) -> Result<(u32, u32), ImageError> {
    let chunk = bytes.get(12..16);
    let truncated = ImageError::Malformed("truncated WebP header");
    match chunk {
        Some(b"VP8X") => {
            let width = le24(bytes, 24).ok_or(truncated.clone())? + 1;
            let height = le24(bytes, 27).ok_or(truncated)? + 1;
            Ok((width, height))
        }
        Some(b"VP8 ") => {
            // Frame tag (3) + start code 9D 01 2A, then 14-bit sizes.
            if bytes.get(23..26) != Some(&[0x9D, 0x01, 0x2A]) {
                return Err(ImageError::Malformed("VP8 start code missing"));
            }
            let w = bytes.get(26..28).ok_or(truncated.clone())?;
            let h = bytes.get(28..30).ok_or(truncated)?;
            Ok((
                u32::from(u16::from_le_bytes([w[0], w[1]]) & 0x3FFF),
                u32::from(u16::from_le_bytes([h[0], h[1]]) & 0x3FFF),
            ))
        }
        Some(b"VP8L") => {
            if bytes.get(20) != Some(&0x2F) {
                return Err(ImageError::Malformed("VP8L signature missing"));
            }
            let bits = le32(bytes, 21).ok_or(truncated)?;
            Ok(((bits & 0x3FFF) + 1, ((bits >> 14) & 0x3FFF) + 1))
        }
        _ => Err(ImageError::Malformed("unknown WebP chunk")),
    }
}

/// Remove EXIF, XMP, comments, and text metadata. Colour data (ICC
/// profiles, Adobe transform markers) is kept so the image looks the same.
pub fn strip_metadata(format: Format, bytes: &[u8]) -> Result<Vec<u8>, ImageError> {
    match format {
        Format::Png => strip_png(bytes),
        Format::Jpeg => strip_jpeg(bytes),
        Format::Webp => strip_webp(bytes),
    }
}

fn strip_png(bytes: &[u8]) -> Result<Vec<u8>, ImageError> {
    const DROP: &[&[u8; 4]] = &[b"eXIf", b"tEXt", b"zTXt", b"iTXt", b"tIME"];
    let mut out = bytes[..8].to_vec();
    let mut at = 8;
    while at + 8 <= bytes.len() {
        let length = be32(bytes, at).unwrap_or(0) as usize;
        let end = at
            .checked_add(12)
            .and_then(|n| n.checked_add(length))
            .filter(|end| *end <= bytes.len())
            .ok_or(ImageError::Malformed("truncated PNG chunk"))?;
        let kind = &bytes[at + 4..at + 8];
        if !DROP.iter().any(|drop| kind == drop.as_slice()) {
            out.extend_from_slice(&bytes[at..end]);
        }
        at = end;
        if kind == b"IEND" {
            break;
        }
    }
    Ok(out)
}

fn strip_jpeg(bytes: &[u8]) -> Result<Vec<u8>, ImageError> {
    let mut out = vec![0xFF, 0xD8];
    let mut at = 2;
    while at + 4 <= bytes.len() {
        if bytes[at] != 0xFF {
            return Err(ImageError::Malformed("JPEG marker expected"));
        }
        let marker = bytes[at + 1];
        if marker == 0xDA || marker == 0xD9 {
            // Start of scan: the rest is entropy-coded data; keep it whole.
            out.extend_from_slice(&bytes[at..]);
            return Ok(out);
        }
        if marker == 0x01 || (0xD0..=0xD7).contains(&marker) || marker == 0xFF {
            out.extend_from_slice(&bytes[at..at + 2]);
            at += 2;
            continue;
        }
        let length = usize::from(u16::from_be_bytes([bytes[at + 2], bytes[at + 3]]));
        let end = (at + 2 + length).min(bytes.len());
        // APP1 (EXIF/XMP), APP13 (IPTC), COM (comments).
        let drop = matches!(marker, 0xE1 | 0xED | 0xFE);
        if !drop {
            out.extend_from_slice(&bytes[at..end]);
        }
        at = end;
    }
    Ok(out)
}

fn strip_webp(bytes: &[u8]) -> Result<Vec<u8>, ImageError> {
    let mut out = bytes[..12].to_vec();
    let mut at = 12;
    let mut vp8x_at = None;
    while at + 8 <= bytes.len() {
        let kind = &bytes[at..at + 4];
        let length = le32(bytes, at + 4).unwrap_or(0) as usize;
        // Chunks are padded to an even length.
        let end = at
            .checked_add(8 + length + (length & 1))
            .map(|end| end.min(bytes.len()))
            .ok_or(ImageError::Malformed("truncated WebP chunk"))?;
        if kind != b"EXIF" && kind != b"XMP " {
            if kind == b"VP8X" {
                vp8x_at = Some(out.len());
            }
            out.extend_from_slice(&bytes[at..end]);
        }
        at = end;
    }
    // Clear the EXIF (0x08) and XMP (0x04) flags, then fix the RIFF size.
    if let Some(chunk) = vp8x_at
        && let Some(flags) = out.get_mut(chunk + 8)
    {
        *flags &= !0x0C;
    }
    let riff = (out.len() - 8) as u32;
    out[4..8].copy_from_slice(&riff.to_le_bytes());
    Ok(out)
}

pub fn human_bytes(bytes: u64) -> String {
    if bytes >= 1024 * 1024 {
        format!("{:.1} MB", bytes as f64 / (1024.0 * 1024.0))
    } else if bytes >= 1024 {
        format!("{} KB", bytes.div_ceil(1024))
    } else {
        format!("{bytes} B")
    }
}

/// Minimal standard base64 (RFC 4648, padded) for `data:` URLs.
pub fn base64(bytes: &[u8]) -> String {
    const TABLE: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let b = [
            chunk[0],
            chunk.get(1).copied().unwrap_or(0),
            chunk.get(2).copied().unwrap_or(0),
        ];
        let n = u32::from(b[0]) << 16 | u32::from(b[1]) << 8 | u32::from(b[2]);
        out.push(TABLE[(n >> 18) as usize & 63] as char);
        out.push(TABLE[(n >> 12) as usize & 63] as char);
        out.push(if chunk.len() > 1 {
            TABLE[(n >> 6) as usize & 63] as char
        } else {
            '='
        });
        out.push(if chunk.len() > 2 {
            TABLE[n as usize & 63] as char
        } else {
            '='
        });
    }
    out
}

#[cfg(test)]
pub(crate) mod fixtures {
    /// A valid 3×2 PNG with a tEXt and an eXIf chunk.
    pub fn png_with_metadata() -> Vec<u8> {
        fn chunk(kind: &[u8; 4], data: &[u8]) -> Vec<u8> {
            let mut out = (data.len() as u32).to_be_bytes().to_vec();
            out.extend_from_slice(kind);
            out.extend_from_slice(data);
            // CRCs are not checked by the stripper; a fixed value suffices.
            out.extend_from_slice(&[0, 0, 0, 0]);
            out
        }
        let mut ihdr = 3u32.to_be_bytes().to_vec();
        ihdr.extend_from_slice(&2u32.to_be_bytes());
        ihdr.extend_from_slice(&[8, 2, 0, 0, 0]);
        let mut png = b"\x89PNG\r\n\x1a\n".to_vec();
        png.extend(chunk(b"IHDR", &ihdr));
        png.extend(chunk(b"tEXt", b"Author\0secret person"));
        png.extend(chunk(b"eXIf", b"MM\0*GPS-COORDINATES"));
        png.extend(chunk(b"IDAT", &[0x78, 0x9C, 0x63, 0, 0, 0, 1, 0, 1]));
        png.extend(chunk(b"IEND", &[]));
        png
    }

    /// A 16×9 baseline JPEG skeleton with an EXIF APP1 and a comment.
    pub fn jpeg_with_exif() -> Vec<u8> {
        let mut jpg = vec![0xFF, 0xD8];
        // APP0 JFIF.
        jpg.extend_from_slice(&[0xFF, 0xE0, 0, 16]);
        jpg.extend_from_slice(b"JFIF\0\x01\x01\0\0\x01\0\x01\0\0");
        // APP1 EXIF.
        let exif = b"Exif\0\0GPS-SECRET-LOCATION";
        jpg.extend_from_slice(&[0xFF, 0xE1, 0, (exif.len() + 2) as u8]);
        jpg.extend_from_slice(exif);
        // COM.
        jpg.extend_from_slice(&[0xFF, 0xFE, 0, 7]);
        jpg.extend_from_slice(b"hello");
        // SOF0: precision 8, height 9, width 16, 1 component.
        jpg.extend_from_slice(&[0xFF, 0xC0, 0, 11, 8, 0, 9, 0, 16, 1, 1, 0x11, 0]);
        // SOS and a few bytes of scan data, then EOI.
        jpg.extend_from_slice(&[0xFF, 0xDA, 0, 8, 1, 1, 0, 0, 0x3F, 0, 0x12, 0x34]);
        jpg.extend_from_slice(&[0xFF, 0xD9]);
        jpg
    }

    /// An extended (VP8X) 40×30 WebP with an EXIF chunk.
    pub fn webp_with_exif() -> Vec<u8> {
        let mut body = b"WEBP".to_vec();
        body.extend_from_slice(b"VP8X");
        body.extend_from_slice(&10u32.to_le_bytes());
        body.push(0x08); // EXIF flag
        body.extend_from_slice(&[0, 0, 0]);
        body.extend_from_slice(&[39, 0, 0]); // width - 1
        body.extend_from_slice(&[29, 0, 0]); // height - 1
        body.extend_from_slice(b"VP8L");
        body.extend_from_slice(&5u32.to_le_bytes());
        body.extend_from_slice(&[0x2F, 0, 0, 0, 0]);
        body.push(0); // padding
        body.extend_from_slice(b"EXIF");
        body.extend_from_slice(&6u32.to_le_bytes());
        body.extend_from_slice(b"SECRET");
        let mut webp = b"RIFF".to_vec();
        webp.extend_from_slice(&(body.len() as u32).to_le_bytes());
        webp.extend(body);
        webp
    }
}

#[cfg(test)]
mod tests {
    use super::fixtures::*;
    use super::*;

    fn contains(haystack: &[u8], needle: &[u8]) -> bool {
        haystack.windows(needle.len()).any(|w| w == needle)
    }

    #[test]
    fn sniffs_by_content_not_extension() {
        assert_eq!(sniff(&png_with_metadata()), Some(Format::Png));
        assert_eq!(sniff(&jpeg_with_exif()), Some(Format::Jpeg));
        assert_eq!(sniff(&webp_with_exif()), Some(Format::Webp));
        assert_eq!(sniff(b"GIF89a"), None);
        assert_eq!(sniff(b"fn main() {}"), None);
    }

    #[test]
    fn reads_dimensions() {
        assert_eq!(dimensions(Format::Png, &png_with_metadata()), Ok((3, 2)));
        assert_eq!(dimensions(Format::Jpeg, &jpeg_with_exif()), Ok((16, 9)));
        assert_eq!(dimensions(Format::Webp, &webp_with_exif()), Ok((40, 30)));
    }

    #[test]
    fn strips_exif_and_text_but_keeps_image_data() {
        let png = strip_metadata(Format::Png, &png_with_metadata()).unwrap();
        assert!(!contains(&png, b"secret person"));
        assert!(!contains(&png, b"GPS-COORDINATES"));
        assert!(contains(&png, b"IDAT") && contains(&png, b"IEND"));
        assert_eq!(dimensions(Format::Png, &png), Ok((3, 2)));

        let jpg = strip_metadata(Format::Jpeg, &jpeg_with_exif()).unwrap();
        assert!(!contains(&jpg, b"GPS-SECRET-LOCATION"));
        assert!(!contains(&jpg, b"hello"));
        assert!(contains(&jpg, b"JFIF"));
        assert!(jpg.ends_with(&[0x12, 0x34, 0xFF, 0xD9]));
        assert_eq!(dimensions(Format::Jpeg, &jpg), Ok((16, 9)));

        let webp = strip_metadata(Format::Webp, &webp_with_exif()).unwrap();
        assert!(!contains(&webp, b"SECRET"));
        assert_eq!(webp[20] & 0x08, 0, "EXIF flag cleared");
        assert_eq!(le32(&webp, 4).unwrap() as usize, webp.len() - 8);
        assert_eq!(dimensions(Format::Webp, &webp), Ok((40, 30)));
    }

    #[test]
    fn refuses_oversized_and_non_images_before_reading_the_body() {
        let dir = tempfile::tempdir().unwrap();
        let big = dir.path().join("big.png");
        std::fs::write(&big, png_with_metadata()).unwrap();
        let error = load(&big, 10, false).unwrap_err();
        assert!(matches!(error, ImageError::TooLarge { .. }), "{error}");

        let fake = dir.path().join("fake.png");
        std::fs::write(&fake, "not really a png").unwrap();
        assert_eq!(
            load(&fake, 1 << 20, false).unwrap_err(),
            ImageError::NotAnImage
        );

        let image = load(&big, 1 << 20, false).unwrap();
        assert_eq!((image.width, image.height), (3, 2));
        assert!(image.stripped > 0);
        let kept = load(&big, 1 << 20, true).unwrap();
        assert_eq!(kept.stripped, 0);
        assert!(contains(&kept.bytes, b"secret person"));
    }

    #[test]
    fn base64_matches_rfc_4648() {
        assert_eq!(base64(b""), "");
        assert_eq!(base64(b"f"), "Zg==");
        assert_eq!(base64(b"fo"), "Zm8=");
        assert_eq!(base64(b"foo"), "Zm9v");
        assert_eq!(base64(b"foobar"), "Zm9vYmFy");
    }
}
