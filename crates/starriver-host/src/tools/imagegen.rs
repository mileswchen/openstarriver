//! `generate_image` tool (WI-102): calls an OpenAI-compatible
//! `POST {base_url}/images/generations` endpoint on a provider declared in
//! settings and lands every returned image in the content-addressed
//! attachment store — the same seam user uploads go through, so generated
//! images get `sha256:` references, dedup, and the desktop lazy-read path
//! for free. The tool's text output carries markdown image refs plus the
//! on-disk object path (the TUI renders that path; terminals can't inline
//! rasters).

use std::sync::Arc;
use std::time::Duration;

use base64::Engine;
use cordis_rslite::Value;
use futures::future::BoxFuture;
use starriver_kernel::attachments::AttachmentStoreApi;
use starriver_kernel::tools::{ToolDefinition, ToolError, ToolExecutor, ToolRunContext};

use super::common::{invalid_input, json_args, text_content, tool_failure};
use crate::settings::LlmProviderSettings;

/// Hot read of the settings slice the route resolver needs. Sampled per
/// call (and once at registration for the description hint) so a settings
/// edit applies without re-registration.
pub type ImageGenConfigSource = Arc<dyn Fn() -> ImageGenConfig + Send + Sync>;

pub struct ImageGenConfig {
    pub providers: Vec<LlmProviderSettings>,
    pub image_model: Option<String>,
}

/// The image route a call resolved to: which provider entry drives the
/// HTTP request and which model id is sent.
pub struct ResolvedImageRoute {
    pub provider: LlmProviderSettings,
    pub model: String,
}

/// Priority: explicit tool params > `llm.image_model` > first cloud
/// provider's default model > first provider's default model. A bare
/// `model` param overrides the model on whichever provider the default
/// chain picked.
pub fn resolve_image_route(
    providers: &[LlmProviderSettings],
    image_model: Option<&str>,
    param_provider: Option<&str>,
    param_model: Option<&str>,
) -> Result<ResolvedImageRoute, String> {
    let pick = |entry: &LlmProviderSettings, model: Option<&str>| {
        let model = match model {
            Some(m) => entry
                .resolve_variant(m)
                .map(|v| v.id.clone())
                .unwrap_or_else(|| m.to_string()),
            None => entry.default_model.clone(),
        };
        ResolvedImageRoute {
            provider: entry.clone(),
            model,
        }
    };
    if let Some(id) = param_provider {
        let entry = providers
            .iter()
            .find(|p| p.id == id)
            .ok_or_else(|| format!("provider `{id}` not found in llm settings"))?;
        return Ok(pick(entry, param_model));
    }
    if let Some(key) = image_model {
        let (pid, variant) = match key.split_once('/') {
            Some((pid, variant)) => (pid, Some(variant)),
            None => (key, None),
        };
        if let Some(entry) = providers.iter().find(|p| p.id == pid) {
            return Ok(pick(entry, variant.or(param_model)));
        }
    }
    let default_entry = providers
        .iter()
        .find(|p| p.model_type == "cloud")
        .or_else(|| providers.first())
        .ok_or_else(|| {
            "no llm provider configured — add one before generating images".to_string()
        })?;
    Ok(pick(default_entry, param_model))
}

/// Resolve a provider's credential reference (`!file:` / `${ENV}` /
/// literal) through the secrets store — the same rules the LLM routes use.
fn resolve_credential(field: &str) -> Result<String, String> {
    if field.is_empty() {
        return Err("provider has no api key configured".into());
    }
    starriver_secrets::credentials::CredentialsStore::new()
        .resolve_field(field)
        .map_err(|e| e.to_string())
}

