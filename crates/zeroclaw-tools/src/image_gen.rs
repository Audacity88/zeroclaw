use crate::helpers::{domain_guard, response_body};
use anyhow::Context;
use async_trait::async_trait;
use reqwest::header::LOCATION;
use serde_json::json;
use std::net::{IpAddr, SocketAddr};
use std::path::PathBuf;
use std::sync::Arc;
use zeroclaw_api::tool::{Tool, ToolOutput, ToolResult, with_ephemeral_workspace_warning};
use zeroclaw_config::policy::SecurityPolicy;
use zeroclaw_config::policy::ToolOperation;

const FAL_RESPONSE_LIMIT_BYTES: usize = 1024 * 1024;
const FAL_ERROR_LIMIT_BYTES: usize = 16 * 1024;
const GENERATED_IMAGE_LIMIT_BYTES: usize = 20 * 1024 * 1024;
const MAX_IMAGE_REDIRECTS: usize = 10;

struct ValidatedImageTarget {
    url: reqwest::Url,
    host: String,
    resolved_addrs: Vec<SocketAddr>,
}

fn parse_public_https_url(raw_url: &str) -> anyhow::Result<(reqwest::Url, String, u16)> {
    let raw_url = raw_url.trim();
    if raw_url.is_empty() || raw_url.chars().any(char::is_whitespace) {
        anyhow::bail!("Generated image URL must be a non-empty URL without whitespace");
    }

    let url = reqwest::Url::parse(raw_url).context("Invalid generated image URL")?;
    if url.scheme() != "https" {
        anyhow::bail!("Generated image URL must use HTTPS");
    }
    if !url.username().is_empty() || url.password().is_some() {
        anyhow::bail!("Generated image URL userinfo is not allowed");
    }

    let request_host = url
        .host_str()
        .ok_or_else(|| anyhow::Error::msg("Generated image URL must include a host"))?;
    if request_host.ends_with('.') {
        anyhow::bail!("Generated image URL host must not end with a dot");
    }
    let host = request_host.to_ascii_lowercase();
    if domain_guard::is_private_or_local_host(&host) {
        anyhow::bail!("Generated image URL targets a local or non-global host");
    }
    if host
        .parse::<IpAddr>()
        .is_ok_and(domain_guard::is_cloud_metadata_ip)
    {
        anyhow::bail!("Generated image URL targets a cloud metadata host");
    }

    let port = url
        .port_or_known_default()
        .ok_or_else(|| anyhow::Error::msg("Generated image URL must include a valid port"))?;
    Ok((url, host, port))
}

async fn validate_image_target(raw_url: &str) -> anyhow::Result<ValidatedImageTarget> {
    let (url, host, port) = parse_public_https_url(raw_url)?;
    let resolved_addrs = tokio::net::lookup_host((host.as_str(), port))
        .await
        .with_context(|| format!("Failed to resolve generated image host '{host}'"))?
        .collect::<Vec<_>>();
    let ips = resolved_addrs
        .iter()
        .map(|addr| addr.ip())
        .collect::<Vec<_>>();
    domain_guard::validate_resolved_ips_are_public(&host, &ips)?;

    Ok(ValidatedImageTarget {
        url,
        host,
        resolved_addrs,
    })
}

fn resolve_redirect_url(current: &reqwest::Url, location: &str) -> anyhow::Result<reqwest::Url> {
    current
        .join(location)
        .context("Invalid generated image redirect target")
}

fn resolve_image_filename(filename_arg: Option<&str>, nanos: u128) -> String {
    filename_arg
        .filter(|s| !s.trim().is_empty())
        .map(|s| {
            PathBuf::from(s).file_name().map_or_else(
                || "generated_image".to_string(),
                |n| n.to_string_lossy().to_string(),
            )
        })
        .unwrap_or_else(|| format!("generated_image_{nanos}"))
}

fn format_image_tool_output(
    path_display: &str,
    size_kb: usize,
    model: &str,
    prompt: &str,
) -> String {
    format!(
        "Image generated successfully.\n\
         File: {path_display}\n\
         Size: {size_kb} KB\n\
         Model: {model}\n\
         Prompt: {prompt}\n\
         [IMAGE:{path_display}]",
    )
}

