//! Optional stored-image dimensions, from bounded headers only. No pixel decode,
//! entropy scan, EXIF orientation, allocation, or claim that the image is decodable.
//!
//! Wire references: W3C PNG (IHDR), ITU T.81 B.2 (JPEG frame headers),
//! https://developers.google.com/speed/webp/docs/riff_container and RFC 6386,
//! https://www.w3.org/Graphics/GIF/spec-gif89a.txt (logical screen descriptor).
const MAX_HEADER_BYTES: usize = 64 * 1024;
const PNG: &[u8; 8] = b"\x89PNG\r\n\x1a\n";

pub(super) fn dimensions(bytes: &[u8]) -> Option<(u32, u32)> {
    let header = &bytes[..bytes.len().min(MAX_HEADER_BYTES)];
    if header.starts_with(PNG) {
        png(header)
    } else if header.starts_with(b"\xff\xd8") {
        jpeg(header)
    } else if header.starts_with(b"RIFF") {
        webp(header, bytes.len())
    } else if header.starts_with(b"GIF87a") || header.starts_with(b"GIF89a") {
        gif(header)
    } else {
        None
    }
}

fn positive(width: u32, height: u32) -> Option<(u32, u32)> {
    (width != 0 && height != 0).then_some((width, height))
}
fn be16(bytes: &[u8], at: usize) -> Option<u16> {
    Some(u16::from_be_bytes(
        bytes.get(at..at.checked_add(2)?)?.try_into().ok()?,
    ))
}
fn le16(bytes: &[u8], at: usize) -> Option<u16> {
    Some(u16::from_le_bytes(
        bytes.get(at..at.checked_add(2)?)?.try_into().ok()?,
    ))
}
fn be32(bytes: &[u8], at: usize) -> Option<u32> {
    Some(u32::from_be_bytes(
        bytes.get(at..at.checked_add(4)?)?.try_into().ok()?,
    ))
}
fn le32(bytes: &[u8], at: usize) -> Option<u32> {
    Some(u32::from_le_bytes(
        bytes.get(at..at.checked_add(4)?)?.try_into().ok()?,
    ))
}
fn le24(bytes: &[u8], at: usize) -> Option<u32> {
    let value = bytes.get(at..at.checked_add(3)?)?;
    Some(u32::from(value[0]) | (u32::from(value[1]) << 8) | (u32::from(value[2]) << 16))
}

fn png(bytes: &[u8]) -> Option<(u32, u32)> {
    let header = bytes.get(..33)?;
    if be32(header, 8)? != 13 || &header[12..16] != b"IHDR" {
        return None;
    }
    let width = be32(header, 16)?;
    let height = be32(header, 20)?;
    if width > i32::MAX as u32 || height > i32::MAX as u32 {
        return None;
    }
    let depth = header[24];
    let depth_valid = match header[25] {
        0 => matches!(depth, 1 | 2 | 4 | 8 | 16),
        2 | 4 | 6 => matches!(depth, 8 | 16),
        3 => matches!(depth, 1 | 2 | 4 | 8),
        _ => false,
    };
    if !depth_valid
        || header[26] != 0
        || header[27] != 0
        || header[28] > 1
        || crc32(&header[12..29]) != be32(header, 29)?
    {
        return None;
    }
    positive(width, height)
}
fn crc32(bytes: &[u8]) -> u32 {
    let mut crc = !0_u32;
    for &byte in bytes {
        crc ^= u32::from(byte);
        for _ in 0..8 {
            crc = if crc & 1 == 0 {
                crc >> 1
            } else {
                (crc >> 1) ^ 0xedb8_8320
            };
        }
    }
    !crc
}