/// One image payload off the wire: inline base64 or a fetch-later URL.
/// Sniff the media type + extension from image bytes (BUG-90): the b64
/// payload of an OpenAI-compatible images endpoint is NOT guaranteed PNG —
/// SDXL/Flux-style gateways return JPEG/WebP, and the attachment normalizer
/// hard-validates declared-type == actual bytes, so a hardcoded png fails
/// admission after the provider already billed the request. Unknown bytes
/// fall back to `fallback` (Content-Type, then png).
fn sniff_image(bytes: &[u8], fallback: &str) -> String {
    if let Ok(fmt) = image::guess_format(bytes) {
        return match fmt {
            image::ImageFormat::Png => "image/png",
            image::ImageFormat::Jpeg => "image/jpeg",
            image::ImageFormat::WebP => "image/webp",
            image::ImageFormat::Gif => "image/gif",
            image::ImageFormat::Bmp => "image/bmp",
            _ => return fallback.to_string(),
        }
        .to_string();
    }
    fallback.to_string()
}

enum ImagePayload {
    Base64(String),
    Url(String),
}

fn parse_images_response(body: &str) -> Result<Vec<ImagePayload>, String> {
    #[derive(serde::Deserialize)]
    struct Item {
        b64_json: Option<String>,
        url: Option<String>,
    }
    #[derive(serde::Deserialize)]
    struct Body {
        #[serde(default)]
        data: Vec<Item>,
    }
    let parsed: Body = serde_json::from_str(body)
        .map_err(|e| format!("image endpoint returned malformed JSON: {e}"))?;
    let payloads = parsed
        .data
        .into_iter()
        .filter_map(|i| {
            match (
                i.b64_json.filter(|s| !s.is_empty()),
                i.url.filter(|s| !s.is_empty()),
            ) {
                (Some(b64), _) => Some(ImagePayload::Base64(b64)),
                (None, Some(url)) => Some(ImagePayload::Url(url)),
                (None, None) => None,
            }
        })
        .collect::<Vec<_>>();
    if payloads.is_empty() {
        return Err("image endpoint returned no images (empty data array)".into());
    }
    Ok(payloads)
}

/// Absolute object path for one `sha256:<hex>` reference — mirrors the
/// store's `<root>/objects/<sha[:2]>/<sha>` layout so the TUI can tell the
/// user where the file lives.
fn object_path(store: &crate::attachments::AttachmentStore, id: &str) -> String {
    match id.strip_prefix("sha256:").filter(|sha| sha.len() >= 2) {
        Some(sha) => store
            .root()
            .join("objects")
            .join(&sha[0..2])
            .join(sha)
            .display()
            .to_string(),
        None => id.to_string(),
    }
}

