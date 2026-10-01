//! Pixel dimensions from the header bytes of a PNG, JPEG, GIF or WebP file
//! (`@t3tools/shared/imageDimensions`), so a client can reserve the exact box before the bytes
//! arrive. Anything else, a truncated header or a malformed file gives `None`.

use zc_contracts::AssetImageDimensions;

/// `IMAGE_DIMENSIONS_HEADER_BYTES`: a JPEG frame header can sit behind several 64 KiB metadata
/// segments.
pub const IMAGE_DIMENSIONS_HEADER_BYTES: usize = 256 * 1024;

/// `HEADER_IMAGE_EXTENSIONS` (`AssetAccess.ts`): the formats [`read_image_dimensions`] parses.
pub const HEADER_IMAGE_EXTENSIONS: &[&str] = &[".png", ".jpg", ".jpeg", ".gif", ".webp"];

/// Width and height in pixels.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ImageDimensions {
    pub width: u32,
    pub height: u32,
}

impl From<ImageDimensions> for AssetImageDimensions {
    fn from(value: ImageDimensions) -> Self {
        Self {
            width: i64::from(value.width),
            height: i64::from(value.height),
        }
    }
}

/// `readImageDimensions`.
pub fn read_image_dimensions(bytes: &[u8]) -> Option<ImageDimensions> {
    let dimensions = read_png(bytes)
        .or_else(|| read_gif(bytes))
        .or_else(|| read_webp(bytes))
        .or_else(|| read_jpeg(bytes))?;
    (dimensions.width > 0 && dimensions.height > 0).then_some(dimensions)
}

fn be16(bytes: &[u8], at: usize) -> Option<u16> {
    Some(u16::from_be_bytes([*bytes.get(at)?, *bytes.get(at + 1)?]))
}

fn le16(bytes: &[u8], at: usize) -> Option<u16> {
    Some(u16::from_le_bytes([*bytes.get(at)?, *bytes.get(at + 1)?]))
}

fn u16_at(bytes: &[u8], at: usize, little: bool) -> Option<u16> {
    if little {
        le16(bytes, at)
    } else {
        be16(bytes, at)
    }
}

fn u32_at(bytes: &[u8], at: usize, little: bool) -> Option<u32> {
    let slice: [u8; 4] = bytes.get(at..at + 4)?.try_into().ok()?;
    Some(if little { u32::from_le_bytes(slice) } else { u32::from_be_bytes(slice) })
}

fn read_png(bytes: &[u8]) -> Option<ImageDimensions> {
    if bytes.len() < 24 || bytes[..4] != [0x89, 0x50, 0x4e, 0x47] || bytes[12..16] != *b"IHDR" {
        return None;
    }
    Some(ImageDimensions {
        width: u32_at(bytes, 16, false)?,
        height: u32_at(bytes, 20, false)?,
    })
}

fn read_gif(bytes: &[u8]) -> Option<ImageDimensions> {
    if bytes.len() < 10 || bytes[..4] != *b"GIF8" {
        return None;
    }
    Some(ImageDimensions {
        width: u32::from(le16(bytes, 6)?),
        height: u32::from(le16(bytes, 8)?),
    })
}

fn read_webp(bytes: &[u8]) -> Option<ImageDimensions> {
    if bytes.len() < 16 || bytes[..4] != *b"RIFF" || bytes[8..12] != *b"WEBP" {
        return None;
    }
    match &bytes[12..16] {
        b"VP8 " => {
            if bytes.len() < 30 {
                return None;
            }
            Some(ImageDimensions {
                width: u32::from(le16(bytes, 26)? & 0x3fff),
                height: u32::from(le16(bytes, 28)? & 0x3fff),
            })
        }
        b"VP8L" => {
            if bytes.len() < 25 {
                return None;
            }
            let packed = u32_at(bytes, 21, true)?;
            Some(ImageDimensions {
                width: (packed & 0x3fff) + 1,
                height: ((packed >> 14) & 0x3fff) + 1,
            })
        }
        b"VP8X" => {
            if bytes.len() < 30 {
                return None;
            }
            let u24 = |at: usize| u32::from(bytes[at]) | (u32::from(bytes[at + 1]) << 8) | (u32::from(bytes[at + 2]) << 16);
            Some(ImageDimensions {
                width: u24(24) + 1,
                height: u24(27) + 1,
            })
        }
        _ => None,
    }
}