pub struct ImageGenTool {
    security: Arc<SecurityPolicy>,
    workspace_dir: PathBuf,
    default_model: String,
    api_key_env: String,
    persistent_writes: bool,
}

impl ImageGenTool {
    pub fn new(
        security: Arc<SecurityPolicy>,
        workspace_dir: PathBuf,
        default_model: String,
        api_key_env: String,
    ) -> Self {
        Self {
            security,
            workspace_dir,
            default_model,
            api_key_env,
            persistent_writes: true,
        }
    }

    /// Construct with an explicit persistence flag derived from the active
    /// runtime adapter's `has_filesystem_access()`. Mirrors
    /// [`super::file_write::FileWriteTool::new_with_persistence`].
    pub fn new_with_persistence(
        security: Arc<SecurityPolicy>,
        workspace_dir: PathBuf,
        default_model: String,
        api_key_env: String,
        persistent_writes: bool,
    ) -> Self {
        Self {
            security,
            workspace_dir,
            default_model,
            api_key_env,
            persistent_writes,
        }
    }

    /// Build a reusable HTTP client with reasonable timeouts.
    fn http_client() -> reqwest::Client {
        reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(120))
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .unwrap_or_default()
    }

    async fn download_generated_image(image_url: &str) -> anyhow::Result<Vec<u8>> {
        let mut current_url =
            reqwest::Url::parse(image_url).context("Invalid generated image URL")?;

        for redirect_count in 0..=MAX_IMAGE_REDIRECTS {
            let target = validate_image_target(current_url.as_str()).await?;
            let builder = reqwest::Client::builder()
                .timeout(std::time::Duration::from_secs(120))
                .connect_timeout(std::time::Duration::from_secs(10))
                .redirect(reqwest::redirect::Policy::none());
            let builder = if target.host.parse::<IpAddr>().is_ok() {
                builder
            } else {
                builder.resolve_to_addrs(&target.host, &target.resolved_addrs)
            };
            let client = builder
                .build()
                .context("Failed to build generated image HTTP client")?;
            let response = client
                .get(target.url.clone())
                .send()
                .await
                .context("Failed to download generated image")?;

            if response.status().is_redirection() {
                if redirect_count == MAX_IMAGE_REDIRECTS {
                    anyhow::bail!("Too many generated image redirects (max {MAX_IMAGE_REDIRECTS})");
                }
                let location = response
                    .headers()
                    .get(LOCATION)
                    .ok_or_else(|| anyhow::Error::msg("Generated image redirect omitted Location"))?
                    .to_str()
                    .context("Generated image redirect Location is not valid text")?;
                current_url = resolve_redirect_url(&target.url, location)?;
                continue;
            }

            if !response.status().is_success() {
                anyhow::bail!(
                    "Generated image download failed with HTTP {}",
                    response.status()
                );
            }

            let body = response_body::read_bounded(response, Some(GENERATED_IMAGE_LIMIT_BYTES))
                .await
                .context("Failed to read generated image bytes")?;
            if body.overflowed {
                anyhow::bail!(
                    "Generated image exceeds the {} MiB size limit",
                    GENERATED_IMAGE_LIMIT_BYTES / (1024 * 1024)
                );
            }
            return Ok(body.bytes);
        }

        unreachable!("redirect loop exits through success or redirect limit")
    }

    /// Read an API key from the environment.
    fn read_api_key(env_var: &str) -> Result<String, String> {
        std::env::var(env_var)
            .map(|v| v.trim().to_string())
            .ok()
            .filter(|v| !v.is_empty())
            .ok_or_else(|| format!("Missing API key: set the {env_var} environment variable"))
    }

    /// Core generation logic: call fal.ai, download image, save to disk.
    async fn generate(&self, args: serde_json::Value) -> anyhow::Result<ToolResult> {
        // ── Parse parameters ───────────────────────────────────────
        let prompt = match args.get("prompt").and_then(|v| v.as_str()) {
            Some(p) if !p.trim().is_empty() => p.trim().to_string(),
            _ => {
                return Ok(ToolResult {
                    success: false,
                    output: ToolOutput::default(),
                    error: Some("Missing required parameter: 'prompt'".into()),
                });
            }
        };

        // Sanitize filename — strip path components to prevent traversal.
        // When the caller doesn't provide one, generate a unique default so
        // successive calls without an explicit name never clobber each other.
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        let safe_name =
            resolve_image_filename(args.get("filename").and_then(|v| v.as_str()), nanos);

        let size = args
            .get("size")
            .and_then(|v| v.as_str())
            .unwrap_or("square_hd");

        // Validate size enum.
        const VALID_SIZES: &[&str] = &[
            "square_hd",
            "landscape_4_3",
            "portrait_4_3",
            "landscape_16_9",
            "portrait_16_9",
        ];
        if !VALID_SIZES.contains(&size) {
            return Ok(ToolResult {
                success: false,
                output: ToolOutput::default(),
                error: Some(format!(
                    "Invalid size '{size}'. Valid values: {}",
                    VALID_SIZES.join(", ")
                )),
            });
        }

        let model = args
            .get("model")
            .and_then(|v| v.as_str())
            .filter(|s| !s.trim().is_empty())
            .unwrap_or(&self.default_model);

        // Validate model identifier: must look like a fal.ai model path
        // (e.g. "fal-ai/flux/schnell"). Reject values with "..", query
        // strings, or fragments that could redirect the HTTP request.
        if model.contains("..")
            || model.contains('?')
            || model.contains('#')
            || model.contains('\\')
            || model.starts_with('/')
        {
            return Ok(ToolResult {
                success: false,
                output: ToolOutput::default(),
                error: Some(format!(
                    "Invalid model identifier '{model}'. \
                     Must be a fal.ai model path (e.g. 'fal-ai/flux/schnell')."
                )),
            });
        }

        // ── Read API key ───────────────────────────────────────────
        let api_key = match Self::read_api_key(&self.api_key_env) {
            Ok(k) => k,
            Err(msg) => {
                return Ok(ToolResult {
                    success: false,
                    output: ToolOutput::default(),
                    error: Some(msg),
                });
            }
        };

        // ── Call fal.ai ────────────────────────────────────────────
        let client = Self::http_client();
        let url = format!("https://fal.run/{model}");

        let body = json!({
            "prompt": prompt,
            "image_size": size,
            "num_images": 1
        });

        let resp = client
            .post(&url)
            .header("Authorization", format!("Key {api_key}"))
            .header("Content-Type", "application/json")
            .json(&body)
            .send()
            .await
            .context("fal.ai request failed")?;

        let status = resp.status();
        if !status.is_success() {
            let body_text = response_body::read_text(resp, Some(FAL_ERROR_LIMIT_BYTES))
                .await
                .map(|(text, _)| text)
                .unwrap_or_default();
            return Ok(ToolResult {
                success: false,
                output: ToolOutput::default(),
                error: Some(format!("fal.ai API error ({status}): {body_text}")),
            });
        }

        let fal_body = response_body::read_bounded(resp, Some(FAL_RESPONSE_LIMIT_BYTES))
            .await
            .context("Failed to read fal.ai response")?;
        if fal_body.overflowed {
            anyhow::bail!("fal.ai response exceeds the 1 MiB size limit");
        }
        let resp_json: serde_json::Value = serde_json::from_slice(&fal_body.bytes)
            .context("Failed to parse fal.ai response as JSON")?;

        let image_url = resp_json
            .pointer("/images/0/url")
            .and_then(|v| v.as_str())
            .ok_or_else(|| {
                ::zeroclaw_log::record!(
                    ERROR,
                    ::zeroclaw_log::Event::new(module_path!(), ::zeroclaw_log::Action::Fail)
                        .with_outcome(::zeroclaw_log::EventOutcome::Failure),
                    "image_gen: fal.ai response missing image URL"
                );
                anyhow::Error::msg("No image URL in fal.ai response")
            })?;

        // ── Download image ─────────────────────────────────────────
        let bytes = match Self::download_generated_image(image_url).await {
            Ok(bytes) => bytes,
            Err(error) => {
                return Ok(ToolResult {
                    success: false,
                    output: ToolOutput::default(),
                    error: Some(error.to_string()),
                });
            }
        };

        // ── Save to disk ───────────────────────────────────────────
        let images_dir = self.workspace_dir.join("images");
        tokio::fs::create_dir_all(&images_dir)
            .await
            .context("Failed to create images directory")?;

        let output_path = images_dir.join(format!("{safe_name}.png"));
        tokio::fs::write(&output_path, &bytes)
            .await
            .context("Failed to write image file")?;

        let size_kb = bytes.len() / 1024;

        let path_display = output_path.display().to_string();
        let output = format_image_tool_output(&path_display, size_kb, model, &prompt);

        Ok(ToolResult {
            success: true,
            output: output.into(),
            error: None,
        })
    }
}