/// POST the generation request and normalize the response into admission
/// candidates (name guessed from position; media type defaults to png for
/// inline b64, taken from the download's Content-Type otherwise).
async fn fetch_images(
    http: &reqwest::Client,
    route: &ResolvedImageRoute,
    prompt: &str,
    size: Option<&str>,
    count: u32,
) -> Result<Vec<starriver_kernel::attachments::AdmissionCandidate>, String> {
    let key = resolve_credential(&route.provider.api_key)?;
    let url = format!(
        "{}/images/generations",
        route.provider.base_url.trim_end_matches('/')
    );
    let mut body = serde_json::json!({
        "model": route.model,
        "prompt": prompt,
        "n": count,
    });
    if let Some(size) = size {
        body["size"] = serde_json::Value::String(size.to_string());
    }
    let response = http
        .post(&url)
        .bearer_auth(&key)
        .json(&body)
        .send()
        .await
        .map_err(|e| format!("image request to `{url}` failed: {e}"))?;
    let status = response.status();
    let text = response
        .text()
        .await
        .map_err(|e| format!("reading image response failed: {e}"))?;
    if !status.is_success() {
        let excerpt: String = text.chars().take(400).collect();
        // 404/405 = the endpoint does not exist on this provider: the resolved
        // route is a chat-only base_url (no /images/generations). Say WHICH
        // route was picked and how to point the tool at an image-capable one —
        // otherwise the failure reads as a mystery HTTP code.
        if status == reqwest::StatusCode::NOT_FOUND
            || status == reqwest::StatusCode::METHOD_NOT_ALLOWED
        {
            return Err(format!(
                "image endpoint `{url}` returned {status} — provider `{}` (model `{}`) has no /images/generations API. \
Set `llm.image_model` to an image-capable route (`provider/model`) in settings, or pass provider/model params.",
                route.provider.id, route.model
            ));
        }
        return Err(format!("image endpoint returned {status}: {excerpt}"));
    }
    let payloads = parse_images_response(&text)?;
    let mut candidates = Vec::with_capacity(payloads.len());
    for (i, payload) in payloads.into_iter().enumerate() {
        let (media_type, data_base64) = match payload {
            ImagePayload::Base64(b64) => {
                // BUG-90: sniff actual bytes; the payload may be JPEG/WebP.
                let bytes = base64::engine::general_purpose::STANDARD
                    .decode(&b64)
                    .unwrap_or_default();
                (sniff_image(&bytes, "image/png"), b64)
            }
            ImagePayload::Url(src) => {
                let get =
                    http.get(&src).send().await.map_err(|e| {
                        format!("downloading generated image from `{src}` failed: {e}")
                    })?;
                if !get.status().is_success() {
                    return Err(format!(
                        "downloading generated image from `{src}` returned {}",
                        get.status()
                    ));
                }
                let media_type = get
                    .headers()
                    .get(reqwest::header::CONTENT_TYPE)
                    .and_then(|v| v.to_str().ok())
                    .map(|s| s.split(';').next().unwrap_or(s).trim().to_string())
                    .filter(|s| s.starts_with("image/"))
                    .unwrap_or_else(|| "image/png".to_string());
                let bytes = get
                    .bytes()
                    .await
                    .map_err(|e| format!("reading image body from `{src}` failed: {e}"))?;
                // BUG-90: bytes win over the header — gateways mislabel.
                (sniff_image(&bytes, &media_type), {
                    base64::engine::general_purpose::STANDARD.encode(&bytes)
                })
            }
        };
        let ext = match media_type.as_str() {
            "image/jpeg" => "jpg",
            "image/webp" => "webp",
            "image/gif" => "gif",
            "image/bmp" => "bmp",
            _ => "png",
        };
        candidates.push(starriver_kernel::attachments::AdmissionCandidate {
            name: Some(format!("generated-{i}.{ext}")),
            media_type,
            data_base64,
        });
    }
    Ok(candidates)
}

struct GenerateImage {
    config: ImageGenConfigSource,
    store: Arc<crate::attachments::AttachmentStore>,
    http: reqwest::Client,
}

impl ToolExecutor for GenerateImage {
    fn execute(
        &self,
        arguments: Value,
        _ctx: ToolRunContext,
    ) -> BoxFuture<'static, Result<Value, ToolError>> {
        let config = Arc::clone(&self.config);
        let store = Arc::clone(&self.store);
        let http = self.http.clone();
        Box::pin(async move {
            let json = json_args(&arguments)?;
            let prompt = json
                .get("prompt")
                .and_then(|p| p.as_str())
                .map(str::trim)
                .filter(|p| !p.is_empty())
                .ok_or_else(|| invalid_input("generate_image", "missing prompt".into()))?
                .to_string();
            let param_provider = json
                .get("provider")
                .and_then(|v| v.as_str())
                .map(String::from);
            let param_model = json.get("model").and_then(|v| v.as_str()).map(String::from);
            let size = json.get("size").and_then(|v| v.as_str()).map(String::from);
            let count = json
                .get("count")
                .and_then(|v| v.as_u64())
                .unwrap_or(1)
                .clamp(1, 4) as u32;

            let cfg = config();
            let route = resolve_image_route(
                &cfg.providers,
                cfg.image_model.as_deref(),
                param_provider.as_deref(),
                param_model.as_deref(),
            )
            .map_err(|e| tool_failure("generate_image", "NO_IMAGE_ROUTE", e))?;

            let candidates = fetch_images(&http, &route, &prompt, size.as_deref(), count)
                .await
                .map_err(|e| tool_failure("generate_image", "IMAGE_PROVIDER_ERROR", e))?;
            let admitted = AttachmentStoreApi::admit_images(store.as_ref(), &candidates)
                .map_err(|e| tool_failure("generate_image", "ATTACHMENT_ADMIT_FAILED", e))?;

            // `output` is the model-visible text: the markdown refs let the
            // assistant echo images into its reply (the desktop renders
            // them), the saved paths serve terminal surfaces.
            let mut output = format!(
                "Generated {} image(s) with {}/{}:",
                admitted.len(),
                route.provider.id,
                route.model
            );
            let mut images = Vec::with_capacity(admitted.len());
            for (i, a) in admitted.iter().enumerate() {
                let path = object_path(&store, &a.id);
                output.push_str(&format!("\n![generated-{}]({})", i + 1, a.id));
                output.push_str(&format!("\n(saved: {path})"));
                images.push(serde_json::json!({
                    "id": a.id,
                    "mediaType": a.media_type,
                    "width": a.width,
                    "height": a.height,
                    "bytes": a.bytes,
                    "path": path,
                }));
            }
            Ok(Value::new(serde_json::json!({
                "output": output,
                "images": images,
            })))
        })
    }
}

