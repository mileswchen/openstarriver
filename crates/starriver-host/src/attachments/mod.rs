pub mod admission;
pub mod error;
pub mod normalize;
pub mod store;
pub mod types;

pub use admission::{admit_encoded_images, decode_canonical_base64};
pub use error::{AttachmentError, AttachmentErrorCode};
pub use normalize::{normalize_image, NormalizedImage};
pub use store::{AttachmentStore, PreparedImage};
pub use types::{
    EncodedImageAttachment, ImageAttachmentLimits, ImageAttachmentRef, NormalizationPolicy,
    SaveImageAttachment, StoredImageAttachment,
};

// Image attachment admission + content-addressed storage for the desktop
// surface (LLP P0 · attachment seam parity).

use std::sync::Arc;

use base64::Engine;
use starriver_kernel::json::JsonValue;
use starriver_kernel::llm::ContentBlock;

use crate::{types as host_types, HostRuntime};

impl HostRuntime {
    /// Admit one wire image batch: canonical base64 + per-image/aggregate
    /// limits + deterministic normalization + content-addressed storage.
    /// Returns durable references in input order; a refused batch starts no
    /// writes.
    pub fn admit_image_attachments(
        &self,
        images_json: String,
    ) -> anyhow::Result<Vec<host_types::AttachmentRef>> {
        #[derive(serde::Deserialize)]
        struct WireImage {
            #[serde(rename = "name")]
            name: Option<String>,
            #[serde(rename = "mediaType")]
            media_type: String,
            #[serde(rename = "data")]
            data_base64: String,
        }
        let images: Vec<WireImage> =
            serde_json::from_str(&images_json).map_err(|e| anyhow::anyhow!("images json: {e}"))?;
        // WI-6/attachment seam: admission goes through the store surface so
        // a replacement implementation carries its own policy.
        let candidates: Vec<starriver_kernel::attachments::AdmissionCandidate> = images
            .into_iter()
            .map(|i| starriver_kernel::attachments::AdmissionCandidate {
                name: i.name,
                media_type: i.media_type,
                data_base64: i.data_base64,
            })
            .collect();
        let admitted = starriver_kernel::attachments::AttachmentStoreApi::admit_images(
            self.attachment_store.as_ref(),
            &candidates,
        )
        .map_err(|e| anyhow::anyhow!("{e}"))?;
        Ok(admitted
            .into_iter()
            .map(|a| host_types::AttachmentRef {
                id: a.id,
                media_type: a.media_type,
                width: a.width,
                height: a.height,
                bytes: a.bytes,
                name: a.name,
                original_width: a.original_width,
                original_height: a.original_height,
            })
            .collect())
    }

    /// Read one stored normalized image back for a model request (scaled
    /// dimensions + base64 payload). BUG-126: the wire path skips the
    /// digest re-verify and full decode of `read_image` — stored objects
    /// were verified at commit time, and thumbnails hit this per render.
    pub fn read_image_attachment(
        &self,
        id: String,
    ) -> anyhow::Result<host_types::StoredAttachment> {
        let stored = self
            .attachment_store
            .read_image_light(&id)
            .ok_or_else(|| anyhow::anyhow!("attachment unavailable: {id}"))?;
        Ok(host_types::StoredAttachment {
            id: stored.reference.attachment_id,
            media_type: stored.reference.media_type,
            width: stored.reference.width,
            height: stored.reference.height,
            base64: base64::engine::general_purpose::STANDARD.encode(&stored.data),
        })
    }
}

