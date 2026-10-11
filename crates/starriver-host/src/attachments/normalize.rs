//! Deterministic provider-independent image normalization (
//! `attachment-local/src/normalization.ts` parity, Rust port).

use image::{imageops::FilterType, DynamicImage, ImageFormat, ImageReader};

use super::error::{AttachmentError, AttachmentErrorCode};
use super::types::{request_image_dimensions, ImageAttachmentLimits, NormalizationPolicy};

/// Normalized bytes beside the facts recorded by a durable reference.
#[derive(Debug, Clone)]
pub struct NormalizedImage {
    pub data: Vec<u8>,
    pub media_type: String,
    pub width: u32,
    pub height: u32,
    /// Source raster dimensions (before any downscale).
    pub source_dimensions: (u32, u32),
}

/// Raster vocabulary accepted by the store. also admits gif; we reject it
/// for now (single-frame png/jpeg/webp only), which keeps normalization
/// deterministic without animation handling.
fn image_format_for(media_type: &str) -> Option<ImageFormat> {
    match media_type {
        "image/png" => Some(ImageFormat::Png),
        "image/jpeg" => Some(ImageFormat::Jpeg),
        "image/webp" => Some(ImageFormat::WebP),
        _ => None,
    }
}

/// Quality ladder shared by normalization: spaced so each step buys a real
/// size reduction (`IMAGE_ENCODING_QUALITIES`).
const ENCODING_QUALITIES: [u8; 3] = [85, 75, 60];

/// Decode, downscale (never enlarge) inside the normalized pixel budget, and
/// re-encode on a quality ladder under the encoded-byte target. Alpha-bearing
/// sources become WebP; everything else becomes JPEG.
///
/// Differences from, deliberately scoped for this first step:
/// - EXIF orientation is not applied (`rotate`).
/// - The source always goes through the ladder (passes clean small images
///   through byte-identically); re-encoding keeps outputs deterministic, and
///   pass-through can be added later as an optimization.
pub fn normalize_image(
    data: &[u8],
    media_type: &str,
    admission: &ImageAttachmentLimits,
    policy: &NormalizationPolicy,
) -> Result<NormalizedImage, AttachmentError> {
    let format = image_format_for(media_type).ok_or_else(|| {
        AttachmentError::new(
            AttachmentErrorCode::UnsupportedImageMediaType,
            format!(
                "{media_type} is not supported (expected image/png, image/jpeg, or image/webp)"
            ),
        )
    })?;
 // Declared media type must match the decoded bytes (IMAGE_TYPE_MISMATCH).
    // BUG-122: the dimension caps are checked BEFORE the full decode — a
    // small compressed payload claiming huge dimensions (decompression
    // bomb) must be rejected off the header, not after the whole bitmap
    // has been allocated.
    let mut reader = ImageReader::with_format(std::io::Cursor::new(data), format);
    let (header_w, header_h) = reader
        .into_dimensions()
        .map_err(|_| {
            AttachmentError::new(
                AttachmentErrorCode::ImageTypeMismatch,
                "declared image type does not match its bytes",
            )
        })?;
    let header_pixels = u64::from(header_w) * u64::from(header_h);
    if header_pixels > admission.max_image_pixels
        || header_w.max(header_h) > admission.max_image_dimension
    {
        return Err(AttachmentError::new(
            AttachmentErrorCode::ImageDimensionsTooLarge,
            format!(
                "image dimensions {header_w}x{header_h} exceed admission caps ({} px / {}-px side)",
                admission.max_image_pixels, admission.max_image_dimension
            ),
        ));
    }
    let decoded = ImageReader::with_format(std::io::Cursor::new(data), format)
        .decode()
        .map_err(|_| {
            AttachmentError::new(
                AttachmentErrorCode::ImageTypeMismatch,
                "declared image type does not match its bytes",
            )
        })?;
    let (src_w, src_h) = (decoded.width(), decoded.height());
    let src_pixels = u64::from(src_w) * u64::from(src_h);
    if src_pixels > admission.max_image_pixels || src_w.max(src_h) > admission.max_image_dimension {
        return Err(AttachmentError::new(
            AttachmentErrorCode::ImageDimensionsTooLarge,
            format!(
                "image dimensions {src_w}x{src_h} exceed admission caps ({} px / {}-px side)",
                admission.max_image_pixels, admission.max_image_dimension
            ),
        ));
    }

    // Aspect-preserving target under the normalized pixel budget, then the
 // long-edge cap (initialDimensions).
    let (mut w, mut h) = request_image_dimensions(src_w, src_h, policy.max_pixels);
    let long_edge = w.max(h);
    if long_edge > policy.max_dimension {
        let scale = f64::from(policy.max_dimension) / f64::from(long_edge);
        w = (f64::from(w) * scale).floor().max(1.0) as u32;
        h = (f64::from(h) * scale).floor().max(1.0) as u32;
    }
    let resized = if (w, h) != (src_w, src_h) {
        decoded.resize(w, h, FilterType::Triangle)
    } else {
        decoded
    };

    let has_alpha = resized.color().has_alpha();
    let out_media_type = if has_alpha {
        "image/webp".to_string()
    } else {
        "image/jpeg".to_string()
    };
    let mut smallest: Option<Vec<u8>> = None;
    for quality in ENCODING_QUALITIES {
        let bytes = encode(&resized, &out_media_type, quality).map_err(|e| {
            AttachmentError::new(
                AttachmentErrorCode::AttachmentWriteFailed,
                format!("normalization encode failed: {e}"),
            )
        })?;
        if bytes.len() <= policy.max_bytes {
            return Ok(NormalizedImage {
                data: bytes,
                media_type: out_media_type,
                width: w,
                height: h,
                source_dimensions: (src_w, src_h),
            });
        }
        if smallest.as_ref().is_none_or(|b| bytes.len() < b.len()) {
            smallest = Some(bytes);
        }
    }
 // No ladder rung fit: keep the smallest output (isExhaustedEncoding).
    let bytes = smallest.unwrap_or_default();
    Ok(NormalizedImage {
        data: bytes,
        media_type: out_media_type,
        width: w,
        height: h,
        source_dimensions: (src_w, src_h),
    })
}