pub fn generate_image_definition(
    config: ImageGenConfigSource,
    store: Arc<crate::attachments::AttachmentStore>,
) -> ToolDefinition {
    let hint = config()
        .image_model
        .as_ref()
        .map(|k| format!(" Default route: `{k}` (llm.image_model)."))
        .unwrap_or_else(|| {
            " No llm.image_model setting — defaults to the first cloud provider's default model."
                .into()
        });
    ToolDefinition {
        name: "generate_image".into(),
        description: format!(
            "Generate image(s) from a text prompt via the configured image model and return durable \
             attachment references. Every generated image lands in the attachment store; the output \
             text embeds markdown image refs (`![generated-N](sha256:…)`) the UI renders inline, plus \
             the on-disk path for terminal use.{hint} Pass `provider`/`model` explicitly to target a \
             different route; `size` like \"1024x1024\" and `count` 1-4 when needed."
        ),
        parameters: Value::new(serde_json::json!({
            "type": "object",
            "properties": {
                "prompt": { "type": "string", "description": "what to draw" },
                "provider": { "type": "string", "description": "llm settings provider id to route the generation through (optional)" },
                "model": { "type": "string", "description": "image model variant id on that provider (optional)" },
                "size": { "type": "string", "description": "image size, e.g. 1024x1024 (optional)" },
                "count": { "type": "integer", "description": "number of images, 1-4 (optional, default 1)" }
            },
            "required": ["prompt"]
        })),
        output: Value::new(serde_json::json!({
            "type": "object",
            "properties": {
                "output": { "type": "string" },
                "images": { "type": "array" }
            }
        })),
        render: Arc::new(text_content),
        executor: Arc::new(GenerateImage {
            config,
            store,
            http: reqwest::Client::builder()
                .timeout(Duration::from_secs(110))
                .build()
                .unwrap_or_default(),
        }),
        is_concurrency_safe: None,
        timeout: Some(Duration::from_secs(120)),
        // No workspace writes and no secrets beyond the provider's own
        // credential — the attachment store is the only side effect.
        requires_approval: false,
        deny_in_readonly: false,
        finalize_content: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::attachments::{ImageAttachmentLimits, NormalizationPolicy};

    fn provider(id: &str, model_type: &str, default: &str) -> LlmProviderSettings {
        LlmProviderSettings {
            id: id.into(),
            name: id.into(),
            model_type: model_type.into(),
            provider_type: "openai-compat".into(),
            base_url: "https://example.invalid/v1".into(),
            api_key: "sk-test".into(),
            model_path: None,
            models: vec![],
            default_model: default.into(),
            concurrency: None,
            max_tokens: None,
            context_window: None,
            temperature: None,
            api_format: "openai-chat".into(),
            reasoning_effort: None,
            model_label: None,
            supports_vision: None,
        }
    }

    #[test]
    fn route_resolution_priority() {
        let providers = vec![
            provider("local-a", "local", "llama3"),
            provider("cloud-a", "cloud", "gpt-5"),
            provider("cloud-b", "cloud", "gpt-image-1"),
        ];

        // Explicit provider + model wins.
        let r =
            resolve_image_route(&providers, None, Some("cloud-b"), Some("gpt-image-1")).unwrap();
        assert_eq!(r.provider.id, "cloud-b");
        assert_eq!(r.model, "gpt-image-1");

        // Settings key beats the default chain.
        let r = resolve_image_route(&providers, Some("cloud-b/gpt-image-1"), None, None).unwrap();
        assert_eq!(r.provider.id, "cloud-b");
        assert_eq!(r.model, "gpt-image-1");

        // Bare model param overrides the default chain's model, not provider.
        let r = resolve_image_route(&providers, None, None, Some("gpt-image-1-mini")).unwrap();
        assert_eq!(r.provider.id, "cloud-a");
        assert_eq!(r.model, "gpt-image-1-mini");

        // Default: first cloud provider's default model (locals skipped).
        let r = resolve_image_route(&providers, None, None, None).unwrap();
        assert_eq!(r.provider.id, "cloud-a");
        assert_eq!(r.model, "gpt-5");

        // No cloud provider → first provider's default.
        let locals = vec![provider("local-a", "local", "llama3")];
        let r = resolve_image_route(&locals, None, None, None).unwrap();
        assert_eq!(r.provider.id, "local-a");

        // Unknown explicit provider errors.
        assert!(resolve_image_route(&providers, None, Some("nope"), None).is_err());
        // No providers at all errors.
        assert!(resolve_image_route(&[], None, None, None).is_err());
        // An image_model key naming a missing provider falls through to the
        // default chain instead of erroring.
        let r = resolve_image_route(&providers, Some("ghost/img"), None, None).unwrap();
        assert_eq!(r.provider.id, "cloud-a");
    }

    #[test]
    fn parses_b64_and_url_payloads() {
        let b64 = parse_images_response(r#"{"data":[{"b64_json":"AAAA"},{"url":"https://x/y.png"},{"b64_json":"","url":"https://x/z.png"}]}"#)
            .unwrap();
        assert_eq!(b64.len(), 3);
        assert!(matches!(&b64[0], ImagePayload::Base64(s) if s == "AAAA"));
        assert!(matches!(&b64[1], ImagePayload::Url(s) if s == "https://x/y.png"));
        // Empty b64 falls through to the url field.
        assert!(matches!(&b64[2], ImagePayload::Url(s) if s == "https://x/z.png"));

        assert!(parse_images_response(r#"{"data":[]}"#).is_err());
        assert!(parse_images_response("not json").is_err());
    }

    #[test]
    fn object_path_follows_store_layout() {
        let dir = tempfile::tempdir().unwrap();
        let store = crate::attachments::AttachmentStore::new(
            dir.path().to_path_buf(),
            ImageAttachmentLimits::default(),
            NormalizationPolicy::default(),
        );
        let id = "sha256:0123456789abcdef";
        let path = object_path(&store, id);
        let sep = std::path::MAIN_SEPARATOR;
        let expected_tail = format!("objects{sep}01{sep}0123456789abcdef");
        assert!(path.ends_with(&expected_tail), "unexpected layout: {path}");
        assert!(path.starts_with(dir.path().display().to_string().as_str()));
        // Non-sha refs pass through untouched.
        assert_eq!(object_path(&store, "weird"), "weird");
    }

    // A 1x1 transparent PNG — decodable by the admission normalizer.
    const TINY_PNG_B64: &str =
        "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mNkYAAAAAYAAjCB0C8AAAAASUVORK5CYII=";

    fn ctx() -> ToolRunContext {
        let exec = starriver_kernel::tools::ToolExecution {
            call_id: starriver_kernel::llm::CallId("c1".into()),
            root_call_id: starriver_kernel::llm::CallId("c1".into()),
            name: "generate_image".into(),
            arguments: Value::new(()),
            agent_id: Some("session-1".into()),
            cwd: None,
            sandbox_mode: None,
            parent: None,
            token: starriver_kernel::tools::mint_tool_execution_token(),
            cancellation: None,
        };
        ToolRunContext::new(exec)
    }

    #[tokio::test]
    async fn executor_generates_admits_and_reports_paths() {
        use crate::tools::web_common::test_support::{http_ok, spawn_http_server_capture};

        let body = serde_json::json!({ "data": [{ "b64_json": TINY_PNG_B64 }] }).to_string();
        let (base, requests) = spawn_http_server_capture(vec![http_ok(&body)]).await;

        let mut entry = provider("img-cloud", "cloud", "gpt-image-1");
        entry.base_url = base;
        let dir = tempfile::tempdir().unwrap();
        let store = Arc::new(crate::attachments::AttachmentStore::new(
            dir.path().to_path_buf(),
            ImageAttachmentLimits::default(),
            NormalizationPolicy::default(),
        ));
        let config: ImageGenConfigSource = Arc::new(move || ImageGenConfig {
            providers: vec![entry.clone()],
            image_model: None,
        });

        let def = generate_image_definition(config, Arc::clone(&store));
        let result = def
            .executor
            .execute(
                Value::new(serde_json::json!({ "prompt": "a red square" })),
                ctx(),
            )
            .await
            .unwrap();
        let json = result.downcast::<serde_json::Value>().unwrap();

        // The image landed in the store under a sha256 ref, and the output
        // text carries the markdown ref + on-disk path for both surfaces.
        let id = json["images"][0]["id"].as_str().unwrap();
        assert!(id.starts_with("sha256:"), "{id}");
        let output = json["output"].as_str().unwrap();
        assert!(output.contains(&format!("![generated-1]({id})")));
        let path = json["images"][0]["path"].as_str().unwrap();
        assert!(
            std::path::Path::new(path).is_file(),
            "object missing: {path}"
        );
        assert!(output.contains("Generated 1 image(s) with img-cloud/gpt-image-1"));

        // The request hit the images endpoint with the bearer key and body.
        let head = requests.lock().unwrap()[0].clone();
        assert!(head.starts_with("POST /images/generations"), "{head}");
        assert!(head.contains("authorization: Bearer sk-test"), "{head}");
    }

    #[tokio::test]
    async fn executor_reports_provider_errors_readably() {
        use crate::tools::web_common::test_support::spawn_http_server;

        let base = spawn_http_server(vec![
            "HTTP/1.1 404 Not Found\r\nContent-Length: 25\r\nConnection: close\r\n\r\n{\"error\":{\"code\":\"nope\"}}"
                .into(),
        ])
        .await;
        let mut entry = provider("img-cloud", "cloud", "gpt-image-1");
        entry.base_url = base;
        let dir = tempfile::tempdir().unwrap();
        let store = Arc::new(crate::attachments::AttachmentStore::new(
            dir.path().to_path_buf(),
            ImageAttachmentLimits::default(),
            NormalizationPolicy::default(),
        ));
        let config: ImageGenConfigSource = Arc::new(move || ImageGenConfig {
            providers: vec![entry.clone()],
            image_model: None,
        });

        let def = generate_image_definition(config, store);
        let err = def
            .executor
            .execute(Value::new(serde_json::json!({ "prompt": "x" })), ctx())
            .await
            .unwrap_err();
        let rendered = format!("{err:?}");
        assert!(rendered.contains("IMAGE_PROVIDER_ERROR"), "{rendered}");
        assert!(rendered.contains("404"), "{rendered}");
    }

    #[test]
    fn sniff_image_detects_jpeg_and_png_bytes() {
        // BUG-90: a JPEG payload must not be admitted as image/png — the
        // normalizer hard-validates declared type == actual bytes.
        let img = image::RgbImage::from_pixel(1, 1, image::Rgb([200u8, 0, 0]));
        let mut jpeg = Vec::new();
        image::DynamicImage::ImageRgb8(img)
            .write_to(
                &mut std::io::Cursor::new(&mut jpeg),
                image::ImageFormat::Jpeg,
            )
            .expect("encode jpeg");
        assert_eq!(sniff_image(&jpeg, "image/png"), "image/jpeg");

        let png = base64::engine::general_purpose::STANDARD
            .decode(TINY_PNG_B64)
            .expect("decode png");
        assert_eq!(sniff_image(&png, "image/png"), "image/png");

        // Unknown bytes keep the caller's fallback.
        assert_eq!(sniff_image(b"not an image", "image/webp"), "image/webp");
    }
}
