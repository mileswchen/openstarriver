//! Content-addressed, owner-private local attachment storage (
//! `attachment-local/src/store.ts` parity, Rust port).

use std::io::Write;
use std::path::{Path, PathBuf};

use sha2::{Digest, Sha256};

use super::error::{AttachmentError, AttachmentErrorCode};
use super::normalize::{normalize_image, NormalizedImage};
use super::types::{
    ImageAttachmentLimits, ImageAttachmentRef, NormalizationPolicy, SaveImageAttachment,
    StoredImageAttachment,
};

/// Fully prepared normalized object, verified before any batch member is
/// persisted.
#[derive(Debug, Clone)]
pub struct PreparedImage {
    pub data: Vec<u8>,
    pub reference: ImageAttachmentRef,
}

/// Header-sniffed identity of one stored object (BUG-120/126): media type
/// and dimensions from the first bytes, length from fs metadata — no full
/// read, no decode.
#[derive(Debug, Clone, PartialEq)]
pub struct ImageInfo {
    pub media_type: String,
    pub bytes: u64,
    pub width: u32,
    pub height: u32,
}

/// Content-addressed attachment store under `<root>/objects/<sha[:2]>/<sha>`.
pub struct AttachmentStore {
    root: PathBuf,
    limits: ImageAttachmentLimits,
    policy: NormalizationPolicy,
}

impl AttachmentStore {
    pub fn new(root: PathBuf, limits: ImageAttachmentLimits, policy: NormalizationPolicy) -> Self {
        Self {
            root,
            limits,
            policy,
        }
    }

    pub fn limits(&self) -> &ImageAttachmentLimits {
        &self.limits
    }

    /// Validate + normalize every member, then commit all. Validation failures
    /// start no writes; a batch commits atomically (already-published objects
    /// are reused by content address).
    pub fn save_images(
        &self,
        inputs: &[SaveImageAttachment],
    ) -> Result<Vec<ImageAttachmentRef>, AttachmentError> {
        let prepared: Vec<PreparedImage> = inputs
            .iter()
            .map(|input| self.prepare(input))
            .collect::<Result<_, _>>()?;
        prepared
            .iter()
            .map(|p| self.commit(p))
            .collect::<Result<_, _>>()
    }

    /// Decode, normalize, and verify one submitted image without touching
    /// storage.
    pub fn prepare(&self, input: &SaveImageAttachment) -> Result<PreparedImage, AttachmentError> {
        let normalized =
            normalize_image(&input.data, &input.media_type, &self.limits, &self.policy)?;
        let sha = digest_hex(&normalized.data);
        let downscaled = (normalized.width, normalized.height) != normalized.source_dimensions;
        let reference = ImageAttachmentRef {
            attachment_id: format!("sha256:{sha}"),
            media_type: normalized.media_type.clone(),
            width: normalized.width,
            height: normalized.height,
            bytes: normalized.data.len(),
            name: display_name(input.name.as_deref()),
            original_dimensions: downscaled.then_some(normalized.source_dimensions),
        };
        Ok(PreparedImage {
            data: normalized.data,
            reference,
        })
    }

    /// Publish one prepared object atomically: staging file + fsync + link
    /// into the content-addressed bucket. Concurrent publication of the same
    /// digest is safe (link EEXIST wins; the object is identical by address).
    pub fn commit(&self, prepared: &PreparedImage) -> Result<ImageAttachmentRef, AttachmentError> {
        let sha = ensure_reference(&prepared.reference)?;
        if digest_hex(&prepared.data) != sha {
            return Err(AttachmentError::new(
                AttachmentErrorCode::AttachmentCorrupt,
                "prepared attachment bytes do not match their reference",
            ));
        }
        let bucket = self.root.join("objects").join(&sha[0..2]);
        let target = bucket.join(&sha);
        if target.exists() {
            return Ok(prepared.reference.clone());
        }
        let staging_dir = self.root.join("tmp");
        create_dir_private(&staging_dir)?;
        let staging = staging_dir.join(uuid::Uuid::new_v4().to_string());
        let mut file = std::fs::File::create(&staging).map_err(|e| io_err(&e))?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(&staging, std::fs::Permissions::from_mode(0o600));
        }
        file.write_all(&prepared.data).map_err(|e| io_err(&e))?;
        file.sync_all().map_err(|e| io_err(&e))?;
        drop(file);