fn encode(img: &DynamicImage, media_type: &str, quality: u8) -> Result<Vec<u8>, image::ImageError> {
    let mut out = Vec::new();
    match media_type {
        "image/jpeg" => {
            let rgb = img.to_rgb8();
            let mut encoder = image::codecs::jpeg::JpegEncoder::new_with_quality(&mut out, quality);
            encoder.encode_image(&rgb)?;
        }
        "image/webp" => {
            let rgba = img.to_rgba8();
            // image 0.25 ships a lossless WebP encoder only; alpha-bearing
            // sources are stored losslessly. A lossy quality ladder can be
            // added later via the image-webp crate.
            let encoder = image::codecs::webp::WebPEncoder::new_lossless(&mut out);
            encoder.encode(
                rgba.as_raw(),
                rgba.width(),
                rgba.height(),
                image::ExtendedColorType::Rgba8,
            )?;
        }
        _ => unreachable!("normalized media type is jpeg or webp"),
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Minimal CRC32 (PNG chunk checksums) for hand-built headers.
    fn crc32(data: &[u8]) -> u32 {
        let mut table = [0u32; 256];
        for (i, entry) in table.iter_mut().enumerate() {
            let mut c = i as u32;
            for _ in 0..8 {
                c = if c & 1 != 0 { 0xEDB8_8320 ^ (c >> 1) } else { c >> 1 };
            }
            *entry = c;
        }
        let mut crc = 0xFFFF_FFFFu32;
        for &byte in data {
            crc = table[((crc ^ u32::from(byte)) & 0xFF) as usize] ^ (crc >> 8);
        }
        crc ^ 0xFFFF_FFFF
    }

    /// BUG-122: a PNG whose header claims 60000×60000 with a tiny payload
    /// (decompression bomb) is rejected off the HEADER — the dimension
    /// caps must fire before any full-bitmap decode is attempted.
    #[test]
    fn decompression_bomb_is_rejected_before_decoding() {
        let mut ihdr_data = Vec::new();
        ihdr_data.extend_from_slice(&60_000u32.to_be_bytes());
        ihdr_data.extend_from_slice(&60_000u32.to_be_bytes());
        ihdr_data.extend_from_slice(&[8, 2, 0, 0, 0]); // depth, RGB, def, filt, interlace
        let mut ihdr = b"IHDR".to_vec();
        ihdr.extend_from_slice(&ihdr_data);
        let mut png: Vec<u8> = vec![0x89, b'P', b'N', b'G', b'\r', b'\n', 0x1a, b'\n'];
        png.extend_from_slice(&13u32.to_be_bytes());
        png.extend_from_slice(&ihdr);
        png.extend_from_slice(&crc32(&ihdr).to_be_bytes());
        // An empty IDAT plus IEND so the header reader sees a well-formed
        // stream skeleton (the dimension probe never touches the pixels).
        let idat = b"IDAT".to_vec();
        png.extend_from_slice(&0u32.to_be_bytes());
        png.extend_from_slice(&idat);
        png.extend_from_slice(&crc32(&idat).to_be_bytes());
        let iend = b"IEND".to_vec();
        png.extend_from_slice(&0u32.to_be_bytes());
        png.extend_from_slice(&iend);
        png.extend_from_slice(&crc32(&iend).to_be_bytes());

        let limits = ImageAttachmentLimits::default();
        let policy = NormalizationPolicy::default();
        let err = normalize_image(&png, "image/png", &limits, &policy).unwrap_err();
        assert_eq!(
            err.code,
            AttachmentErrorCode::ImageDimensionsTooLarge,
            "header pre-check must fire before decode: {err:?}"
        );
    }

    fn png_bytes(width: u32, height: u32, color: image::Rgba<u8>) -> Vec<u8> {
        let mut buf = Vec::new();
        let img = image::RgbaImage::from_pixel(width, height, color);
        image::DynamicImage::ImageRgba8(img)
            .write_to(&mut std::io::Cursor::new(&mut buf), ImageFormat::Png)
            .unwrap();
        buf
    }

    fn opaque_rgb_png(width: u32, height: u32) -> Vec<u8> {
        let mut buf = Vec::new();
        let img = image::RgbImage::from_pixel(width, height, image::Rgb([10, 20, 30]));
        image::DynamicImage::ImageRgb8(img)
            .write_to(&mut std::io::Cursor::new(&mut buf), ImageFormat::Png)
            .unwrap();
        buf
    }

    #[test]
    fn large_image_is_downscaled_inside_pixel_budget() {
        let limits = ImageAttachmentLimits::default();
        let policy = NormalizationPolicy::default();
        let png = opaque_rgb_png(4096, 4096);
        let out = normalize_image(&png, "image/png", &limits, &policy).unwrap();
        let pixels = u64::from(out.width) * u64::from(out.height);
        assert!(
            pixels <= policy.max_pixels,
            "{pixels} > {}",
            policy.max_pixels
        );
        assert_eq!(out.source_dimensions, (4096, 4096));
        assert_eq!(
            out.media_type, "image/jpeg",
            "opaque png normalizes to jpeg"
        );
        assert!(out.width < 4096 && out.height < 4096);
    }

    #[test]
    fn alpha_sources_become_webp() {
        let png = png_bytes(64, 64, image::Rgba([10, 20, 30, 128]));
        let out = normalize_image(
            &png,
            "image/png",
            &ImageAttachmentLimits::default(),
            &NormalizationPolicy::default(),
        )
        .unwrap();
        assert_eq!(out.media_type, "image/webp");
        assert_eq!(
            (out.width, out.height),
            (64, 64),
            "small images are not enlarged"
        );
    }

    #[test]
    fn small_opaque_image_is_not_enlarged() {
        let png = png_bytes(64, 32, image::Rgba([10, 20, 30, 255]));
        let out = normalize_image(
            &png,
            "image/png",
            &ImageAttachmentLimits::default(),
            &NormalizationPolicy::default(),
        )
        .unwrap();
        assert_eq!((out.width, out.height), (64, 32));
        assert_eq!(out.source_dimensions, (64, 32));
    }

    #[test]
    fn admission_dimension_caps_reject_oversized_sources() {
        let limits = ImageAttachmentLimits {
            max_image_pixels: 1000,
            ..Default::default()
        };
        let png = png_bytes(200, 200, image::Rgba([1, 2, 3, 255]));
        let err = normalize_image(&png, "image/png", &limits, &NormalizationPolicy::default())
            .unwrap_err();
        assert_eq!(err.code, AttachmentErrorCode::ImageDimensionsTooLarge);
    }

    #[test]
    fn unsupported_and_mismatched_media_types_fail() {
        let png = png_bytes(16, 16, image::Rgba([1, 2, 3, 255]));
        let err = normalize_image(
            &png,
            "image/gif",
            &ImageAttachmentLimits::default(),
            &NormalizationPolicy::default(),
        )
        .unwrap_err();
        assert_eq!(err.code, AttachmentErrorCode::UnsupportedImageMediaType);

        let err = normalize_image(
            &png,
            "image/jpeg",
            &ImageAttachmentLimits::default(),
            &NormalizationPolicy::default(),
        )
        .unwrap_err();
        assert_eq!(err.code, AttachmentErrorCode::ImageTypeMismatch);
    }
}