fn is_sof(marker: u8) -> bool {
    matches!(marker, 0xc0..=0xc3 | 0xc5..=0xc7 | 0xc9..=0xcb | 0xcd..=0xcf)
}
fn jpeg(bytes: &[u8]) -> Option<(u32, u32)> {
    let mut at = 2;
    while at < bytes.len() {
        if *bytes.get(at)? != 0xff {
            return None;
        }
        while bytes.get(at) == Some(&0xff) {
            at += 1;
        }
        let marker = *bytes.get(at)?;
        at += 1;
        match marker {
            0x01 => continue, // TEM is the only stand-alone marker accepted before a frame.
            0x00 | 0xd0..=0xda => return None, // No stuffed data, restarts, repeated SOI, EOI or entropy/SOS scan.
            _ => {}
        }
        let length = usize::from(be16(bytes, at)?);
        if length < 2 {
            return None;
        }
        let end = at.checked_add(length)?;
        let payload = bytes.get(at.checked_add(2)?..end)?;
        if is_sof(marker) {
            return jpeg_frame(marker, payload);
        }
        // Skip metadata/tables by their declared length, never search inside an
        // APP thumbnail or entropy stream for something resembling an SOF.
        // Hierarchical DHP and deferred DNL dimensions are intentionally omitted.
        if !matches!(marker, 0xc4 | 0xcc | 0xdb | 0xdd | 0xe0..=0xef | 0xfe) {
            return None;
        }
        at = end;
    }
    None
}
fn jpeg_frame(marker: u8, payload: &[u8]) -> Option<(u32, u32)> {
    let header = payload.get(..6)?;
    let components = usize::from(header[5]);
    if components == 0 || payload.len() != 6 + 3 * components {
        return None;
    }
    let lossless = matches!(marker, 0xc3 | 0xc7 | 0xcb | 0xcf);
    let precision_valid = if marker == 0xc0 {
        header[0] == 8
    } else if lossless {
        (2..=16).contains(&header[0])
    } else {
        matches!(header[0], 8 | 12)
    };
    if !precision_valid || matches!(marker, 0xc2 | 0xc6 | 0xca | 0xce) && components > 4 {
        return None;
    }
    let mut identifiers = [false; 256];
    for component in payload[6..].chunks_exact(3) {
        let id = usize::from(component[0]);
        let horizontal = component[1] >> 4;
        let vertical = component[1] & 15;
        if identifiers[id]
            || !(1..=4).contains(&horizontal)
            || !(1..=4).contains(&vertical)
            || if lossless {
                component[2] != 0
            } else {
                component[2] > 3
            }
        {
            return None;
        }
        identifiers[id] = true;
    }
    // A zero height can be resolved by a later DNL, which would require an
    // entropy scan. Omit it rather than inventing dimensions.
    positive(u32::from(be16(header, 3)?), u32::from(be16(header, 1)?))
}

fn webp(bytes: &[u8], available: usize) -> Option<(u32, u32)> {
    if bytes.get(8..12)? != b"WEBP" {
        return None;
    }
    let size = le32(bytes, 4)?;
    if size < 4 || size & 1 != 0 || size > u32::MAX - 9 {
        return None;
    }
    let end = usize::try_from(size).ok()?.checked_add(8)?;
    if end > available {
        return None;
    }
    let mut at = 12;
    while at < end {
        let chunk_header_end = at.checked_add(8)?;
        if chunk_header_end > end {
            return None;
        }
        let chunk = bytes.get(at..chunk_header_end)?;
        if !chunk[..4].iter().all(|byte| (0x20..=0x7e).contains(byte)) {
            return None;
        }
        let length = usize::try_from(le32(chunk, 4)?).ok()?;
        let payload_end = chunk_header_end.checked_add(length)?;
        let next = payload_end.checked_add(length & 1)?;
        if next > end {
            return None;
        }
        // Padding contents beyond our inspection prefix are not part of the
        // header. Container extents still must fit the supplied bytes.
        if length & 1 != 0 && payload_end < bytes.len() && bytes[payload_end] != 0 {
            return None;
        }
        let payload = bytes.get(chunk_header_end..payload_end.min(bytes.len()))?;
        match &chunk[..4] {
            b"VP8 " => return vp8(payload, length),
            b"VP8L" => {
                if length < 5 || payload.first() != Some(&0x2f) {
                    return None;
                }
                let bits = le32(payload, 1)?;
                if bits >> 29 != 0 {
                    return None;
                }
                return positive((bits & 0x3fff) + 1, ((bits >> 14) & 0x3fff) + 1);
            }
            b"VP8X" => {
                if length != 10 || payload.get(..4)?[0] & 0xc1 != 0 || payload[1..4] != [0, 0, 0] {
                    return None;
                }
                let width = le24(payload, 4)? + 1;
                let height = le24(payload, 7)? + 1;
                width.checked_mul(height)?; // The WebP canvas product must fit u32.
                return positive(width, height);
            }
            _ => at = next,
        }
    }
    None
}
fn vp8(payload: &[u8], declared: usize) -> Option<(u32, u32)> {
    let header = payload.get(..10)?;
    let tag = le24(header, 0)?;
    if declared < 10
        || tag & 1 != 0
        || (tag >> 1) & 7 > 3
        || tag & 0x10 == 0
        || (tag >> 5) as usize > declared - 10
        || &header[3..6] != b"\x9d\x01\x2a"
    {
        return None;
    }
    positive(
        u32::from(le16(header, 6)? & 0x3fff),
        u32::from(le16(header, 8)? & 0x3fff),
    )
}

