//! Attachment wire types: admission limits, normalization policy, refs, and
//! stored images. Mirrors `ImageAttachmentLimits` / `NormalizationPolicy`
//! defaults.

/// Default maximum encoded bytes accepted for one submitted image (20 MiB).
pub const DEFAULT_MAX_IMAGE_BYTES: usize = 20 * 1024 * 1024;
/// Default maximum image count accepted in one submitted batch (20).
pub const DEFAULT_MAX_IMAGES_PER_BATCH: usize = 20;
/// Default maximum aggregate encoded image bytes in one submitted batch (200 MiB).
pub const DEFAULT_MAX_AGGREGATE_BYTES: usize = 200 * 1024 * 1024;
/// Default maximum intrinsic width × height for one submitted image.
pub const DEFAULT_MAX_IMAGE_PIXELS: u64 = 64_000_000;
/// Default per-side pixel cap for one submitted image (8192 px).
pub const DEFAULT_MAX_IMAGE_DIMENSION: u32 = 8192;
/// Default total-pixel budget of the stored normalized image (2048 × 2048).
pub const DEFAULT_NORMALIZED_MAX_PIXELS: u64 = 2048 * 2048;
/// Default long-edge cap of the stored normalized image (8192 px).
pub const DEFAULT_NORMALIZED_MAX_DIMENSION: u32 = 8192;
/// Default encoded-byte target for one stored normalized image (4 MiB).
pub const DEFAULT_NORMALIZED_MAX_BYTES: usize = 4 * 1024 * 1024;

/// Source admission policy for submitted image batches.
#[derive(Debug, Clone)]
pub struct ImageAttachmentLimits {
    pub max_image_bytes: usize,
    pub max_images_per_batch: usize,
    pub max_aggregate_bytes: usize,
    pub max_image_pixels: u64,
    pub max_image_dimension: u32,
}

impl Default for ImageAttachmentLimits {
    fn default() -> Self {
        Self {
            max_image_bytes: DEFAULT_MAX_IMAGE_BYTES,
            max_images_per_batch: DEFAULT_MAX_IMAGES_PER_BATCH,
            max_aggregate_bytes: DEFAULT_MAX_AGGREGATE_BYTES,
            max_image_pixels: DEFAULT_MAX_IMAGE_PIXELS,
            max_image_dimension: DEFAULT_MAX_IMAGE_DIMENSION,
        }
    }
}

/// Deterministic normalization policy for the stored normalized image.
#[derive(Debug, Clone)]
pub struct NormalizationPolicy {
    /// Total-pixel budget; larger sources are downscaled proportionally.
    pub max_pixels: u64,
    /// Long-edge cap applied after the total-pixel budget.
    pub max_dimension: u32,
    /// Encoded-byte target for the quality ladder; the smallest ladder output
    /// is kept when no quality fits.
    pub max_bytes: usize,
}

impl Default for NormalizationPolicy {
    fn default() -> Self {
        Self {
            max_pixels: DEFAULT_NORMALIZED_MAX_PIXELS,
            max_dimension: DEFAULT_NORMALIZED_MAX_DIMENSION,
            max_bytes: DEFAULT_NORMALIZED_MAX_BYTES,
        }
    }
}

/// One encoded image upload in wire form (base64 payload, before decoding).
#[derive(Debug, Clone)]
pub struct EncodedImageAttachment {
    pub name: Option<String>,
    pub media_type: String,
    pub data_base64: String,
}

/// One decoded image ready for validation / normalization.
#[derive(Debug, Clone)]
pub struct SaveImageAttachment {
    pub name: Option<String>,
    pub media_type: String,
    pub data: Vec<u8>,
}

/// Durable reference to one stored normalized image.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImageAttachmentRef {
    /// `sha256:<hex>` content address.
    pub attachment_id: String,
    pub media_type: String,
    pub width: u32,
    pub height: u32,
    pub bytes: usize,
    pub name: Option<String>,
    /// Present when normalization downscaled the source.
    pub original_dimensions: Option<(u32, u32)>,
}

/// The stored normalized bytes beside their durable reference.
#[derive(Debug, Clone)]
pub struct StoredImageAttachment {
    pub reference: ImageAttachmentRef,
    pub data: Vec<u8>,
}

/// Aspect-preserving integer dimensions within a hard total-pixel budget
/// (`requestImageDimensions`): inward-rounded, small images not enlarged.
pub fn request_image_dimensions(width: u32, height: u32, max_pixels: u64) -> (u32, u32) {
    let pixels = u64::from(width) * u64::from(height);
    if pixels <= max_pixels {
        return (width, height);
    }
    let scale = (max_pixels as f64 / pixels as f64).sqrt();
    if width >= height {
        let mut projected_width = (f64::from(width) * scale).floor().max(1.0) as u32;
        let mut projected_height = (f64::from(projected_width) * f64::from(height)
            / f64::from(width))
        .round()
        .max(1.0) as u32;
        while u64::from(projected_width) * u64::from(projected_height) > max_pixels
            && projected_width > 1
        {
            projected_width -= 1;
            projected_height = (f64::from(projected_width) * f64::from(height) / f64::from(width))
                .round()
                .max(1.0) as u32;
        }
        (projected_width, projected_height)
    } else {
        let mut projected_height = (f64::from(height) * scale).floor().max(1.0) as u32;
        let mut projected_width = (f64::from(projected_height) * f64::from(width)
            / f64::from(height))
        .round()
        .max(1.0) as u32;
        while u64::from(projected_width) * u64::from(projected_height) > max_pixels
            && projected_height > 1
        {
            projected_height -= 1;
            projected_width = (f64::from(projected_height) * f64::from(width) / f64::from(height))
                .round()
                .max(1.0) as u32;
        }
        (projected_width, projected_height)
    }
}