#[async_trait]
impl Tool for ImageGenTool {
    fn name(&self) -> &str {
        "image_gen"
    }

    fn description(&self) -> &str {
        "Generate an image from a text prompt using fal.ai (Flux models). \
         Saves the result to the workspace images directory and returns the file path."
    }

    fn parameters_schema(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "required": ["prompt"],
            "properties": {
                "prompt": {
                    "type": "string",
                    "description": "Text prompt describing the image to generate."
                },
                "filename": {
                    "type": "string",
                    "description": "Output filename without extension (default: 'generated_image'). Saved as PNG in workspace/images/."
                },
                "size": {
                    "type": "string",
                    "enum": ["square_hd", "landscape_4_3", "portrait_4_3", "landscape_16_9", "portrait_16_9"],
                    "description": "Image aspect ratio / size preset (default: 'square_hd')."
                },
                "model": {
                    "type": "string",
                    "description": "fal.ai model identifier (default: 'fal-ai/flux/schnell')."
                }
            }
        })
    }

    async fn execute(&self, args: serde_json::Value) -> anyhow::Result<ToolResult> {
        // Security: image generation is a side-effecting action (HTTP + file write).
        if let Err(error) = self
            .security
            .enforce_tool_operation(ToolOperation::Act, "image_gen")
        {
            return Ok(ToolResult {
                success: false,
                output: ToolOutput::default(),
                error: Some(error),
            });
        }

        let mut result = self.generate(args).await?;
        // A generated image saved to an ephemeral workspace never reaches the
        // host and is lost at session end; warn loudly on success
        if !self.persistent_writes && result.success {
            result.output = with_ephemeral_workspace_warning(&result.output).into();
        }
        Ok(result)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use zeroclaw_config::autonomy::AutonomyLevel;
    use zeroclaw_config::policy::SecurityPolicy;

    fn test_security() -> Arc<SecurityPolicy> {
        Arc::new(SecurityPolicy {
            autonomy: AutonomyLevel::Full,
            workspace_dir: std::env::temp_dir(),
            ..SecurityPolicy::default()
        })
    }

    fn test_tool() -> ImageGenTool {
        ImageGenTool::new(
            test_security(),
            std::env::temp_dir(),
            "fal-ai/flux/schnell".into(),
            "FAL_API_KEY".into(),
        )
    }

    #[test]
    fn generated_image_target_rejects_private_and_userinfo_urls() {
        for url in [
            "https://127.0.0.1/image.png",
            "https://169.254.169.254/latest/meta-data",
            "https://user@example.com/image.png",
        ] {
            assert!(parse_public_https_url(url).is_err(), "accepted {url}");
        }
    }

    #[test]
    fn generated_image_redirect_to_private_target_is_rejected() {
        let current = reqwest::Url::parse("https://cdn.example.com/image.png").unwrap();
        let redirect = resolve_redirect_url(&current, "https://127.0.0.1/private.png").unwrap();

        assert!(parse_public_https_url(redirect.as_str()).is_err());
    }

    #[test]
    fn generated_image_target_requires_https() {
        assert!(parse_public_https_url("http://example.com/image.png").is_err());
        assert!(parse_public_https_url("https://example.com/image.png").is_ok());
    }

    #[test]
    fn generated_image_target_rejects_trailing_dot_host() {
        assert!(parse_public_https_url("https://example.com./image.png").is_err());
    }

    #[tokio::test]
    async fn fal_client_does_not_follow_redirects() {
        use tokio::io::AsyncWriteExt;

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        zeroclaw_spawn::spawn!(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            stream
                .write_all(
                    b"HTTP/1.1 302 Found\r\nLocation: http://127.0.0.1/unchecked\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                )
                .await
                .unwrap();
        });

        let response = ImageGenTool::http_client()
            .get(format!("http://{addr}/start"))
            .send()
            .await
            .unwrap();

        assert_eq!(response.status(), reqwest::StatusCode::FOUND);
    }

    #[test]
    fn tool_name() {
        let tool = test_tool();
        assert_eq!(tool.name(), "image_gen");
    }

    #[test]
    fn tool_description_is_nonempty() {
        let tool = test_tool();
        assert!(!tool.description().is_empty());
        assert!(tool.description().contains("image"));
    }

    #[test]
    fn tool_schema_has_required_prompt() {
        let tool = test_tool();
        let schema = tool.parameters_schema();
        assert_eq!(schema["required"], json!(["prompt"]));
        assert!(schema["properties"]["prompt"].is_object());
    }

    #[test]
    fn tool_schema_has_optional_params() {
        let tool = test_tool();
        let schema = tool.parameters_schema();
        assert!(schema["properties"]["filename"].is_object());
        assert!(schema["properties"]["size"].is_object());
        assert!(schema["properties"]["model"].is_object());
    }

    #[test]
    fn tool_spec_roundtrip() {
        let tool = test_tool();
        let spec = tool.spec();
        assert_eq!(spec.name, "image_gen");
        assert!(spec.parameters.is_object());
    }

    #[tokio::test]
    async fn missing_prompt_returns_error() {
        let tool = test_tool();
        let result = tool.execute(json!({})).await.unwrap();
        assert!(!result.success);
        assert!(result.error.as_deref().unwrap().contains("prompt"));
    }

    #[tokio::test]
    async fn empty_prompt_returns_error() {
        let tool = test_tool();
        let result = tool.execute(json!({"prompt": "   "})).await.unwrap();
        assert!(!result.success);
        assert!(result.error.as_deref().unwrap().contains("prompt"));
    }

    #[tokio::test]
    async fn missing_api_key_returns_error() {
        // Temporarily ensure the env var is unset.
        let original = std::env::var("FAL_API_KEY_TEST_IMAGE_GEN").ok();
        // SAFETY: test-only, single-threaded test runner.
        unsafe { std::env::remove_var("FAL_API_KEY_TEST_IMAGE_GEN") };

        let tool = ImageGenTool::new(
            test_security(),
            std::env::temp_dir(),
            "fal-ai/flux/schnell".into(),
            "FAL_API_KEY_TEST_IMAGE_GEN".into(),
        );
        let result = tool
            .execute(json!({"prompt": "a sunset over the ocean"}))
            .await
            .unwrap();
        assert!(!result.success);
        assert!(
            result
                .error
                .as_deref()
                .unwrap()
                .contains("FAL_API_KEY_TEST_IMAGE_GEN")
        );

        // Restore if it was set.
        if let Some(val) = original {
            // SAFETY: test-only, single-threaded test runner.
            unsafe { std::env::set_var("FAL_API_KEY_TEST_IMAGE_GEN", val) };
        }
    }

    #[tokio::test]
    async fn invalid_size_returns_error() {
        // Set a dummy key so we get past the key check.
        // SAFETY: test-only, single-threaded test runner.
        unsafe { std::env::set_var("FAL_API_KEY_TEST_SIZE", "dummy_key") };

        let tool = ImageGenTool::new(
            test_security(),
            std::env::temp_dir(),
            "fal-ai/flux/schnell".into(),
            "FAL_API_KEY_TEST_SIZE".into(),
        );
        let result = tool
            .execute(json!({"prompt": "test", "size": "invalid_size"}))
            .await
            .unwrap();
        assert!(!result.success);
        assert!(result.error.as_deref().unwrap().contains("Invalid size"));

        // SAFETY: test-only, single-threaded test runner.
        unsafe { std::env::remove_var("FAL_API_KEY_TEST_SIZE") };
    }

    #[tokio::test]
    async fn read_only_autonomy_blocks_execution() {
        let security = Arc::new(SecurityPolicy {
            autonomy: AutonomyLevel::ReadOnly,
            workspace_dir: std::env::temp_dir(),
            ..SecurityPolicy::default()
        });
        let tool = ImageGenTool::new(
            security,
            std::env::temp_dir(),
            "fal-ai/flux/schnell".into(),
            "FAL_API_KEY".into(),
        );
        let result = tool.execute(json!({"prompt": "test image"})).await.unwrap();
        assert!(!result.success);
        let err = result.error.as_deref().unwrap();
        assert!(
            err.contains("read-only") || err.contains("image_gen"),
            "expected read-only or image_gen in error, got: {err}"
        );
    }

    #[tokio::test]
    async fn invalid_model_with_traversal_returns_error() {
        // SAFETY: test-only, single-threaded test runner.
        unsafe { std::env::set_var("FAL_API_KEY_TEST_MODEL", "dummy_key") };

        let tool = ImageGenTool::new(
            test_security(),
            std::env::temp_dir(),
            "fal-ai/flux/schnell".into(),
            "FAL_API_KEY_TEST_MODEL".into(),
        );
        let result = tool
            .execute(json!({"prompt": "test", "model": "../../evil-endpoint"}))
            .await
            .unwrap();
        assert!(!result.success);
        assert!(
            result
                .error
                .as_deref()
                .unwrap()
                .contains("Invalid model identifier")
        );

        // SAFETY: test-only, single-threaded test runner.
        unsafe { std::env::remove_var("FAL_API_KEY_TEST_MODEL") };
    }

    #[test]
    fn read_api_key_missing() {
        let result = ImageGenTool::read_api_key("DEFINITELY_NOT_SET_ZC_TEST_12345");
        assert!(result.is_err());
        assert!(
            result
                .unwrap_err()
                .contains("DEFINITELY_NOT_SET_ZC_TEST_12345")
        );
    }

    #[test]
    fn filename_traversal_is_sanitized() {
        // Verify that path traversal in filenames is stripped to just the final component.
        let sanitized = PathBuf::from("../../etc/passwd").file_name().map_or_else(
            || "generated_image".to_string(),
            |n| n.to_string_lossy().to_string(),
        );
        assert_eq!(sanitized, "passwd");

        // ".." alone has no file_name, falls back to default.
        let sanitized = PathBuf::from("..").file_name().map_or_else(
            || "generated_image".to_string(),
            |n| n.to_string_lossy().to_string(),
        );
        assert_eq!(sanitized, "generated_image");
    }

    #[test]
    fn resolve_image_filename_default_is_non_clobbering_and_unique() {
        // Exercises the PRODUCTION filename-selection helper an omitted
        // filename must yield a unique timestamped name, never the bare
        // `generated_image` that would clobber prior generations, and two
        // default calls must differ. Fails if the code reverts to a fixed name.
        let a = resolve_image_filename(None, 1_000);
        let b = resolve_image_filename(None, 2_000);
        assert_eq!(a, "generated_image_1000");
        assert_ne!(
            a, "generated_image",
            "default must not clobber the bare name"
        );
        assert_ne!(a, b, "successive default names must differ");
        // An explicit filename is used verbatim, with path components stripped.
        assert_eq!(resolve_image_filename(Some("my_pic"), 1_000), "my_pic");
        assert_eq!(
            resolve_image_filename(Some("../../etc/passwd"), 1_000),
            "passwd"
        );
        // Blank/whitespace filename falls back to the timestamped default.
        assert_eq!(
            resolve_image_filename(Some("   "), 1_000),
            "generated_image_1000"
        );
    }

    #[test]
    fn image_output_emits_matching_file_line_and_image_marker() {
        let path = "/ws/images/generated_image_42.png";
        let out = format_image_tool_output(path, 12, "fal-ai/flux", "a cat");
        assert!(
            out.contains(&format!("File: {path}")),
            "output must carry a durable File: line: {out}"
        );
        assert!(
            out.contains(&format!("[IMAGE:{path}]")),
            "output must carry a matching [IMAGE:<path>] marker: {out}"
        );
    }

    #[test]
    fn read_api_key_present() {
        // SAFETY: test-only, single-threaded test runner.
        unsafe { std::env::set_var("ZC_IMAGE_GEN_TEST_KEY", "test_value_123") };
        let result = ImageGenTool::read_api_key("ZC_IMAGE_GEN_TEST_KEY");
        assert!(result.is_ok());
        assert_eq!(result.unwrap(), "test_value_123");
        // SAFETY: test-only, single-threaded test runner.
        unsafe { std::env::remove_var("ZC_IMAGE_GEN_TEST_KEY") };
    }
}