fn gif(bytes: &[u8]) -> Option<(u32, u32)> {
    let header = bytes.get(..13)?;
    if &header[..6] == b"GIF87a" && (header[10] & 8 != 0 || header[12] != 0) {
        return None;
    }
    if header[10] & 0x80 != 0 {
        let colors = 1_usize << (usize::from(header[10] & 7) + 1);
        if usize::from(header[11]) >= colors {
            return None;
        }
        bytes.get(13..13 + 3 * colors)?;
    }
    positive(u32::from(le16(header, 6)?), u32::from(le16(header, 8)?))
}

#[cfg(test)]
mod tests {
    use super::*;

    // A real 1x1 RGBA IHDR, including its independent published CRC.
    const ONE_PNG: &[u8] = &[
        137, 80, 78, 71, 13, 10, 26, 10, 0, 0, 0, 13, 73, 72, 68, 82, 0, 0, 0, 1, 0, 0, 0, 1, 8, 6,
        0, 0, 0, 31, 21, 196, 137,
    ];
    fn png_header(width: u32, height: u32) -> Vec<u8> {
        let mut bytes = ONE_PNG.to_vec();
        bytes[16..20].copy_from_slice(&width.to_be_bytes());
        bytes[20..24].copy_from_slice(&height.to_be_bytes());
        let crc = crc32(&bytes[12..29]);
        bytes[29..33].copy_from_slice(&crc.to_be_bytes());
        bytes
    }
    fn jpeg_header(
        marker: u8,
        width: u16,
        height: u16,
        precision: u8,
        components: &[[u8; 3]],
    ) -> Vec<u8> {
        let mut bytes = vec![0xff, 0xd8, 0xff, marker];
        bytes.extend_from_slice(&(8 + 3 * components.len() as u16).to_be_bytes());
        bytes.push(precision);
        bytes.extend_from_slice(&height.to_be_bytes());
        bytes.extend_from_slice(&width.to_be_bytes());
        bytes.push(components.len() as u8);
        for component in components {
            bytes.extend_from_slice(component);
        }
        bytes
    }
    fn webp_chunk(kind: &[u8; 4], payload: &[u8]) -> Vec<u8> {
        let mut bytes = b"RIFF".to_vec();
        let size = 4 + 8 + payload.len() + (payload.len() & 1);
        bytes.extend_from_slice(&(size as u32).to_le_bytes());
        bytes.extend_from_slice(b"WEBP");
        bytes.extend_from_slice(kind);
        bytes.extend_from_slice(&(payload.len() as u32).to_le_bytes());
        bytes.extend_from_slice(payload);
        if payload.len() & 1 != 0 {
            bytes.push(0);
        }
        bytes
    }
    fn vp8_header(width: u16, height: u16) -> Vec<u8> {
        let mut bytes = vec![0x30, 0, 0, 0x9d, 1, 0x2a]; // One-byte first partition, plus its opaque byte.
        bytes.extend_from_slice(&width.to_le_bytes());
        bytes.extend_from_slice(&height.to_le_bytes());
        bytes.push(0);
        bytes
    }
    fn vp8l_header(width: u32, height: u32) -> Vec<u8> {
        let bits = (width - 1) | ((height - 1) << 14);
        let mut bytes = vec![0x2f];
        bytes.extend_from_slice(&bits.to_le_bytes());
        bytes
    }
    fn vp8x_header(width: u32, height: u32) -> Vec<u8> {
        let mut bytes = vec![0, 0, 0, 0];
        bytes.extend_from_slice(&(width - 1).to_le_bytes()[..3]);
        bytes.extend_from_slice(&(height - 1).to_le_bytes()[..3]);
        bytes
    }
    fn gif_header(version: &[u8; 6], width: u16, height: u16) -> Vec<u8> {
        let mut bytes = version.to_vec();
        bytes.extend_from_slice(&width.to_le_bytes());
        bytes.extend_from_slice(&height.to_le_bytes());
        bytes.extend_from_slice(&[0, 0, 0]);
        bytes
    }

