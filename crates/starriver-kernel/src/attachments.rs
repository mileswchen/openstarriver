//! Attachment seam (`attachment` family parity): content-addressed,
//! durable raster attachments behind a kernel-owned service, so loop-side
//! consumers (image projection, tools) resolve references without knowing
//! the host storage layout. The host's concrete store registers the default
//! implementation through `ctx.provide`; swap implementations without
//! touching consumers.
//!
//! Event vocabulary for lifecycle records (log-only, emitted by hosts that
//! have a session scope at admission time):
//! [`EVENT_ATTACHMENT_ADMITTED`] / [`EVENT_ATTACHMENT_REMOVED`].

use std::sync::Arc;

/// Session-event type: an attachment was admitted (id, media type, size,
/// digest).
pub const EVENT_ATTACHMENT_ADMITTED: &str = "attachment/admitted";
/// Session-event type: an attachment was removed.
pub const EVENT_ATTACHMENT_REMOVED: &str = "attachment/removed";

/// A resolved stored image, wire-neutral (base64 payload included).
#[derive(Debug, Clone)]
pub struct StoredImage {
    pub media_type: String,
    pub base64: String,
}

/// One admission candidate: a single image submission (canonical base64).
#[derive(Debug, Clone)]
pub struct AdmissionCandidate {
    pub name: Option<String>,
    pub media_type: String,
    pub data_base64: String,
}

/// Admission outcome per candidate, in input order (a refused batch starts
/// no writes and errors instead).
#[derive(Debug, Clone)]
pub struct AdmittedAttachment {
    pub id: String,
    pub media_type: String,
    pub width: u32,
    pub height: u32,
    pub bytes: u32,
    pub name: Option<String>,
    pub original_width: Option<u32>,
    pub original_height: Option<u32>,
}

/// The attachment storage surface: admission (limits/normalization are part
/// of the implementation's policy) plus the read side.
pub trait AttachmentStoreApi: Send + Sync {
    /// Validate, normalize, and durably store a batch. Validation failures
    /// start no writes; already-published objects are reused by content.
    fn admit_images(
        &self,
        images: &[AdmissionCandidate],
    ) -> Result<Vec<AdmittedAttachment>, String>;
    fn read_image(&self, file_id: &str) -> Option<StoredImage>;
    fn exists(&self, file_id: &str) -> bool;
    /// Normalized payload byte length for request-budget accounting
    /// (WI-159). The default derives it from a full read (ceil — a ≤2-byte
    /// overestimate is the safe direction for a budget); implementations
    /// with a cheaper metadata path should override.
    fn byte_len(&self, file_id: &str) -> Option<u64> {
        self.read_image(file_id)
            .map(|image| image.base64.len().div_ceil(4) as u64 * 3)
    }
}

/// TypedService wrapper handed to consumers through `ctx.require::<AttachmentStore>()`.
#[derive(Clone)]
pub struct AttachmentStore {
    inner: Arc<dyn AttachmentStoreApi>,
}

impl AttachmentStore {
    pub fn new(inner: Arc<dyn AttachmentStoreApi>) -> Self {
        Self { inner }
    }
}

impl AttachmentStoreApi for AttachmentStore {
    fn admit_images(
        &self,
        images: &[AdmissionCandidate],
    ) -> Result<Vec<AdmittedAttachment>, String> {
        self.inner.admit_images(images)
    }
    fn read_image(&self, file_id: &str) -> Option<StoredImage> {
        self.inner.read_image(file_id)
    }
    fn exists(&self, file_id: &str) -> bool {
        self.inner.exists(file_id)
    }
    fn byte_len(&self, file_id: &str) -> Option<u64> {
        self.inner.byte_len(file_id)
    }
}

impl cordis_rslite::TypedService for AttachmentStore {
    const NAME: &'static str = "seam:attachment";
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use std::sync::Mutex;

    struct FakeStore {
        images: Mutex<HashMap<String, StoredImage>>,
    }

    impl AttachmentStoreApi for FakeStore {
        fn admit_images(
            &self,
            images: &[AdmissionCandidate],
        ) -> Result<Vec<AdmittedAttachment>, String> {
            Ok(images
                .iter()
                .enumerate()
                .map(|(i, c)| AdmittedAttachment {
                    id: format!("sha256:{i}"),
                    media_type: c.media_type.clone(),
                    width: 1,
                    height: 1,
                    bytes: c.data_base64.len() as u32,
                    name: c.name.clone(),
                    original_width: None,
                    original_height: None,
                })
                .collect())
        }
        fn read_image(&self, file_id: &str) -> Option<StoredImage> {
            self.images.lock().unwrap().get(file_id).cloned()
        }
        fn exists(&self, file_id: &str) -> bool {
            self.images.lock().unwrap().contains_key(file_id)
        }
    }

    #[test]
    fn wrapper_delegates_to_the_inner_store() {
        let mut images = HashMap::new();
        images.insert(
            "sha256:abc".to_string(),
            StoredImage {
                media_type: "image/png".into(),
                base64: "AAAA".into(),
            },
        );
        let service = AttachmentStore::new(Arc::new(FakeStore {
            images: Mutex::new(images),
        }));
        assert!(service.exists("sha256:abc"));
        assert_eq!(service.read_image("sha256:abc").unwrap().base64, "AAAA");
        assert!(service.read_image("sha256:gone").is_none());
    }

    /// The seam registers as a named typed service: `ctx.provide` +
    /// `ctx.require` round-trip on one context.
    #[test]
    fn seam_resolves_through_the_context() {
        let ctx = cordis_rslite::Context::new();
        let service = AttachmentStore::new(Arc::new(FakeStore {
            images: Mutex::new(HashMap::from([(
                "sha256:abc".to_string(),
                StoredImage {
                    media_type: "image/png".into(),
                    base64: "AAAA".into(),
                },
            )])),
        }));
        ctx.provide::<AttachmentStore>(Arc::new(service)).unwrap();
        let resolved = ctx.require::<AttachmentStore>().unwrap();
        assert!(resolved.exists("sha256:abc"));
        assert!(resolved.read_image("sha256:abc").is_some());
    }
}