        create_dir_private(&bucket)?;
        match std::fs::hard_link(&staging, &target) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(e) => return Err(io_err(&e)),
        }
        let _ = std::fs::remove_file(&staging);
        // Make the publication durable: the directory entries must reach
 // storage, not just the file bytes (syncDirectory).
        let _ = sync_dir(&bucket);
        let _ = sync_dir(&self.root);
        Ok(prepared.reference.clone())
    }

    /// Read a stored normalized image by its `sha256:<hex>` reference.
    pub fn read_image(&self, id: &str) -> Result<StoredImageAttachment, AttachmentError> {
        let sha = parse_reference(id)?;
        let path = self.root.join("objects").join(&sha[0..2]).join(&sha);
        let data = std::fs::read(&path).map_err(|e| io_err(&e))?;
        if digest_hex(&data) != sha {
            return Err(AttachmentError::new(
                AttachmentErrorCode::AttachmentCorrupt,
                "stored attachment bytes do not match their reference",
            ));
        }
        // Re-derive the durable facts from the stored bytes (media type from
        // the actual encoding, dimensions from a decode).
        let normalized = normalized_from_stored(&data)?;
        let reference = ImageAttachmentRef {
            attachment_id: format!("sha256:{sha}"),
            media_type: normalized.media_type,
            width: normalized.width,
            height: normalized.height,
            bytes: data.len(),
            name: None,
            original_dimensions: None,
        };
        Ok(StoredImageAttachment { reference, data })
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Normalized payload byte length for one `sha256:<hex>` reference —
    /// an `fs::metadata` lookup on the content-addressed object, no read,
    /// no decode (WI-159 request-budget accounting). `None` = not stored.
    pub fn image_bytes(&self, id: &str) -> Option<u64> {
        std::fs::metadata(self.object_path(id)?).ok().map(|meta| meta.len())
    }

    /// Cheap identity probe (BUG-120/126): the fs-metadata length plus a
    /// header sniff for the media type AND dimensions — no full read, no
    /// decode, no digest re-verify. `None` = missing, invalid, or a stored
    /// object outside the raster vocabulary.
    pub fn image_info(&self, id: &str) -> Option<ImageInfo> {
        use std::io::Read as _;
        let path = self.object_path(id)?;
        let len = std::fs::metadata(&path).ok()?.len();
        let mut head = vec![0u8; 8192.min(len as usize)];
        std::fs::File::open(&path).ok()?.read_exact(&mut head).ok()?;
        let media_type = if head.starts_with(&[0x89, b'P', b'N', b'G']) {
            "image/png"
        } else if head.starts_with(&[0xFF, 0xD8]) {
            "image/jpeg"
        } else if head.len() >= 12 && &head[0..4] == b"RIFF" && &head[8..12] == b"WEBP" {
            "image/webp"
        } else {
            return None;
        };
        let (width, height) = image::ImageReader::new(std::io::Cursor::new(&head))
            .with_guessed_format()
            .ok()?
            .into_dimensions()
            .ok()?;
        Some(ImageInfo {
            media_type: media_type.to_string(),
            bytes: len,
            width,
            height,
        })
    }

    /// BUG-126: the payload read for wire/thumbnail paths — full bytes,
    /// but no digest re-verify and no decode (objects were verified at
    /// commit time; the metadata comes from the header sniff).
    pub fn read_image_light(&self, id: &str) -> Option<StoredImageAttachment> {
        let info = self.image_info(id)?;
        let data = std::fs::read(self.object_path(id)?).ok()?;
        Some(StoredImageAttachment {
            reference: ImageAttachmentRef {
                attachment_id: id.to_string(),
                media_type: info.media_type,
                width: info.width,
                height: info.height,
                bytes: info.bytes as usize,
                name: None,
                original_dimensions: None,
            },
            data,
        })
    }

    /// Content-addressed object path for a `sha256:<hex>` reference.
    fn object_path(&self, id: &str) -> Option<PathBuf> {
        let sha = parse_reference(id).ok()?;
        Some(self.root.join("objects").join(&sha[0..2]).join(&sha))
    }
}

fn normalized_from_stored(data: &[u8]) -> Result<NormalizedImage, AttachmentError> {
    // The stored vocabulary is png/jpeg/webp; guess from bytes.
    let reader = image::ImageReader::new(std::io::Cursor::new(data))
        .with_guessed_format()
        .map_err(|e| io_err(&e))?;
    let format = reader.format().ok_or_else(|| {
        AttachmentError::new(
            AttachmentErrorCode::AttachmentIo,
            "stored attachment has an unknown image format",
        )
    })?;
    let media_type = match format {
        image::ImageFormat::Png => "image/png",
        image::ImageFormat::Jpeg => "image/jpeg",
        image::ImageFormat::WebP => "image/webp",
        _ => {
            return Err(AttachmentError::new(
                AttachmentErrorCode::AttachmentIo,
                "stored attachment is outside the raster vocabulary",
            ))
        }
    };
    let decoded = reader.decode().map_err(|e| {
        AttachmentError::new(
            AttachmentErrorCode::AttachmentIo,
            format!("stored attachment decode failed: {e}"),
        )
    })?;
    Ok(NormalizedImage {
        data: data.to_vec(),
        media_type: media_type.to_string(),
        width: decoded.width(),
        height: decoded.height(),
        source_dimensions: (decoded.width(), decoded.height()),
    })
}

