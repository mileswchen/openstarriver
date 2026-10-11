//! Stable error taxonomy for the attachment pipeline. Codes mirror
//! `AttachmentError` so product surfaces can map failures without parsing
//! messages.

/// Stable machine-readable attachment failure codes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AttachmentErrorCode {
    /// Upload payload is not canonical base64.
    InvalidImageBase64,
    /// The image payload is empty.
    InvalidImage,
    /// One image exceeds the per-image encoded-byte cap.
    ImageTooLarge,
    /// The batch exceeds the aggregate encoded-byte cap.
    ImageBatchTooLarge,
    /// The batch exceeds the per-message image-count cap.
    TooManyImages,
    /// Intrinsic pixels or a side exceed the admission caps.
    ImageDimensionsTooLarge,
    /// Declared media type does not match the decoded bytes.
    ImageTypeMismatch,
    /// The media type is outside the supported raster vocabulary.
    UnsupportedImageMediaType,
    /// The attachment reference grammar is invalid.
    InvalidAttachmentRef,
    /// Stored bytes do not match their reference digest.
    AttachmentCorrupt,
    /// A storage write or read failed.
    AttachmentIo,
    /// Normalization produced bytes inconsistent with the claimed facts.
    AttachmentWriteFailed,
}

impl AttachmentErrorCode {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::InvalidImageBase64 => "INVALID_IMAGE_BASE64",
            Self::InvalidImage => "INVALID_IMAGE",
            Self::ImageTooLarge => "IMAGE_TOO_LARGE",
            Self::ImageBatchTooLarge => "IMAGE_BATCH_TOO_LARGE",
            Self::TooManyImages => "TOO_MANY_IMAGES",
            Self::ImageDimensionsTooLarge => "IMAGE_DIMENSIONS_TOO_LARGE",
            Self::ImageTypeMismatch => "IMAGE_TYPE_MISMATCH",
            Self::UnsupportedImageMediaType => "UNSUPPORTED_IMAGE_MEDIA_TYPE",
            Self::InvalidAttachmentRef => "INVALID_ATTACHMENT_REF",
            Self::AttachmentCorrupt => "ATTACHMENT_CORRUPT",
            Self::AttachmentIo => "ATTACHMENT_IO",
            Self::AttachmentWriteFailed => "ATTACHMENT_WRITE_FAILED",
        }
    }
}

/// Attachment pipeline error: stable code + human message.
#[derive(Debug, Clone)]
pub struct AttachmentError {
    pub code: AttachmentErrorCode,
    pub message: String,
}

impl AttachmentError {
    pub fn new(code: AttachmentErrorCode, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
        }
    }
}

impl std::fmt::Display for AttachmentError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: {}", self.code.as_str(), self.message)
    }
}

impl std::error::Error for AttachmentError {}