    #[test]
    fn png_ihdr_dimensions_crc_and_field_validation() {
        assert_eq!(dimensions(ONE_PNG), Some((1, 1)));
        assert_eq!(dimensions(&png_header(800, 600)), Some((800, 600)));
        for (width, height) in [(0, 1), (1, 0), (u32::MAX, 1), (1, 0x8000_0000)] {
            assert_eq!(dimensions(&png_header(width, height)), None);
        }
        for (index, value) in [
            (8, 1),
            (12, b'X'),
            (24, 7),
            (25, 5),
            (26, 1),
            (27, 1),
            (28, 2),
            (32, 0),
        ] {
            let mut bytes = ONE_PNG.to_vec();
            bytes[index] = value;
            if index < 29 {
                let crc = crc32(&bytes[12..29]);
                bytes[29..33].copy_from_slice(&crc.to_be_bytes());
            }
            assert_eq!(dimensions(&bytes), None, "bad field {index}");
        }
    }

    #[test]
    fn jpeg_sof_family_uses_frame_dimensions_not_table_marker_ranges() {
        for marker in [
            0xc0, 0xc1, 0xc2, 0xc3, 0xc5, 0xc6, 0xc7, 0xc9, 0xca, 0xcb, 0xcd, 0xce, 0xcf,
        ] {
            assert_eq!(
                dimensions(&jpeg_header(marker, 321, 654, 8, &[[1, 0x11, 0]])),
                Some((321, 654)),
                "marker {marker:x}"
            );
        }
        for marker in [0xc4, 0xc8, 0xcc] {
            assert_eq!(
                dimensions(&jpeg_header(marker, 321, 654, 8, &[[1, 0x11, 0]])),
                None
            );
        }
        assert_eq!(
            dimensions(&jpeg_header(0xc0, 65535, 65535, 8, &[[1, 0x11, 0]])),
            Some((65535, 65535))
        );
        assert_eq!(
            dimensions(&jpeg_header(0xc3, 1, 2, 16, &[[1, 0x11, 0]])),
            Some((1, 2))
        );
    }