fn digest_hex(data: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(data);
    hasher
        .finalize()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

/// Strip both separator styles and control characters, cap at 255 bytes
/// (`displayName`).
pub(crate) fn display_name(value: Option<&str>) -> Option<String> {
    let value = value?;
    let leaf = value.rsplit(['/', '\\']).next().unwrap_or(value);
    let clean: String = leaf
        .chars()
        .filter(|c| !c.is_control())
        .collect::<String>()
        .trim()
        .chars()
        .take(255)
        .collect();
    (!clean.is_empty()).then_some(clean)
}

fn ensure_reference(reference: &ImageAttachmentRef) -> Result<String, AttachmentError> {
    parse_reference(&reference.attachment_id)
}

fn parse_reference(id: &str) -> Result<String, AttachmentError> {
    let sha = id
        .strip_prefix("sha256:")
        .filter(|s| s.len() == 64 && s.bytes().all(|b| b.is_ascii_hexdigit()))
        .ok_or_else(|| {
            AttachmentError::new(
                AttachmentErrorCode::InvalidAttachmentRef,
                "attachment reference must be sha256:<64 hex digits>",
            )
        })?;
    Ok(sha.to_string())
}

fn create_dir_private(path: &Path) -> Result<(), AttachmentError> {
    std::fs::create_dir_all(path).map_err(|e| io_err(&e))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700));
    }
    Ok(())
}

fn sync_dir(path: &Path) -> std::io::Result<()> {
    std::fs::File::open(path)?.sync_all()
}

