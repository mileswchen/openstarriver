//! WI-1 `reload_image`: pull a historical image back into the model's
//! context. The executor itself only validates the reference and returns a
//! hint — the actual re-attachment happens in the agent-loop's cold/hot
//! projection, which detects the call by name (turn-scoped hot set) and
//! resolves the file id on the very next request.

use std::sync::Arc;

use cordis_rslite::Value;
use starriver_kernel::tools::{ToolDefinition, ToolError, ToolExecutor, ToolRunContext};

use super::common::{invalid_input, json_args, text_content, tool_failure};
use crate::attachments::AttachmentStore;

struct ReloadImage {
    store: Arc<AttachmentStore>,
}

impl ToolExecutor for ReloadImage {
    fn execute(
        &self,
        arguments: Value,
        _ctx: ToolRunContext,
    ) -> cordis_rslite::BoxFuture<Result<Value, ToolError>> {
        // Cloned up front: the boxed future is 'static and must not borrow
        // `&self`.
        let store = Arc::clone(&self.store);
        Box::pin(async move {
            let json = json_args(&arguments)?;
            let file_id = json
                .get("file_id")
                .and_then(|v| v.as_str())
                .map(str::trim)
                .filter(|v| !v.is_empty())
                .ok_or_else(|| invalid_input("reload_image", "missing file_id".into()))?;
            if !file_id.starts_with("sha256:") {
                return Err(tool_failure(
                    "reload_image",
                    "INVALID_FILE_ID",
                    format!("file_id must be a `sha256:<hex>` reference, got: {file_id}"),
                ));
            }
            // Verify the asset exists so a pruned attachment fails here with
            // a clear reason instead of a silently-empty reload next step.
            // BUG-120: the probe is the cheap fs-metadata + magic-sniff
            // `image_info` — no full read, no decode.
            let fid = file_id.to_string();
            let verified = tokio::task::spawn_blocking(move || store.image_info(&fid))
                .await
                .map_err(|e| tool_failure("reload_image", "JOIN_FAILED", format!("{e}")))?
                .map(|info| (info.media_type, info.bytes as usize))
                .ok_or_else(|| {
                    tool_failure(
                        "reload_image",
                        "NOT_FOUND",
                        "attachment unavailable: object missing or unrecognized".into(),
                    )
                })?;
            let output = format!(
                "Image {file_id} ({}; {} bytes) re-attached: it rides the next model request \
                 and stays visible for the rest of this turn.",
                verified.0, verified.1
            );
            Ok(Value::new(serde_json::json!({ "output": output })))
        })
    }
}

