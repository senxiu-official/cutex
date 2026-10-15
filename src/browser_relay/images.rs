//! Full PNG/JPEG decode, not a header-sniff acceptance test.
//! image::Limits max_alloc is best-effort, NOT a hard process memory bound.
use super::model::*;
use base64::Engine;
use image::{ImageFormat, ImageReader, Limits};
use std::{
    io::Cursor,
    sync::atomic::{AtomicUsize, Ordering},
    time::{Duration, Instant},
};

pub const MAX_COMPRESSED: usize = 4_718_592;
const MAX_BASE64: usize = 6 * 1024 * 1024;
const MAX_EDGE: u32 = 1600;
static DECODING: AtomicUsize = AtomicUsize::new(0);
pub(crate) struct DecodeSlot;
impl DecodeSlot {
    pub(crate) fn acquire() -> Result<Self> {
        DECODING
            .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |n| {
                (n < 2).then_some(n + 1)
            })
            .map(|_| Self)
            .map_err(|_| RelayError::new("busy"))
    }
    fn wait(budget: Duration) -> Result<Self> {
        let deadline = Instant::now() + budget;
        loop {
            if let Ok(slot) = Self::acquire() {
                return Ok(slot);
            }
            let Some(remaining) = deadline.checked_duration_since(Instant::now()) else {
                return Err(RelayError::new("busy"));
            };
            std::thread::sleep(remaining.min(Duration::from_millis(5)));
        }
    }
}
impl Drop for DecodeSlot {
    fn drop(&mut self) {
        DECODING.fetch_sub(1, Ordering::SeqCst);
    }
}
pub struct ValidImage {
    pub bytes: Vec<u8>,
    pub mime: &'static str,
    pub width: u32,
    pub height: u32,
}

pub fn decode(data: &str, mime: &str, width: u64, height: u64) -> Result<ValidImage> {
    if data.len() > MAX_BASE64
        || width == 0
        || height == 0
        || width > u64::from(MAX_EDGE)
        || height > u64::from(MAX_EDGE)
    {
        return Err(RelayError::new("invalid_result"));
    }
    let image = decode_model(data, mime)?;
    if u64::from(image.width) != width || u64::from(image.height) != height {
        return Err(RelayError::new("invalid_result"));
    }
    Ok(image)
}
/// MCP images do not repeat dimensions. Full decode still imposes strict
/// dimensions; the semaphore covers both compressed allocation and decode.
pub fn decode_model(data: &str, mime: &str) -> Result<ValidImage> {
    if data.len() > MAX_BASE64 {
        return Err(RelayError::new("invalid_result"));
    }
    let _slot = DecodeSlot::acquire()?;
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(data)
        .map_err(|_| RelayError::new("invalid_result"))?;
    decode_bytes_inner(bytes, mime)
}
pub fn decode_bytes(bytes: Vec<u8>, mime: &str) -> Result<ValidImage> {
    let _slot = DecodeSlot::acquire()?;
    decode_bytes_inner(bytes, mime)
}
/// Owner reads can wait briefly for resource capacity. This never retries an
/// image decode, CDP operation, or mutating browser command.
pub fn decode_bytes_owner(bytes: Vec<u8>, mime: &str) -> Result<ValidImage> {
    let _slot = DecodeSlot::wait(Duration::from_millis(100))?;
    decode_bytes_inner(bytes, mime)
}
fn decode_bytes_inner(bytes: Vec<u8>, mime: &str) -> Result<ValidImage> {
    let format = match mime {
        "image/png" => ImageFormat::Png,
        "image/jpeg" => ImageFormat::Jpeg,
        _ => return Err(RelayError::new("invalid_result")),
    };
    if bytes.is_empty() || bytes.len() > MAX_COMPRESSED {
        return Err(RelayError::new("invalid_result"));
    }
    if image::guess_format(&bytes).ok() != Some(format) {
        return Err(RelayError::new("invalid_result"));
    }
    let mut reader = ImageReader::with_format(Cursor::new(&bytes), format);
    let mut limits = Limits::default();
    limits.max_image_width = Some(MAX_EDGE);
    limits.max_image_height = Some(MAX_EDGE);
    limits.max_alloc = Some(32 * 1024 * 1024);
    reader.limits(limits);
    let decoded = reader
        .decode()
        .map_err(|_| RelayError::new("invalid_result"))?;
    if decoded.width() == 0 || decoded.height() == 0 {
        return Err(RelayError::new("invalid_result"));
    }
    Ok(ValidImage {
        bytes,
        mime: if format == ImageFormat::Png {
            "image/png"
        } else {
            "image/jpeg"
        },
        width: decoded.width(),
        height: decoded.height(),
    })
}

// Tests that decode images or occupy slots must hold this for their full duration.
#[cfg(test)]
pub(super) static TEST_DECODE_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

#[cfg(test)]
mod tests {
    use super::*;
    fn png() -> String {
        let mut out = Cursor::new(Vec::new());
        image::DynamicImage::new_rgb8(2, 3)
            .write_to(&mut out, ImageFormat::Png)
            .unwrap();
        base64::engine::general_purpose::STANDARD.encode(out.into_inner())
    }
    #[test]
    fn browser_image_full_decode_and_dimensions() {
        let _decode_guard = TEST_DECODE_LOCK.lock().unwrap();
        let p = png();
        let image = decode(&p, "image/png", 2, 3).unwrap();
        assert_eq!((image.width, image.height), (2, 3));
        assert!(decode(&p, "image/jpeg", 2, 3).is_err());
        assert!(decode(&p, "image/png", 3, 2).is_err());
        assert!(decode(&p, "image/png", 1601, 1).is_err());
    }
    #[test]
    fn browser_image_truncated_header_and_svg_are_not_images() {
        let _decode_guard = TEST_DECODE_LOCK.lock().unwrap();
        let p = png();
        let mut bytes = base64::engine::general_purpose::STANDARD.decode(p).unwrap();
        bytes.truncate(33);
        assert!(decode(
            &base64::engine::general_purpose::STANDARD.encode(bytes),
            "image/png",
            2,
            3
        )
        .is_err());
        assert!(decode("PHN2Zz48L3N2Zz4=", "image/png", 2, 3).is_err());
        assert!(decode("!", "image/png", 2, 3).is_err());
    }
    #[test]
    fn browser_owner_decode_waits_for_capacity_but_busy_is_bounded_and_not_invalid_result() {
        let _decode_guard = TEST_DECODE_LOCK.lock().unwrap();
        let first = DecodeSlot::acquire().unwrap();
        let second = DecodeSlot::acquire().unwrap();
        assert_eq!(
            decode_model(&png(), "image/png").err().unwrap().code,
            "busy"
        );
        let started = Instant::now();
        let bytes = base64::engine::general_purpose::STANDARD
            .decode(png())
            .unwrap();
        assert_eq!(
            decode_bytes_owner(bytes, "image/png").err().unwrap().code,
            "busy"
        );
        assert!(started.elapsed() >= Duration::from_millis(90));
        assert!(started.elapsed() < Duration::from_secs(2));
        let release = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(15));
            drop(first);
        });
        let bytes = base64::engine::general_purpose::STANDARD
            .decode(png())
            .unwrap();
        assert_eq!(decode_bytes_owner(bytes, "image/png").unwrap().height, 3);
        release.join().unwrap();
        drop(second);
    }
}