fn io_err(e: &std::io::Error) -> AttachmentError {
    AttachmentError::new(
        AttachmentErrorCode::AttachmentIo,
        format!("attachment storage IO failed: {e}"),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    #[cfg(unix)]
    use std::os::unix::fs::PermissionsExt;

    fn png_bytes(width: u32, height: u32, alpha: bool) -> Vec<u8> {
        let mut buf = Vec::new();
        if alpha {
            let img = image::RgbaImage::from_pixel(width, height, image::Rgba([10, 20, 30, 128]));
            image::DynamicImage::ImageRgba8(img)
                .write_to(&mut std::io::Cursor::new(&mut buf), image::ImageFormat::Png)
                .unwrap();
        } else {
            let img = image::RgbImage::from_pixel(width, height, image::Rgb([10, 20, 30]));
            image::DynamicImage::ImageRgb8(img)
                .write_to(&mut std::io::Cursor::new(&mut buf), image::ImageFormat::Png)
                .unwrap();
        }
        buf
    }

    fn store(dir: &Path) -> AttachmentStore {
        AttachmentStore::new(
            dir.join("attachments").join("v1"),
            ImageAttachmentLimits::default(),
            NormalizationPolicy::default(),
        )
    }

    #[test]
    fn save_and_read_roundtrip_with_permissions() {
        let dir = tempfile::tempdir().unwrap();
        let store = store(dir.path());
        let refs = store
            .save_images(&[SaveImageAttachment {
                name: Some("photo.png".into()),
                media_type: "image/png".into(),
                data: png_bytes(128, 96, false),
            }])
            .unwrap();
        assert_eq!(refs.len(), 1);
        assert!(refs[0].attachment_id.starts_with("sha256:"));
        assert_eq!(refs[0].name.as_deref(), Some("photo.png"));
        assert_eq!((refs[0].width, refs[0].height), (128, 96));
        assert!(refs[0].original_dimensions.is_none());

        let stored = store.read_image(&refs[0].attachment_id).unwrap();
        assert_eq!(stored.reference.attachment_id, refs[0].attachment_id);
        assert!(!stored.data.is_empty());

        // Permissions: object file 0600, directories 0700 (POSIX modes;
        // Windows ACLs have no direct equivalent assertion).
        #[cfg(unix)]
        {
            let sha = refs[0].attachment_id.trim_start_matches("sha256:");
            let object = store.root().join("objects").join(&sha[0..2]).join(sha);
            assert_eq!(
                object.metadata().unwrap().permissions().mode() & 0o777,
                0o600
            );
            let bucket = store.root().join("objects").join(&sha[0..2]);
            assert_eq!(
                bucket.metadata().unwrap().permissions().mode() & 0o777,
                0o700
            );
        }
    }

    #[test]
    fn same_content_address_is_idempotent() {
        let dir = tempfile::tempdir().unwrap();
        let store = store(dir.path());
        let input = SaveImageAttachment {
            name: None,
            media_type: "image/png".into(),
            data: png_bytes(64, 64, true),
        };
        let a = store.save_images(&[input.clone()]).unwrap();
        let b = store.save_images(&[input]).unwrap();
        assert_eq!(a[0].attachment_id, b[0].attachment_id);
        assert_eq!(a[0].media_type, "image/webp", "alpha png → webp");
    }

    #[test]
    fn downscaled_sources_record_original_dimensions() {
        let dir = tempfile::tempdir().unwrap();
        let store = store(dir.path());
        let refs = store
            .save_images(&[SaveImageAttachment {
                name: None,
                media_type: "image/png".into(),
                data: png_bytes(4096, 2048, false),
            }])
            .unwrap();
        assert_eq!(refs[0].original_dimensions, Some((4096, 2048)));
        let pixels = u64::from(refs[0].width) * u64::from(refs[0].height);
        assert!(
            pixels <= 2048 * 2048,
            "downscaled {pixels} exceeds the normalized pixel budget"
        );
    }

    #[test]
    fn invalid_reference_and_corrupt_object_fail() {
        let dir = tempfile::tempdir().unwrap();
        let store = store(dir.path());
        let err = store.read_image("not-a-ref").unwrap_err();
        assert_eq!(err.code, AttachmentErrorCode::InvalidAttachmentRef);
        let err = store
            .read_image(&format!("sha256:{}", "0".repeat(64)))
            .unwrap_err();
        assert_eq!(err.code, AttachmentErrorCode::AttachmentIo);
    }

    /// WI-159/WI-160: budget accounting reads the object's fs metadata —
    /// same length as the stored bytes, no read; unknown/invalid refs are
    /// None (never a panic, never a fake zero).
    #[test]
    fn image_bytes_reads_fs_metadata_only() {
        let dir = tempfile::tempdir().unwrap();
        let store = store(dir.path());
        let refs = store
            .save_images(&[SaveImageAttachment {
                name: None,
                media_type: "image/png".into(),
                data: png_bytes(64, 64, true),
            }])
            .unwrap();
        let id = refs[0].attachment_id.clone();
        let sha = id.trim_start_matches("sha256:").to_string();
        let on_disk = std::fs::read(
            store
                .root()
                .join("objects")
                .join(&sha[0..2])
                .join(&sha),
        )
        .unwrap()
        .len() as u64;
        assert_eq!(store.image_bytes(&id), Some(on_disk));
        assert_eq!(store.image_bytes("not-a-ref"), None, "invalid ref");
        assert_eq!(
            store.image_bytes(&format!("sha256:{}", "0".repeat(64))),
            None,
            "missing object"
        );
    }

    /// BUG-120/126: image_info sniffs the media type AND dimensions from
    /// the header bytes and takes the length from fs metadata — no full
    /// read, no decode — and refuses objects outside the raster
    /// vocabulary.
    #[test]
    fn image_info_sniffs_media_type_without_decoding() {
        let dir = tempfile::tempdir().unwrap();
        let store = store(dir.path());
        let refs = store
            .save_images(&[SaveImageAttachment {
                name: None,
                media_type: "image/png".into(),
                data: png_bytes(32, 32, true),
            }])
            .unwrap();
        let id = refs[0].attachment_id.clone();
        // Normalization stores webp for alpha png input, 32x32.
        let info = store.image_info(&id).expect("stored object probed");
        assert_eq!(info.media_type, "image/webp");
        assert_eq!((info.width, info.height), (32, 32));
        assert_eq!(info.bytes, store.image_bytes(&id).unwrap());
        assert_eq!(store.image_info("not-a-ref"), None);
        assert_eq!(
            store.image_info(&format!("sha256:{}", "0".repeat(64))),
            None
        );
        // A non-raster object under the store root is refused, not guessed.
        let sha = id.trim_start_matches("sha256:").to_string();
        let object = store
            .root()
            .join("objects")
            .join(&sha[0..2])
            .join(&sha);
        std::fs::write(&object, b"not an image at all").unwrap();
        assert_eq!(store.image_info(&id), None, "unrecognized magic");
    }

    /// BUG-126: the light payload read returns the full bytes with
    /// sniffed metadata — no digest re-verify, no decode.
    #[test]
    fn read_image_light_returns_payload_with_sniffed_metadata() {
        let dir = tempfile::tempdir().unwrap();
        let store = store(dir.path());
        let refs = store
            .save_images(&[SaveImageAttachment {
                name: Some("light.png".into()),
                media_type: "image/png".into(),
                data: png_bytes(24, 16, false),
            }])
            .unwrap();
        let id = refs[0].attachment_id.clone();
        let light = store.read_image_light(&id).expect("light read");
        assert_eq!(light.reference.attachment_id, id);
        assert_eq!(light.reference.media_type, "image/jpeg");
        assert_eq!((light.reference.width, light.reference.height), (24, 16));
        assert_eq!(
            light.data.len() as u64,
            store.image_bytes(&id).unwrap(),
            "payload length matches the object"
        );
        assert!(store.read_image_light("not-a-ref").is_none());
    }
}