    #[test]
    fn jpeg_rejects_invalid_frame_structure_and_stops_before_entropy() {
        for bytes in [
            jpeg_header(0xc0, 0, 1, 8, &[[1, 0x11, 0]]),
            jpeg_header(0xc0, 1, 0, 8, &[[1, 0x11, 0]]),
            jpeg_header(0xc0, 1, 1, 12, &[[1, 0x11, 0]]),
            jpeg_header(0xc3, 1, 1, 1, &[[1, 0x11, 0]]),
            jpeg_header(0xc0, 1, 1, 8, &[]),
            jpeg_header(0xc0, 1, 1, 8, &[[1, 0x01, 0]]),
            jpeg_header(0xc0, 1, 1, 8, &[[1, 0x51, 0]]),
            jpeg_header(0xc0, 1, 1, 8, &[[1, 0x11, 4]]),
            jpeg_header(0xc3, 1, 1, 8, &[[1, 0x11, 1]]),
            jpeg_header(0xc0, 1, 1, 8, &[[1, 0x11, 0], [1, 0x11, 0]]),
            vec![0xff, 0xd8, 0xff, 0xe0, 0, 1],
            vec![0xff, 0xd8, 0xff, 0, 0, 2],
            vec![0xff, 0xd8, 0x44],
        ] {
            assert_eq!(dimensions(&bytes), None);
        }
        let frame = jpeg_header(0xc0, 7, 9, 8, &[[1, 0x11, 0]]);
        let mut metadata = vec![0xff, 0xd8, 0xff, 0xe1];
        metadata.extend_from_slice(&((frame.len() + 2) as u16).to_be_bytes());
        metadata.extend_from_slice(&frame); // An EXIF-like embedded image is not the enclosing frame.
        assert_eq!(dimensions(&metadata), None);
        metadata.extend_from_slice(&frame[2..]);
        assert_eq!(dimensions(&metadata), Some((7, 9)));
        let mut entropy = vec![0xff, 0xd8, 0xff, 0xda];
        entropy.extend_from_slice(&frame[2..]);
        assert_eq!(dimensions(&entropy), None);
    }

    #[test]
    fn webp_vp8_vp8l_and_vp8x_canvas_dimensions() {
        assert_eq!(
            dimensions(&webp_chunk(b"VP8 ", &vp8_header(640, 480))),
            Some((640, 480))
        );
        assert_eq!(
            dimensions(&webp_chunk(b"VP8 ", &vp8_header(0x4001, 0x8002))),
            Some((1, 2))
        );
        assert_eq!(
            dimensions(&webp_chunk(b"VP8L", &vp8l_header(16384, 1))),
            Some((16384, 1))
        );
        let mut alpha = vp8l_header(123, 456);
        alpha[4] |= 0x10;
        assert_eq!(dimensions(&webp_chunk(b"VP8L", &alpha)), Some((123, 456)));
        let mut animation = vp8x_header(1200, 900);
        animation[0] = 2;
        assert_eq!(
            dimensions(&webp_chunk(b"VP8X", &animation)),
            Some((1200, 900))
        );
    }

    #[test]
    fn webp_checks_container_chunk_extents_and_codec_header_fields() {
        let valid = webp_chunk(b"VP8 ", &vp8_header(3, 5));
        for (at, value) in [
            (0, b'X'),
            (8, b'X'),
            (4, 0xff),
            (16, 0xff),
            (20, 0x31),
            (23, 0),
            (26, 0),
        ] {
            let mut bytes = valid.clone();
            bytes[at] = value;
            if at == 26 {
                bytes[27] = 0;
            }
            assert_eq!(dimensions(&bytes), None, "bad field {at}");
        }
        let mut version = vp8l_header(3, 5);
        version[4] |= 0x20;
        assert_eq!(dimensions(&webp_chunk(b"VP8L", &version)), None);
        let mut signature = vp8l_header(3, 5);
        signature[0] = 0;
        assert_eq!(dimensions(&webp_chunk(b"VP8L", &signature)), None);
        let mut reserved = vp8x_header(3, 5);
        reserved[1] = 1;
        assert_eq!(dimensions(&webp_chunk(b"VP8X", &reserved)), None);
        reserved = vp8x_header(3, 5);
        reserved[0] = 1;
        assert_eq!(dimensions(&webp_chunk(b"VP8X", &reserved)), None);
        assert_eq!(
            dimensions(&webp_chunk(b"VP8X", &vp8x_header(65536, 65536))),
            None
        );
        assert_eq!(dimensions(&webp_chunk(b"VP8X", &[0; 9])), None);
    }