/// Scan Markdown image references in a task and resolve them into kernel
/// `Image` content blocks (attachment refs, local image files, or inline data
/// URLs); returns the remaining text with the consumed references removed.
///
/// References that cannot be resolved are left in the text verbatim
/// (WI-152 S3: no OCR fallback — unresolved refs stay as plain markdown).
/// This is the host-side projection of attachment → request-image
/// admission.
pub fn extract_image_blocks(task: &str, store: &AttachmentStore) -> (String, Vec<ContentBlock>) {
    if !task.contains("![") {
        return (task.to_string(), vec![ContentBlock::Text(task.to_string())]);
    }
    // BUG-129: the path capture allows spaces (Finder-drop paths carry
    // them); it still stops at the first ')' and unresolved captures stay
    // verbatim in the text.
    let re = regex::Regex::new(r"!\[([^\]]*)\]\(([^)]+)\)").unwrap();
    let mut consumed: Vec<(usize, usize, ContentBlock)> = Vec::new();
    for cap in re.captures_iter(task) {
        let full = cap.get(0).unwrap();
        let path = cap.get(2).map(|m| m.as_str()).unwrap_or("").trim();
        if let Some(block) = image_block_for(path, store) {
            consumed.push((full.start(), full.end(), block));
        }
    }
    // Stripped text (titles / notices): resolved refs removed, same as ever.
    let mut result = task.to_string();
    for (start, end, _) in consumed.iter().rev() {
        result.replace_range(start..end, "");
    }
    let result = result.trim().to_string();
    if consumed.is_empty() {
        return (result, vec![ContentBlock::Text(task.to_string())]);
    }
    // Position-faithful content: text segments stay BETWEEN the image blocks
    // exactly where the user composed them. The old shape ([Text, Images…])
    // reordered the message — the phone's transcript (block order) showed
    // text above images even when the composer put the image first.
    let mut blocks: Vec<ContentBlock> = Vec::new();
    let mut push_text = |blocks: &mut Vec<ContentBlock>, seg: &str| {
        let seg = seg.trim();
        if !seg.is_empty() {
            blocks.push(ContentBlock::Text(seg.to_string()));
        }
    };
    let mut cursor = 0usize;
    for (start, end, block) in &consumed {
        push_text(&mut blocks, &task[cursor..*start]);
        blocks.push(block.clone());
        cursor = *end;
    }
    push_text(&mut blocks, &task[cursor..]);
    (result, blocks)
}

/// Build the content block for one image reference (WI-1):
/// - `sha256:<hex>` → a *reference* block `{fileId, mediaType, name?, width,
///   height}` — the session log never carries base64. The agent-loop's
///   cold/hot projection resolves hot ones back to data URLs at request time
///   (WI-152 S3: identity + metadata only, no content summaries).
/// - `data:image/...` and local files → inline `{url}` blocks (legacy shape,
///   still cold/hot-projectable).
/// Unresolvable refs yield `None` and stay in the text verbatim.
fn image_block_for(path: &str, store: &AttachmentStore) -> Option<ContentBlock> {
    if let Some(hex) = path.strip_prefix("sha256:") {
        let file_id = format!("sha256:{hex}");
        // BUG-126: metadata comes from the header sniff — no full read, no
        // decode, per execute per reference. (The read path re-derived
        // `name: None` anyway, so nothing is lost.)
        let info = store.image_info(&file_id)?;
        let attachment = vec![
            ("fileId".into(), JsonValue::String(file_id.clone())),
            ("mediaType".into(), JsonValue::String(info.media_type)),
            ("width".into(), JsonValue::UInt(u64::from(info.width))),
            ("height".into(), JsonValue::UInt(u64::from(info.height))),
        ];
        return Some(ContentBlock::Image {
            attachment: JsonValue::Object(attachment),
        });
    }
    let url = resolve_image_url(path, store)?;
    Some(ContentBlock::Image {
        attachment: JsonValue::Object(vec![("url".into(), JsonValue::String(url))]),
    })
}