/// WI-1 tool definition. Read-only family: it never mutates anything (the
/// store read is a existence check), so no approval and no read-only refusal.
pub fn reload_image_definition(store: Arc<AttachmentStore>) -> ToolDefinition {
    ToolDefinition {
        name: "reload_image".into(),
        description: "Re-attach a historical image so you can see it again. Parameters: \
                      file_id (the `sha256:<hex>` id from a 历史图片 reference). The image \
                      appears in your next context and stays visible for the rest of the turn; \
                      fails with NOT_FOUND when the attachment no longer exists. Each file_id \
                      is honored once per turn — repeating a call for an already re-attached \
                      id is a no-op and returns a stop hint, so don't retry it."
            .into(),
        parameters: Value::new(serde_json::json!({
            "type": "object",
            "properties": {
                "file_id": { "type": "string", "description": "sha256:<hex> attachment reference" }
            },
            "required": ["file_id"]
        })),
        output: Value::new(serde_json::json!({
            "type": "object",
            "properties": { "output": { "type": "string" } }
        })),
        render: Arc::new(text_content),
        executor: Arc::new(ReloadImage { store }),
        is_concurrency_safe: None,
        timeout: None,
        finalize_content: None,
        requires_approval: false,
        deny_in_readonly: false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::attachments::{ImageAttachmentLimits, NormalizationPolicy, SaveImageAttachment};
    use base64::Engine as _;
    use starriver_kernel::tools::{ToolExecution, ToolRuntime};

    fn store_with_asset() -> (tempfile::TempDir, Arc<AttachmentStore>, String) {
        let dir = tempfile::tempdir().unwrap();
        let store = Arc::new(AttachmentStore::new(
            dir.path().join("attachments").join("v1"),
            ImageAttachmentLimits::default(),
            NormalizationPolicy::default(),
        ));
        let mut buf = Vec::new();
        let img = image::RgbImage::from_pixel(8, 8, image::Rgb([1, 2, 3]));
        image::DynamicImage::ImageRgb8(img)
            .write_to(&mut std::io::Cursor::new(&mut buf), image::ImageFormat::Png)
            .unwrap();
        let refs = store
            .save_images(&[SaveImageAttachment {
                name: None,
                media_type: "image/png".into(),
                data: buf,
            }])
            .unwrap();
        (dir, store, refs[0].attachment_id.clone())
    }

    async fn run(def: &ToolDefinition, args: serde_json::Value) -> Result<Value, ToolError> {
        let execution = ToolExecution {
            call_id: starriver_kernel::llm::CallId("c1".into()),
            root_call_id: starriver_kernel::llm::CallId("c1".into()),
            name: "reload_image".into(),
            arguments: Value::new(args.clone()),
            agent_id: None,
            cwd: None,
            sandbox_mode: None,
            parent: None,
            token: starriver_kernel::tools::mint_tool_execution_token(),
            cancellation: None,
        };
        def.executor
            .execute(Value::new(args), ToolRunContext::new(execution))
            .await
    }

    #[test]
    fn definition_shape_matches_tool_family() {
        let (_dir, store, _id) = store_with_asset();
        let def = reload_image_definition(store);
        assert_eq!(def.name, "reload_image");
        assert!(!def.requires_approval, "read-only family: no approval");
        assert!(!def.deny_in_readonly, "read-only family: allowed in RO");
        let params = def.parameters.downcast::<serde_json::Value>().unwrap();
        assert_eq!(params["required"], serde_json::json!(["file_id"]));
    }

    #[tokio::test]
    async fn known_file_id_confirms_the_reload() {
        let (_dir, store, id) = store_with_asset();
        let def = reload_image_definition(store);
        let result = run(&def, serde_json::json!({ "file_id": id }))
            .await
            .unwrap();
        let output = result.downcast::<serde_json::Value>().unwrap();
        assert!(output["output"].as_str().unwrap().contains("re-attached"));
    }

    #[tokio::test]
    async fn missing_attachment_fails_with_not_found() {
        let (_dir, store, _id) = store_with_asset();
        let def = reload_image_definition(store);
        let result = run(&def, serde_json::json!({ "file_id": "sha256:deadbeef" })).await;
        assert!(
            matches!(result, Err(ToolError::Execution { ref code, .. }) if code == "NOT_FOUND")
        );
    }

    #[tokio::test]
    async fn non_sha_reference_is_rejected() {
        let (_dir, store, _id) = store_with_asset();
        let def = reload_image_definition(store);
        let result = run(&def, serde_json::json!({ "file_id": "/tmp/pic.png" })).await;
        assert!(
            matches!(result, Err(ToolError::Execution { ref code, .. }) if code == "INVALID_FILE_ID")
        );
    }

    #[test]
    fn registry_contains_reload_schema() {
        let (_dir, store, _id) = store_with_asset();
        let runtime = starriver_tools::InMemoryToolRuntime::new(
            cordis_rslite::EventBus::new(),
            cordis_rslite::Context::new(),
        );
        let _ = runtime.register(reload_image_definition(store)).unwrap();
        let schema = runtime
            .schemas()
            .into_iter()
            .find(|schema| schema.name == "reload_image")
            .expect("reload_image must appear in the registry schemas");
        assert!(schema.parameters.get("required").is_some());
    }

    /// WI-106: a reference produced by the generate_image admission path
    /// (AttachmentStoreApi::admit_images — the exact call the imagegen tool
    /// makes) reloads cleanly: same store layout, same `sha256:<hex>` id,
    /// media type matches the bytes.
    #[tokio::test]
    async fn imagegen_admitted_reference_reloads() {
        use starriver_kernel::attachments::{AdmissionCandidate, AttachmentStoreApi};

        let dir = tempfile::tempdir().unwrap();
        let store = Arc::new(AttachmentStore::new(
            dir.path().join("attachments").join("v1"),
            ImageAttachmentLimits::default(),
            NormalizationPolicy::default(),
        ));
        // A real 1x1 PNG via the image crate (admit normalizes+decodes).
        let mut buf = Vec::new();
        let img = image::RgbaImage::from_pixel(1, 1, image::Rgba([10, 20, 30, 255]));
        image::DynamicImage::ImageRgba8(img)
            .write_to(&mut std::io::Cursor::new(&mut buf), image::ImageFormat::Png)
            .unwrap();
        let candidate = AdmissionCandidate {
            name: Some("generated-1.png".into()),
            media_type: "image/png".into(),
            data_base64: base64::engine::general_purpose::STANDARD.encode(&buf),
        };
        let admitted = AttachmentStoreApi::admit_images(store.as_ref(), &[candidate])
            .expect("admit (imagegen path)");
        assert_eq!(admitted.len(), 1);
        let reference = admitted[0].id.clone();
        assert!(
            reference.starts_with("sha256:"),
            "imagegen ref shape: {reference}"
        );

        let def = reload_image_definition(Arc::clone(&store));
        let result = run(&def, serde_json::json!({ "file_id": reference }))
            .await
            .expect("generated-image reference reloads");
        let output = result.downcast::<serde_json::Value>().unwrap();
        let text = output["output"].as_str().unwrap();
        assert!(text.contains("re-attached"), "{text}");
        // The admission path normalizes to the store's canonical encoding
        // (webp) — the reload reports the normalized type; the exact codec
        // is transparent to the model and the renderer alike.
        assert!(text.contains("image/"), "media type carried: {text}");
    }

    /// WI-106: a syntactically valid but unknown sha reference fails with
    /// NOT_FOUND (pruned attachment), a non-sha reference with
    /// INVALID_FILE_ID.
    #[tokio::test]
    async fn bogus_references_fail_with_clear_codes() {
        let (_dir, store, _id) = store_with_asset();
        let def = reload_image_definition(store);
        let unknown = format!("sha256:{}", "0".repeat(64));
        let err = run(&def, serde_json::json!({ "file_id": unknown }))
            .await
            .unwrap_err();
        assert!(format!("{err:?}").contains("NOT_FOUND"), "{err:?}");

        let err = run(&def, serde_json::json!({ "file_id": "file.txt" }))
            .await
            .unwrap_err();
        assert!(format!("{err:?}").contains("INVALID_FILE_ID"), "{err:?}");
    }
}