    #[test]
    fn webp_skips_padded_unknown_chunks_and_does_not_cap_large_pixel_payloads() {
        let mut bytes = webp_chunk(b"JUNK", b"x");
        let tail = webp_chunk(b"VP8L", &vp8l_header(17, 19));
        bytes.extend_from_slice(&tail[12..]);
        let length = (bytes.len() - 8) as u32;
        bytes[4..8].copy_from_slice(&length.to_le_bytes());
        assert_eq!(dimensions(&bytes), Some((17, 19)));
        bytes[21] = 1;
        assert_eq!(dimensions(&bytes), None); // Nonzero RIFF padding.
        let mut payload = vp8_header(23, 29);
        payload.resize(MAX_HEADER_BYTES + 100, 0);
        assert_eq!(dimensions(&webp_chunk(b"VP8 ", &payload)), Some((23, 29)));
    }

    #[test]
    fn gif_87a_89a_screen_dimensions_and_palette_descriptor_validation() {
        for version in [b"GIF87a", b"GIF89a"] {
            assert_eq!(dimensions(&gif_header(version, 320, 200)), Some((320, 200)));
            assert_eq!(dimensions(&gif_header(version, 0, 200)), None);
        }
        let mut palette = gif_header(b"GIF89a", 2, 3);
        palette[10] = 0x80;
        palette.extend_from_slice(&[0; 6]);
        assert_eq!(dimensions(&palette), Some((2, 3)));
        palette.pop();
        assert_eq!(dimensions(&palette), None);
        palette.push(0);
        palette[11] = 2;
        assert_eq!(dimensions(&palette), None);
        let mut aspect = gif_header(b"GIF87a", 2, 3);
        aspect[12] = 1;
        assert_eq!(dimensions(&aspect), None);
        aspect[..6].copy_from_slice(b"GIF89a");
        assert_eq!(dimensions(&aspect), Some((2, 3)));
    }

    #[test]
    fn every_truncated_required_header_is_omitted_without_panicking() {
        for bytes in [
            ONE_PNG.to_vec(),
            jpeg_header(0xc0, 3, 5, 8, &[[1, 0x11, 0]]),
            webp_chunk(b"VP8 ", &vp8_header(3, 5)),
            webp_chunk(b"VP8L", &vp8l_header(3, 5)),
            webp_chunk(b"VP8X", &vp8x_header(3, 5)),
            gif_header(b"GIF89a", 3, 5),
        ] {
            for end in 0..bytes.len() {
                assert_eq!(
                    dimensions(&bytes[..end]),
                    None,
                    "prefix {end} of {}",
                    bytes.len()
                );
            }
        }
        assert_eq!(dimensions(b"not an image"), None);
        assert_eq!(dimensions(b"GIF90a\x01\x00\x01\x00\x00\x00\x00"), None);
    }

    #[test]
    fn scan_limit_is_inclusive_and_metadata_beyond_it_is_not_inspected() {
        let frame = jpeg_header(0xc0, 31, 37, 8, &[[1, 0x11, 0]]);
        let pad = MAX_HEADER_BYTES - 2 - 4 - (frame.len() - 2);
        let mut bytes = vec![0xff, 0xd8, 0xff, 0xe1];
        bytes.extend_from_slice(&((pad + 2) as u16).to_be_bytes());
        bytes.resize(6 + pad, 0);
        bytes.extend_from_slice(&frame[2..]);
        assert_eq!(bytes.len(), MAX_HEADER_BYTES);
        assert_eq!(dimensions(&bytes), Some((31, 37)));
        bytes.insert(6, 0);
        let length = ((pad + 3) as u16).to_be_bytes();
        bytes[4..6].copy_from_slice(&length);
        assert_eq!(dimensions(&bytes), None);
        let mut webp = webp_chunk(b"JUNK", &vec![0; MAX_HEADER_BYTES]);
        let tail = webp_chunk(b"VP8X", &vp8x_header(31, 37));
        webp.extend_from_slice(&tail[12..]);
        let length = (webp.len() - 8) as u32;
        webp[4..8].copy_from_slice(&length.to_le_bytes());
        assert_eq!(dimensions(&webp), None);
        let mut fills = vec![0xff; MAX_HEADER_BYTES + 32];
        fills[0..2].copy_from_slice(b"\xff\xd8");
        assert_eq!(dimensions(&fills), None);
    }
}