fn read_jpeg(bytes: &[u8]) -> Option<ImageDimensions> {
    if bytes.len() < 4 || bytes[0] != 0xff || bytes[1] != 0xd8 {
        return None;
    }
    let mut offset = 2usize;
    let mut rotated = false;
    while offset + 9 <= bytes.len() {
        if bytes[offset] != 0xff {
            return None;
        }
        let marker = bytes[offset + 1];
        // Padding bytes between segments.
        if marker == 0xff {
            offset += 1;
            continue;
        }
        // Start-of-frame markers carry the dimensions (not DHT, JPG, DAC).
        if (0xc0..=0xcf).contains(&marker) && marker != 0xc4 && marker != 0xc8 && marker != 0xcc {
            let height = u32::from(be16(bytes, offset + 5)?);
            let width = u32::from(be16(bytes, offset + 7)?);
            return Some(if rotated {
                ImageDimensions { width: height, height: width }
            } else {
                ImageDimensions { width, height }
            });
        }
        if marker == 0xd9 || marker == 0xda {
            return None;
        }
        // TEM and the restart markers stand alone.
        if marker == 0x01 || (0xd0..=0xd7).contains(&marker) {
            offset += 2;
            continue;
        }
        let length = usize::from(be16(bytes, offset + 2)?);
        if marker == 0xe1 && !rotated {
            rotated = exif_orientation_swaps_axes(bytes, offset + 4, offset + 2 + length);
        }
        offset += 2 + length;
    }
    None
}

/// Whether EXIF orientation 5–8 (a 90° rotation) applies; `start` is the APP1 payload.
fn exif_orientation_swaps_axes(bytes: &[u8], start: usize, end: usize) -> bool {
    let end = end.min(bytes.len());
    if end < start + 14 || bytes.get(start..start + 4) != Some(b"Exif") {
        return false;
    }
    let tiff = start + 6;
    let little = bytes[tiff] == 0x49 && bytes[tiff + 1] == 0x49;
    if !little && !(bytes[tiff] == 0x4d && bytes[tiff + 1] == 0x4d) {
        return false;
    }
    let Some(ifd_offset) = u32_at(bytes, tiff + 4, little) else { return false };
    let ifd = tiff + ifd_offset as usize;
    if ifd + 2 > end {
        return false;
    }
    let Some(entries) = u16_at(bytes, ifd, little) else { return false };
    for index in 0..usize::from(entries) {
        let entry = ifd + 2 + index * 12;
        if entry + 12 > end {
            return false;
        }
        if u16_at(bytes, entry, little) == Some(0x0112) {
            let orientation = u16_at(bytes, entry + 8, little).unwrap_or(0);
            return (5..=8).contains(&orientation);
        }
    }
    false
}

#[cfg(test)]
mod tests {
    //! `imageDimensions.test.ts`.
    use super::*;

