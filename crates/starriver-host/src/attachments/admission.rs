//! Wire-form admission of base64-encoded image uploads (
//! `attachment/src/admission.ts` parity).

use base64::Engine;

use super::error::{AttachmentError, AttachmentErrorCode};
use super::types::{EncodedImageAttachment, ImageAttachmentLimits, SaveImageAttachment};

/// Decode one upload payload while rejecting non-canonical base64 forms.
/// Whitespace, URL-safe aliases, and trailing garbage all fail.
pub fn decode_canonical_base64(data: &str) -> Result<Vec<u8>, AttachmentError> {
    if data.is_empty() {
        return Err(AttachmentError::new(
            AttachmentErrorCode::InvalidImageBase64,
            "image upload is empty",
        ));
    }
    let engine = base64::engine::general_purpose::STANDARD;
    let decoded = engine.decode(data).map_err(|_| {
        AttachmentError::new(
            AttachmentErrorCode::InvalidImageBase64,
            "image upload is not canonical base64",
        )
    })?;
    // Canonicality: re-encoding must reproduce the exact input.
    if engine.encode(&decoded) != data {
        return Err(AttachmentError::new(
            AttachmentErrorCode::InvalidImageBase64,
            "image upload is not canonical base64",
        ));
    }
    Ok(decoded)
}

/// Admit one wire image batch: enforce canonical base64 on every member, then
/// per-image byte, aggregate byte, and count caps. Validation happens before
/// any member is returned, so a refused batch starts no writes.
pub fn admit_encoded_images(
    images: &[EncodedImageAttachment],
    limits: &ImageAttachmentLimits,
) -> Result<Vec<SaveImageAttachment>, AttachmentError> {
    if images.len() > limits.max_images_per_batch {
        return Err(AttachmentError::new(
            AttachmentErrorCode::TooManyImages,
            format!(
                "image batch exceeds the maximum of {} images",
                limits.max_images_per_batch
            ),
        ));
    }
    let mut out = Vec::with_capacity(images.len());
    let mut aggregate = 0usize;
    for image in images {
        let data = decode_canonical_base64(&image.data_base64)?;
        if data.is_empty() {
            return Err(AttachmentError::new(
                AttachmentErrorCode::InvalidImage,
                "image is empty",
            ));
        }
        if data.len() > limits.max_image_bytes {
            return Err(AttachmentError::new(
                AttachmentErrorCode::ImageTooLarge,
                format!(
                    "image exceeds the per-image limit of {} bytes",
                    limits.max_image_bytes
                ),
            ));
        }
        aggregate = aggregate.saturating_add(data.len());
        if aggregate > limits.max_aggregate_bytes {
            return Err(AttachmentError::new(
                AttachmentErrorCode::ImageBatchTooLarge,
                format!(
                    "image batch exceeds the aggregate limit of {} bytes",
                    limits.max_aggregate_bytes
                ),
            ));
        }
        out.push(SaveImageAttachment {
            name: image.name.clone(),
            media_type: image.media_type.clone(),
            data,
        });
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn b64(bytes: &[u8]) -> String {
        base64::engine::general_purpose::STANDARD.encode(bytes)
    }

    #[test]
    fn canonical_base64_roundtrip_and_rejections() {
        let bytes = b"hello attachment";
        let encoded = b64(bytes);
        assert_eq!(decode_canonical_base64(&encoded).unwrap(), bytes);

        // URL-safe alphabet is not canonical.
        let url_safe = encoded.replace('/', "_").replace('+', "-");
        if url_safe != encoded {
            assert!(decode_canonical_base64(&url_safe).is_err());
        }
        // Whitespace is not canonical.
        let spaced = format!("{}\n", encoded);
        assert!(decode_canonical_base64(&spaced).is_err());
        // Empty payload is rejected.
        assert!(decode_canonical_base64("").is_err());
        // Garbage is rejected.
        assert!(decode_canonical_base64("!!!not-base64!!!").is_err());
    }

    #[test]
    fn batch_admission_enforces_count_and_aggregate() {
        let limits = ImageAttachmentLimits {
            max_image_bytes: 1000,
            max_images_per_batch: 2,
            max_aggregate_bytes: 1500,
            ..Default::default()
        };
        let one = EncodedImageAttachment {
            name: None,
            media_type: "image/png".into(),
            data_base64: b64(&vec![0u8; 800]),
        };
        // Two members fit the count cap but blow the aggregate cap.
        let err = admit_encoded_images(&[one.clone(), one.clone()], &limits).unwrap_err();
        assert_eq!(err.code, AttachmentErrorCode::ImageBatchTooLarge);
        // Three members blow the count cap.
        let three = vec![one.clone(), one.clone(), one];
        let err = admit_encoded_images(&three, &limits).unwrap_err();
        assert_eq!(err.code, AttachmentErrorCode::TooManyImages);
        // One oversized member fails per-image.
        let big = EncodedImageAttachment {
            name: None,
            media_type: "image/png".into(),
            data_base64: b64(&vec![0u8; 2000]),
        };
        let err = admit_encoded_images(&[big], &limits).unwrap_err();
        assert_eq!(err.code, AttachmentErrorCode::ImageTooLarge);
    }
}