/// Resolve one image reference to an inline data URL:
/// - `sha256:<hex>` → the attachment store's normalized object
/// - `data:image/...` → passed through unchanged
/// - a local file path → read + encoded by extension (png/jpeg/webp).
///   Local reads are capped at the store's per-image byte limit (an
///   oversized file stays verbatim in the text instead of ballooning the
///   request/log — same envelope as admitted uploads).
fn resolve_image_url(path: &str, store: &AttachmentStore) -> Option<String> {
    if let Some(sha) = path.strip_prefix("sha256:") {
        let stored = store.read_image(&format!("sha256:{sha}")).ok()?;
        return Some(format!(
            "data:{};base64,{}",
            stored.reference.media_type,
            base64::engine::general_purpose::STANDARD.encode(&stored.data)
        ));
    }
    if path.starts_with("data:image/") {
        // Same envelope as admitted uploads: the base64 payload must decode
        // to at most max_image_bytes — an oversized inline URL stays
        // verbatim instead of ballooning the log and every request.
        let cap = store.limits().max_image_bytes as u64;
        let payload = path.split_once(',').map(|(_, p)| p).unwrap_or("");
        let payload_bytes = (payload.len() as u64) / 4 * 3;
        return (payload_bytes <= cap).then(|| path.to_string());
    }
    let cap = store.limits().max_image_bytes;
    let data = std::fs::read(path)
        .ok()
        .filter(|data| !data.is_empty() && data.len() <= cap)?;
    let media_type = match std::path::Path::new(path)
        .extension()
        .and_then(|e| e.to_str())
        .map(str::to_ascii_lowercase)
        .as_deref()
    {
        Some("png") => "image/png",
        Some("jpg") | Some("jpeg") => "image/jpeg",
        Some("webp") => "image/webp",
        _ => return None,
    };
    Some(format!(
        "data:{media_type};base64,{}",
        base64::engine::general_purpose::STANDARD.encode(&data)
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::attachments::SaveImageAttachment;
    use starriver_kernel::llm::ContentBlock;

    fn png_bytes() -> Vec<u8> {
        let mut buf = Vec::new();
        let img = image::RgbImage::from_pixel(32, 24, image::Rgb([1, 2, 3]));
        image::DynamicImage::ImageRgb8(img)
            .write_to(&mut std::io::Cursor::new(&mut buf), image::ImageFormat::Png)
            .unwrap();
        buf
    }

    #[test]
    fn extract_image_blocks_consumes_refs_and_keeps_text() {
        let dir = tempfile::tempdir().unwrap();
        let store = AttachmentStore::new(
            dir.path().join("attachments").join("v1"),
            ImageAttachmentLimits::default(),
            NormalizationPolicy::default(),
        );
        let refs = store
            .save_images(&[SaveImageAttachment {
                name: None,
                media_type: "image/png".into(),
                data: png_bytes(),
            }])
            .unwrap();
        let task = format!("look at ![photo]({}) and describe", refs[0].attachment_id);

        let (text, blocks) = extract_image_blocks(&task, &store);
        assert_eq!(text, "look at  and describe");
        // Position-faithful: the image sits BETWEEN its surrounding text.
        assert_eq!(blocks.len(), 3);
        let ContentBlock::Text(t0) = &blocks[0] else { panic!("leading text") };
        assert_eq!(t0, "look at");
        let ContentBlock::Image { attachment } = &blocks[1] else {
            panic!("expected image block");
        };
        let ContentBlock::Text(t2) = &blocks[2] else { panic!("trailing text") };
        assert_eq!(t2, "and describe");
        // WI-1: stored refs become reference blocks — no base64 in the log.
        // WI-152 S3: identity + metadata only, no summary field.
        let file_id = attachment
            .as_str_at("fileId")
            .expect("attachment carries a file id");
        assert!(file_id.starts_with("sha256:"), "{file_id}");
        assert!(attachment.as_str_at("summary").is_none(), "{attachment:?}");
        assert!(attachment.as_str_at("url").is_none(), "{attachment:?}");
        assert!(
            attachment.as_str_at("mediaType").is_some(),
            "{attachment:?}"
        );
    }

    #[test]
    fn extract_image_blocks_passes_through_data_urls_and_plain_text() {
        let dir = tempfile::tempdir().unwrap();
        let store = AttachmentStore::new(
            dir.path().join("attachments").join("v1"),
            ImageAttachmentLimits::default(),
            NormalizationPolicy::default(),
        );
        let (text, blocks) = extract_image_blocks("![x](data:image/png;base64,AAAA) hi", &store);
        assert_eq!(text, "hi");
        assert_eq!(blocks.len(), 2, "image first, trailing text kept");
        assert!(matches!(blocks[0], ContentBlock::Image { .. }));

        let (text, blocks) = extract_image_blocks("plain text only", &store);
        assert_eq!(text, "plain text only");
        // No images: the content is the text block itself (caller contract).
        assert!(matches!(blocks.as_slice(), [ContentBlock::Text(t)] if t == "plain text only"));
    }

    /// BUG-129: local paths with spaces resolve (Finder-drop style); the
    /// reference stays verbatim only when it cannot resolve.
    #[test]
    fn local_paths_with_spaces_resolve() {
        let dir = tempfile::tempdir().unwrap();
        let store = AttachmentStore::new(
            dir.path().join("attachments").join("v1"),
            ImageAttachmentLimits::default(),
            NormalizationPolicy::default(),
        );
        let spaced = dir.path().join("my screenshot.png");
        std::fs::write(&spaced, vec![0x89, b'P', b'N', b'G', 1, 2, 3, 4]).unwrap();
        let (text, blocks) = extract_image_blocks(
            &format!("see ![shot]({})", spaced.display()),
            &store,
        );
        assert_eq!(blocks.len(), 2, "spaced path resolves: {text}");
        assert!(matches!(blocks[1], ContentBlock::Image { .. }));
        let ContentBlock::Text(lead) = &blocks[0] else { panic!("lead text") };
        assert_eq!(lead, "see");
        assert!(!text.contains("![shot]"), "consumed reference: {text}");
    }

    /// Oversized inline data URLs stay verbatim — same envelope as local
    /// files and admitted uploads.
    #[test]
    fn oversized_data_urls_stay_verbatim() {
        let dir = tempfile::tempdir().unwrap();
        let store = AttachmentStore::new(
            dir.path().join("attachments").join("v1"),
            ImageAttachmentLimits {
                max_image_bytes: 16,
                ..ImageAttachmentLimits::default()
            },
            NormalizationPolicy::default(),
        );
        let huge = format!("data:image/png;base64,{}", "A".repeat(200));
        let task = format!("![big]({huge})");
        let (text, blocks) = extract_image_blocks(&task, &store);
        assert_eq!(text, task, "oversized inline URL stays verbatim");
        assert!(
            matches!(blocks.as_slice(), [ContentBlock::Text(t)] if t == &task),
            "unresolved-only content is the verbatim text block"
        );

        let small = "data:image/png;base64,AAAA".to_string();
        let (text, blocks) =
            extract_image_blocks(&format!("![ok]({small})"), &store);
        assert_eq!(blocks.len(), 1, "in-cap inline URL rides: {text}");
    }

    #[test]
    fn extract_image_blocks_leaves_unresolvable_refs_verbatim() {
        let dir = tempfile::tempdir().unwrap();
        let store = AttachmentStore::new(
            dir.path().join("attachments").join("v1"),
            ImageAttachmentLimits::default(),
            NormalizationPolicy::default(),
        );
        let task = "![x](/nonexistent/photo.bmp) and ![y](sha256:deadbeef)";
        let (text, blocks) = extract_image_blocks(task, &store);
        assert_eq!(text, task, "unresolvable refs stay in the text");
        assert!(
            matches!(blocks.as_slice(), [ContentBlock::Text(t)] if t == task),
            "content stays the verbatim text block"
        );
    }

    /// BUG-123: a local-file markdown reference is capped at the store's
    /// per-image byte limit — an oversized file stays verbatim instead of
    /// ballooning the request and the session log.
    #[test]
    fn local_file_references_respect_the_byte_cap() {
        let dir = tempfile::tempdir().unwrap();
        let store = AttachmentStore::new(
            dir.path().join("attachments").join("v1"),
            ImageAttachmentLimits {
                max_image_bytes: 16,
                ..ImageAttachmentLimits::default()
            },
            NormalizationPolicy::default(),
        );
        let big = dir.path().join("big.png");
        std::fs::write(&big, vec![0u8; 64]).unwrap();
        let small = dir.path().join("small.png");
        std::fs::write(&small, vec![0x89, b'P', b'N', b'G', 1, 2, 3, 4]).unwrap();

        let (text, blocks) = extract_image_blocks(
            &format!("![big]({}) ![small]({})", big.display(), small.display()),
            &store,
        );
        assert!(text.contains("big.png"), "oversized stays verbatim: {text}");
        assert_eq!(blocks.len(), 2, "only the in-cap file rides");
        let ContentBlock::Text(kept) = &blocks[0] else { panic!("verbatim lead") };
        assert!(kept.contains("big.png"), "oversized ref stays in its segment");
    }
}

/// Build the host attachment store under `~/.starriver/attachments/v1`.
pub fn open_attachment_store() -> Arc<AttachmentStore> {
    let home = std::env::var("HOME")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|_| std::path::PathBuf::from("."));
    Arc::new(AttachmentStore::new(
        home.join(".starriver").join("attachments").join("v1"),
        ImageAttachmentLimits::default(),
        NormalizationPolicy::default(),
    ))
}

/// WI-1: kernel `ImageResolver` bridge over the host attachment store — hot
/// projection resolves `sha256:` refs back to data URLs, cold projection
/// reads the cached summary. The store is content-addressed and stateless,
/// so opening the canonical root is coherent with the runtime's instance.
pub struct StoreImageResolver {
    store: starriver_kernel::attachments::AttachmentStore,
}

impl StoreImageResolver {
    /// Wraps the provided seam service (resolved from the kernel context).
    pub fn new(store: starriver_kernel::attachments::AttachmentStore) -> Self {
        Self { store }
    }
}

impl starriver_kernel::agent::ImageResolver for StoreImageResolver {
    fn resolve(&self, file_id: &str) -> Option<String> {
        let stored =
            starriver_kernel::attachments::AttachmentStoreApi::read_image(&self.store, file_id)?;
        Some(format!(
            "data:{};base64,{}",
            stored.media_type, stored.base64
        ))
    }
    fn byte_len(&self, file_id: &str) -> Option<u64> {
        starriver_kernel::attachments::AttachmentStoreApi::byte_len(&self.store, file_id)
    }
}

impl starriver_kernel::attachments::AttachmentStoreApi for AttachmentStore {
    fn admit_images(
        &self,
        images: &[starriver_kernel::attachments::AdmissionCandidate],
    ) -> Result<Vec<starriver_kernel::attachments::AdmittedAttachment>, String> {
        use starriver_kernel::attachments::AdmittedAttachment;
        let encoded: Vec<EncodedImageAttachment> = images
            .iter()
            .map(|c| EncodedImageAttachment {
                name: c.name.clone(),
                media_type: c.media_type.clone(),
                data_base64: c.data_base64.clone(),
            })
            .collect();
        let save = admit_encoded_images(&encoded, self.limits()).map_err(|e| e.to_string())?;
        let refs = self.save_images(&save).map_err(|e| e.to_string())?;
        Ok(refs
            .into_iter()
            .zip(save.iter())
            .map(|(r, save_input)| AdmittedAttachment {
                id: r.attachment_id,
                media_type: r.media_type.clone(),
                width: r.width,
                height: r.height,
                bytes: r.bytes as u32,
                name: crate::attachments::store::display_name(save_input.name.as_deref()),
                original_width: r.original_dimensions.map(|d| d.0),
                original_height: r.original_dimensions.map(|d| d.1),
            })
            .collect())
    }

    fn read_image(&self, file_id: &str) -> Option<starriver_kernel::attachments::StoredImage> {
        let stored = self.read_image(file_id).ok()?;
        Some(starriver_kernel::attachments::StoredImage {
            media_type: stored.reference.media_type.clone(),
            base64: base64::engine::general_purpose::STANDARD.encode(&stored.data),
        })
    }

    fn exists(&self, file_id: &str) -> bool {
        // BUG-120: existence is a metadata question — the old full
        // read + decode probe made every check pay the whole object.
        AttachmentStore::image_bytes(self, file_id).is_some()
    }

    /// WI-159: budget accounting must not read or decode image bytes — the
    /// content-addressed object's `fs::metadata` is the normalized length.
    fn byte_len(&self, file_id: &str) -> Option<u64> {
        AttachmentStore::image_bytes(self, file_id)
    }
}