    enum Part<'a> {
        Bytes(&'a [u8]),
        Text(&'a str),
    }

    fn bytes(parts: &[Part<'_>]) -> Vec<u8> {
        let mut out = Vec::new();
        for part in parts {
            match part {
                Part::Bytes(b) => out.extend_from_slice(b),
                Part::Text(t) => out.extend(t.chars().map(|c| c as u8)),
            }
        }
        out
    }

    fn u32b(n: u32) -> [u8; 4] {
        n.to_be_bytes()
    }
    fn u16b(n: u16) -> [u8; 2] {
        n.to_be_bytes()
    }
    fn u16l(n: u16) -> [u8; 2] {
        n.to_le_bytes()
    }

    fn dims(width: u32, height: u32) -> Option<ImageDimensions> {
        Some(ImageDimensions { width, height })
    }

    use Part::{Bytes as B, Text as T};

    #[test]
    fn reads_a_png_ihdr() {
        let png = bytes(&[
            B(&[0x89]),
            T("PNG"),
            B(&[0x0d, 0x0a, 0x1a, 0x0a]),
            B(&u32b(13)),
            T("IHDR"),
            B(&u32b(1600)),
            B(&u32b(900)),
        ]);
        assert_eq!(read_image_dimensions(&png), dims(1600, 900));
    }

    #[test]
    fn reads_a_gif_logical_screen() {
        assert_eq!(read_image_dimensions(&bytes(&[T("GIF89a"), B(&u16l(320)), B(&u16l(240))])), dims(320, 240));
    }

    #[test]
    fn reads_a_jpeg_start_of_frame_after_an_app_segment() {
        let app1 = bytes(&[B(&[0xff, 0xe1]), B(&u16b(8)), T("Exif\0\0")]);
        let sof0 = bytes(&[B(&[0xff, 0xc0]), B(&u16b(17)), B(&[8]), B(&u16b(1400)), B(&u16b(720))]);
        assert_eq!(read_image_dimensions(&bytes(&[B(&[0xff, 0xd8]), B(&app1), B(&sof0)])), dims(720, 1400));
    }

    #[test]
    fn swaps_the_axes_for_a_rotated_jpeg() {
        let tiff: Vec<u8> = vec![
            0x4d, 0x4d, 0x00, 0x2a, 0x00, 0x00, 0x00, 0x08, 0x00, 0x01, 0x01, 0x12, 0x00, 0x03, 0x00, 0x00, 0x00, 0x01, 0x00, 0x06, 0x00, 0x00, 0x00, 0x00,
            0x00, 0x00,
        ];
        let app1 = bytes(&[B(&[0xff, 0xe1]), B(&u16b((2 + 6 + tiff.len()) as u16)), T("Exif\0\0"), B(&tiff)]);
        let sof0 = bytes(&[B(&[0xff, 0xc0]), B(&u16b(17)), B(&[8]), B(&u16b(3024)), B(&u16b(4032))]);
        assert_eq!(read_image_dimensions(&bytes(&[B(&[0xff, 0xd8]), B(&app1), B(&sof0)])), dims(3024, 4032));
        let xmp = bytes(&[B(&[0xff, 0xe1]), B(&u16b(2 + 29)), T("http://ns.adobe.com/xap/1.0/\0")]);
        assert_eq!(
            read_image_dimensions(&bytes(&[B(&[0xff, 0xd8]), B(&app1), B(&xmp), B(&sof0)])),
            dims(3024, 4032)
        );
        let mut upright = tiff.clone();
        upright[19] = 0x01;
        let app1_upright = bytes(&[B(&[0xff, 0xe1]), B(&u16b((2 + 6 + upright.len()) as u16)), T("Exif\0\0"), B(&upright)]);
        assert_eq!(read_image_dimensions(&bytes(&[B(&[0xff, 0xd8]), B(&app1_upright), B(&sof0)])), dims(4032, 3024));
    }

    #[test]
    fn steps_over_standalone_markers() {
        let sof0 = bytes(&[B(&[0xff, 0xc0]), B(&u16b(17)), B(&[8]), B(&u16b(10)), B(&u16b(20))]);
        assert_eq!(
            read_image_dimensions(&bytes(&[B(&[0xff, 0xd8]), B(&[0xff, 0x01]), B(&[0xff, 0xd3]), B(&sof0)])),
            dims(20, 10)
        );
    }

    #[test]
    fn does_not_mistake_a_huffman_table_for_a_frame() {
        let dht = bytes(&[B(&[0xff, 0xc4]), B(&u16b(4)), B(&[0, 0])]);
        let sof2 = bytes(&[B(&[0xff, 0xc2]), B(&u16b(17)), B(&[8]), B(&u16b(10)), B(&u16b(20))]);
        assert_eq!(read_image_dimensions(&bytes(&[B(&[0xff, 0xd8]), B(&dht), B(&sof2)])), dims(20, 10));
    }

    #[test]
    fn reads_each_webp_flavour() {
        let riff = |chunk: &str, body: &[u8]| bytes(&[T("RIFF"), B(&u32b(0)), T("WEBP"), T(chunk), B(&u32b(body.len() as u32)), B(body)]);
        let mut vp8 = vec![0, 0, 0, 0x9d, 0x01, 0x2a];
        vp8.extend(u16l(800));
        vp8.extend(u16l(600));
        assert_eq!(read_image_dimensions(&riff("VP8 ", &vp8)), dims(800, 600));
        let packed: u32 = (800 - 1) | ((600 - 1) << 14);
        let mut vp8l = vec![0x2f];
        vp8l.extend(packed.to_le_bytes());
        assert_eq!(read_image_dimensions(&riff("VP8L", &vp8l)), dims(800, 600));
        let vp8x = [0, 0, 0, 0, (799 & 0xff) as u8, (799 >> 8) as u8, 0, (599 & 0xff) as u8, (599 >> 8) as u8, 0];
        assert_eq!(read_image_dimensions(&riff("VP8X", &vp8x)), dims(800, 600));
    }

    #[test]
    fn rejects_unsupported_truncated_or_empty_input() {
        assert_eq!(read_image_dimensions(b"<svg xmlns='http://www.w3.org/2000/svg'/>"), None);
        assert_eq!(read_image_dimensions(&bytes(&[B(&[0x89]), T("PNG")])), None);
        assert_eq!(read_image_dimensions(&bytes(&[T("GIF89a"), B(&u16l(0)), B(&u16l(240))])), None);
        assert_eq!(read_image_dimensions(&[0xff, 0xd8, 0xff, 0xd9]), None);
        assert_eq!(read_image_dimensions(&[]), None);
    }
}
