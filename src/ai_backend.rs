//! Pluggable AI backend for chat-style interactions.
//!
//! Provides a `Backend` whose `chat(system, user)` method sends a
//! message pair to a language model and returns the assistant reply.

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::sync::Mutex;
use std::time::Instant;

const DEFAULT_API_URL: &str = "https://models.inference.ai.azure.com";
const DEFAULT_MODEL: &str = "gpt-4o";

const DEFAULT_COPILOT_CLI: &str = "npx -y @github/copilot";
const DEFAULT_OLLAMA_URL: &str = "http://localhost:11434/v1";
const DEFAULT_OLLAMA_MODEL: &str = "qwen3:4b";
const GITHUB_MODELS_URL: &str = "https://models.github.ai/inference";
const DEFAULT_GITHUB_MODELS_MODEL: &str = "gpt-4.1";
const DEFAULT_AZURE_MODEL: &str = "Mistral-Large-3";
const MAX_RETRY_WAIT_SECS: u64 = 300;

/// Exponential backoff: 10s, 20s, 40s, 80s, capped at 120s.
fn retry_backoff_secs(attempt: u32) -> u64 {
    std::cmp::min(10u64 << (attempt - 1), 120)
}

/// Return `true` if the `AI_BACKEND_DUMP_HTTP` env var is set to `1`.
/// When enabled, `chat_api` emits full HTTP response diagnostics
/// (status, every header, full body) to stderr.  This is a
/// diagnostic-only knob meant to be toggled per-invocation by tools
/// like `test-prompt --dump-http`; production runs never set it.
fn dump_http_enabled() -> bool {
    matches!(std::env::var("AI_BACKEND_DUMP_HTTP").as_deref(), Ok("1"))
}

/// Dump status code and every response header to stderr.  Header
/// values that are not UTF-8 are rendered as `<non-utf8>` rather
/// than skipped, so the dump is faithful even for binary header
/// values that no production server should ever send.
fn dump_response_meta(response: &reqwest::Response) {
    eprintln!("[dump-http] response status: {}", response.status());
    for (name, value) in response.headers().iter() {
        let v = value.to_str().unwrap_or("<non-utf8>");
        eprintln!("[dump-http] response header: {name}: {v}");
    }
}

/// Returned when the chat API responds with HTTP 200 but no
/// `choices`.  Azure OpenAI deployments do this when, for example,
/// the prompt exceeds the model's context window, or a content
/// filter trips on input that the gateway accepted without an
/// explicit 400.  Callers can walk the `anyhow::Error::chain()` and
/// downcast to this type via `is_no_choices()` to recognize the
/// condition and recover (typically by skipping the offending item)
/// rather than treating it as a fatal abort.
#[derive(Debug)]
pub struct NoChoicesError {
    pub body_snippet: String,
}

impl std::fmt::Display for NoChoicesError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "no choices in API response; body snippet: {}",
            self.body_snippet
        )
    }
}

impl std::error::Error for NoChoicesError {}

/// Return `true` if `err` (or any error in its source chain) is a
/// `NoChoicesError`.  Use this in callers that want to convert the
/// "no choices" failure mode into a soft skip while still propagating
/// every other failure.
pub fn is_no_choices(err: &anyhow::Error) -> bool {
    err.chain()
        .any(|c| c.downcast_ref::<NoChoicesError>().is_some())
}

/// Returned when the chat API rejects the request because Azure's
/// Responsible AI content filter classifies the input or output as
/// disallowed.  Surfaces as either HTTP 400 with a body containing
/// `"code":"content_filter"` (input rejection) or HTTP 200 with
/// `"finish_reason":"content_filter"` (output rejection); both
/// shapes carry an empty assistant message that no amount of
/// retrying will populate.  Callers should treat this like
/// `NoChoicesError`: walk the `anyhow` chain via
/// `is_content_filter()` and convert to a soft skip.
///
/// Observed in practice on the digestive pipeline against entirely
/// innocuous technical email (a refactor of repository-format
/// handling in setup.c that happens to call the `die()` helper),
/// where the gateway returned HTTP 400 and the loop burned ~54 k
/// tokens across five retries that had zero chance of succeeding.
#[derive(Debug)]
pub struct ContentFilterError {
    pub body_snippet: String,
}

impl std::fmt::Display for ContentFilterError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "content filter rejected request; body snippet: {}",
            self.body_snippet
        )
    }
}

impl std::error::Error for ContentFilterError {}

/// Return `true` if `err` (or any error in its source chain) is a
/// `ContentFilterError`.
pub fn is_content_filter(err: &anyhow::Error) -> bool {
    err.chain()
        .any(|c| c.downcast_ref::<ContentFilterError>().is_some())
}

/// Substring check that recognizes Azure's content-filter markers
/// in a raw response body.  Matches both the snake_case form
/// (`"code":"content_filter"`, the JSON the documented schema
/// emits) and the PascalCase form (`ContentFilter` / `Responsible
/// AI`, which appear in human-readable messages and in some older
/// deployments' responses).
fn body_is_content_filter(body: &str) -> bool {
    body.contains("content_filter")
        || body.contains("ContentFilter")
        || body.contains("Responsible AI")
        || body.contains("ResponsibleAI")
}

/// Tracks rate limit state reported by the API via `x-ratelimit-*` headers.
pub struct RateLimitState {
    pub remaining_requests: Option<u64>,
    pub remaining_tokens: Option<u64>,
    pub last_updated: Instant,
}

/// How to authenticate with an API endpoint.
pub enum ApiAuth<'a> {
    /// Bearer token in the Authorization header.
    Bearer(&'a str),
    /// API key in a custom header (e.g. Azure OpenAI uses `api-key`).
    ApiKey(&'a str),
    /// No authentication required.
    None,
}

/// Endpoint-specific parameters for chat_api(), grouping the
/// per-backend state that varies across Backend variants.
struct ApiEndpoint<'a> {
    api_url: &'a str,
    model: &'a str,
    auth: ApiAuth<'a>,
    rate_limits: Option<&'a Mutex<RateLimitState>>,
}

/// An AI chat backend.
pub enum Backend {
    /// OpenAI-compatible chat completions API.
    Api {
        api_url: String,
        model: String,
        token: Option<String>,
    },
    /// Shell out to a Copilot-CLI-compatible command.
    CopilotCli {
        command: String,
        model: Option<String>,
    },
    /// Local Ollama instance (OpenAI-compatible API, no auth).
    Ollama { api_url: String, model: String },
    /// GitHub Models (models.github.ai) — OpenAI-compatible, separate from Copilot.
    GitHubModels {
        model: String,
        token: String,
        rate_limits: Mutex<RateLimitState>,
    },
    /// Azure OpenAI Service — uses `api-key` header for authentication.
    AzureOpenAI {
        api_url: String,
        model: String,
        api_key: String,
    },
    /// Deterministic mock for testing.  Returns every Nth word from
    /// the user message (default: every 5th word).
    #[cfg(test)]
    Mock { nth_word: usize },
    /// Mock that always fails with `NoChoicesError`, simulating an
    /// API response with `choices: []` (e.g. when the prompt exceeds
    /// the model context window).
    #[cfg(test)]
    MockNoChoices,
    /// Mock that always fails with `ContentFilterError`, simulating
    /// an Azure Responsible AI content-filter rejection.
    #[cfg(test)]
    MockContentFilter,
}

impl Backend {
    /// Build an Api backend from environment variables.
    ///
    /// Reads `GITHUB_TOKEN` or `OPENAI_API_KEY` for the token,
    /// `GIT_DIGEST_API_URL` for the endpoint (default: Azure AI),
    /// and `GIT_DIGEST_MODEL` for the model (default: gpt-4o).
    pub fn api_from_env() -> Result<Self> {
        let token = std::env::var("GITHUB_TOKEN")
            .or_else(|_| std::env::var("OPENAI_API_KEY"))
            .context("GITHUB_TOKEN or OPENAI_API_KEY must be set")?;
        let api_url =
            std::env::var("GIT_DIGEST_API_URL").unwrap_or_else(|_| DEFAULT_API_URL.to_string());
        let model = std::env::var("GIT_DIGEST_MODEL").unwrap_or_else(|_| DEFAULT_MODEL.to_string());
        Ok(Self::Api {
            api_url,
            model,
            token: Some(token),
        })
    }

    pub fn copilot_cli(command: Option<String>, model: Option<String>) -> Self {
        Self::CopilotCli {
            command: command.unwrap_or_else(|| DEFAULT_COPILOT_CLI.to_string()),
            model,
        }
    }

    pub fn ollama(url: Option<String>, model: Option<String>) -> Self {
        Self::Ollama {
            api_url: url.unwrap_or_else(|| DEFAULT_OLLAMA_URL.to_string()),
            model: model.unwrap_or_else(|| DEFAULT_OLLAMA_MODEL.to_string()),
        }
    }

    pub fn github_models(token: String, model: Option<String>) -> Self {
        Self::GitHubModels {
            model: model.unwrap_or_else(|| DEFAULT_GITHUB_MODELS_MODEL.to_string()),
            token,
            rate_limits: Mutex::new(RateLimitState {
                remaining_requests: None,
                remaining_tokens: None,
                last_updated: Instant::now(),
            }),
        }
    }

    pub fn azure_openai(api_url: String, model: Option<String>, api_key: String) -> Self {
        Self::AzureOpenAI {
            api_url,
            model: model.unwrap_or_else(|| DEFAULT_AZURE_MODEL.to_string()),
            api_key,
        }
    }

    /// Send a system + user message pair and return the assistant reply.
    pub async fn chat(&self, system: &str, user: &str) -> Result<String> {
        self.chat_with_options(system, user, None).await
    }

    /// Like `chat`, but with optional temperature override.
    pub async fn chat_with_options(
        &self,
        system: &str,
        user: &str,
        temperature: Option<f32>,
    ) -> Result<String> {
        match self {
            Backend::Api {
                api_url,
                model,
                token,
            } => {
                let auth = match token.as_deref() {
                    Some(t) => ApiAuth::Bearer(t),
                    None => ApiAuth::None,
                };
                let ep = ApiEndpoint {
                    api_url,
                    model,
                    auth,
                    rate_limits: None,
                };
                chat_api(&ep, system, user, temperature).await
            }
            Backend::CopilotCli { command, model } => {
                chat_cli(command, model.as_deref(), system, user).await
            }
            Backend::Ollama { api_url, model } => {
                let ep = ApiEndpoint {
                    api_url,
                    model,
                    auth: ApiAuth::None,
                    rate_limits: None,
                };
                chat_api(&ep, system, user, temperature).await
            }
            Backend::GitHubModels {
                model,
                token,
                rate_limits,
            } => {
                let ep = ApiEndpoint {
                    api_url: GITHUB_MODELS_URL,
                    model,
                    auth: ApiAuth::Bearer(token),
                    rate_limits: Some(rate_limits),
                };
                chat_api(&ep, system, user, temperature).await
            }
            Backend::AzureOpenAI {
                api_url,
                model,
                api_key,
            } => {
                let ep = ApiEndpoint {
                    api_url,
                    model,
                    auth: ApiAuth::ApiKey(api_key),
                    rate_limits: None,
                };
                chat_api(&ep, system, user, temperature).await
            }
            #[cfg(test)]
            Backend::Mock { nth_word } => Ok(mock_summarize(user, *nth_word)),
            #[cfg(test)]
            Backend::MockNoChoices => Err(anyhow::Error::new(NoChoicesError {
                body_snippet: r#"{"choices": []}"#.to_string(),
            })),
            #[cfg(test)]
            Backend::MockContentFilter => Err(anyhow::Error::new(ContentFilterError {
                body_snippet: r#"{"choices":[{"finish_reason":"content_filter"}]}"#.to_string(),
            })),
        }
    }
}

/// Shared CLI arguments for backend selection.
///
/// Embed in any clap `Args` struct with `#[command(flatten)]` to get
/// `--copilot-cli`, `--ollama`, `--github-models`, `--azure-openai`,
/// and `--model` flags.
#[derive(clap::Args, Clone, Debug)]
#[command(group = clap::ArgGroup::new("backend-choice").multiple(false))]
pub struct BackendArgs {
    /// Use GitHub Copilot CLI instead of the API.
    /// Optionally specify a custom command (default: "npx -y @github/copilot").
    #[arg(long, num_args = 0..=1, default_missing_value = "", group = "backend-choice")]
    pub copilot_cli: Option<String>,

    /// Use a local Ollama instance. Optionally specify the URL
    /// (default: http://localhost:11434/v1).
    #[arg(long, value_name = "OLLAMA_URL", num_args = 0..=1, default_missing_value = "", group = "backend-choice")]
    pub ollama: Option<String>,

    /// Use GitHub Models (models.github.ai). Requires GITHUB_TOKEN env var
    /// with the `models` scope.
    #[arg(long, group = "backend-choice")]
    pub github_models: bool,

    /// Use Azure OpenAI Service. Optionally pass the endpoint URL
    /// (falls back to AZURE_OPENAI_ENDPOINT env var).
    /// Reads the API key from AZURE_OPENAI_API_KEY.
    /// Model defaults to Mistral-Large-3; override via --model or
    /// AZURE_OPENAI_MODEL env var.
    #[arg(long, num_args = 0..=1, default_missing_value = "", group = "backend-choice")]
    pub azure_openai: Option<String>,

    /// Model to use (applies to all backends).
    #[arg(long)]
    pub model: Option<String>,
}

impl BackendArgs {
    /// Resolve these CLI flags into a concrete `Backend`.
    pub fn resolve(self) -> Result<Backend> {
        if let Some(cmd) = self.copilot_cli {
            let cmd = if cmd.is_empty() { None } else { Some(cmd) };
            Ok(Backend::copilot_cli(cmd, self.model))
        } else if let Some(url) = self.ollama {
            let url = if url.is_empty() { None } else { Some(url) };
            Ok(Backend::ollama(url, self.model))
        } else if self.github_models {
            let token = std::env::var("GITHUB_TOKEN")
                .context("GITHUB_TOKEN must be set for --github-models (needs `models` scope)")?;
            Ok(Backend::github_models(token, self.model))
        } else if let Some(url) = self.azure_openai {
            let api_url = if url.is_empty() {
                std::env::var("AZURE_OPENAI_ENDPOINT").context(
                    "AZURE_OPENAI_ENDPOINT must be set (or pass the URL to --azure-openai)",
                )?
            } else {
                url
            };
            let api_key = std::env::var("AZURE_OPENAI_API_KEY")
                .context("AZURE_OPENAI_API_KEY must be set for --azure-openai")?;
            let model = self
                .model
                .or_else(|| std::env::var("AZURE_OPENAI_MODEL").ok());
            Ok(Backend::azure_openai(api_url, model, api_key))
        } else {
            let mut b = Backend::api_from_env()?;
            if let Backend::Api { ref mut model, .. } = b
                && let Some(m) = self.model
            {
                *model = m;
            }
            Ok(b)
        }
    }
}

#[derive(Serialize)]
struct ChatRequest {
    model: String,
    messages: Vec<ChatMessage>,
    #[serde(skip_serializing_if = "Option::is_none")]
    temperature: Option<f32>,
}

#[derive(Serialize, Deserialize)]
struct ChatMessage {
    role: String,
    content: String,
}

#[derive(Deserialize)]
struct ChatResponse {
    choices: Vec<ChatChoice>,
}

#[derive(Deserialize)]
struct ChatChoice {
    message: ChatMessage,
}

async fn chat_api(
    ep: &ApiEndpoint<'_>,
    system: &str,
    user: &str,
    temperature: Option<f32>,
) -> Result<String> {
    // Preemptive rate limit back-off: if we know we are running low on
    // requests and the information is recent (within the last 60 s), sleep
    // briefly to avoid hammering the API.
    if let Some(rl) = ep.rate_limits {
        let sleep_needed = {
            let state = rl.lock().expect("rate limit lock poisoned");
            state
                .remaining_requests
                .is_some_and(|rem| rem <= 5 && state.last_updated.elapsed().as_secs() < 60)
        };
        if sleep_needed {
            let wait = std::time::Duration::from_secs(15);
            eprintln!("[rate-limit] running low on requests — sleeping {wait:?}");
            tokio::time::sleep(wait).await;
        }
    }

    let client = reqwest::Client::new();
    let base = ep.api_url.trim_end_matches('/');
    let url = if base.ends_with("/chat/completions") || base.contains("/chat/completions?") {
        base.to_string()
    } else {
        format!("{base}/chat/completions")
    };
    let req = ChatRequest {
        model: ep.model.to_string(),
        messages: vec![
            ChatMessage {
                role: "system".into(),
                content: system.to_string(),
            },
            ChatMessage {
                role: "user".into(),
                content: user.to_string(),
            },
        ],
        temperature,
    };

    let max_retries: u32 = 5;
    let mut attempt = 0;
    loop {
        let mut builder = client.post(&url).json(&req);
        match &ep.auth {
            ApiAuth::Bearer(t) => {
                builder = builder.bearer_auth(t);
            }
            ApiAuth::ApiKey(k) => {
                builder = builder.header("api-key", *k);
            }
            ApiAuth::None => {}
        }
        let resp = match builder.send().await {
            Ok(r) => r,
            Err(e) => {
                attempt += 1;
                if attempt > max_retries {
                    return Err(e).context("API request failed");
                }
                let backoff = retry_backoff_secs(attempt);
                eprintln!(
                    "[retry] send error (attempt {attempt}/{max_retries}): \
                     {e:#}; sleeping {backoff}s"
                );
                tokio::time::sleep(std::time::Duration::from_secs(backoff)).await;
                continue;
            }
        };

        if resp.status() == reqwest::StatusCode::TOO_MANY_REQUESTS {
            attempt += 1;
            if attempt > max_retries {
                anyhow::bail!("API returned 429 Too Many Requests after {max_retries} retries");
            }
            // Prefer the server's retry-after header when present.
            let backoff = if let Some(ra) = resp.headers().get("retry-after") {
                if let Ok(s) = ra.to_str() {
                    if let Ok(secs) = s.parse::<u64>() {
                        if secs > MAX_RETRY_WAIT_SECS {
                            anyhow::bail!(
                                "API returned retry-after of {secs}s which exceeds \
                                 the {MAX_RETRY_WAIT_SECS}s cap -- giving up"
                            );
                        }
                        secs
                    } else {
                        retry_backoff_secs(attempt)
                    }
                } else {
                    retry_backoff_secs(attempt)
                }
            } else {
                retry_backoff_secs(attempt)
            };
            eprintln!(
                "[retry] 429 Too Many Requests (attempt {attempt}/{max_retries}), \
                 sleeping {backoff}s"
            );
            tokio::time::sleep(std::time::Duration::from_secs(backoff)).await;
            continue;
        }

        if resp.status().is_server_error()
            || resp.status() == reqwest::StatusCode::BAD_REQUEST
            || resp.status() == reqwest::StatusCode::NOT_FOUND
            || resp.status() == reqwest::StatusCode::REQUEST_TIMEOUT
        {
            attempt += 1;
            let status = resp.status();
            // Read the body so we can log it (and surface it in the
            // final bail message when retries are exhausted).  The
            // previous code dropped `resp` here without reading the
            // body, leaving CI logs with just the status code and no
            // gateway-provided explanation for the failure.
            if dump_http_enabled() {
                dump_response_meta(&resp);
            }
            let body = resp
                .text()
                .await
                .unwrap_or_else(|e| format!("<failed to read response body: {e}>"));
            if dump_http_enabled() {
                eprintln!("[dump-http] response body ({} bytes):\n{body}", body.len());
            }
            // Content-filter rejections (Azure's Responsible AI input
            // gate) are deterministic: the same input will always be
            // blocked.  Short-circuit out of the retry loop and return
            // a typed error so the caller can soft-skip the offending
            // item the same way it already does for `NoChoicesError`,
            // instead of burning ~5x the tokens on retries that have
            // zero chance of succeeding.
            if status == reqwest::StatusCode::BAD_REQUEST && body_is_content_filter(&body) {
                return Err(anyhow::Error::new(ContentFilterError {
                    body_snippet: body[..body.len().min(500)].to_string(),
                }));
            }
            if attempt > max_retries {
                anyhow::bail!(
                    "API returned {status} after {max_retries} retries; \
                     last response body: {}",
                    &body[..body.len().min(500)]
                );
            }
            let backoff = retry_backoff_secs(attempt);
            eprintln!(
                "[retry] {status} (attempt {attempt}/{max_retries}), \
                 body snippet: {}; sleeping {backoff}s",
                &body[..body.len().min(200)]
            );
            tokio::time::sleep(std::time::Duration::from_secs(backoff)).await;
            continue;
        }

        let response = resp
            .error_for_status()
            .context("API returned error status")?;

        if dump_http_enabled() {
            dump_response_meta(&response);
        }

        // Update rate limit state from response headers when available.
        if let Some(rl) = ep.rate_limits {
            let mut state = rl.lock().expect("rate limit lock poisoned");
            if let Some(v) = response.headers().get("x-ratelimit-remaining-requests")
                && let Ok(s) = v.to_str()
                && let Ok(n) = s.parse::<u64>()
            {
                state.remaining_requests = Some(n);
            }
            if let Some(v) = response.headers().get("x-ratelimit-remaining-tokens")
                && let Ok(s) = v.to_str()
                && let Ok(n) = s.parse::<u64>()
            {
                state.remaining_tokens = Some(n);
            }
            state.last_updated = Instant::now();
            eprintln!(
                "[rate-limit] remaining: requests={:?}, tokens={:?}",
                state.remaining_requests, state.remaining_tokens
            );
        }

        let body = response
            .text()
            .await
            .context("failed to read API response body")?;
        if dump_http_enabled() {
            eprintln!("[dump-http] response body ({} bytes):\n{body}", body.len());
        }
        let parsed: ChatResponse = serde_json::from_str(&body).with_context(|| {
            format!(
                "failed to parse API response: {}",
                &body[..body.len().min(200)]
            )
        })?;
        let content = match parsed.choices.into_iter().next() {
            Some(c) => c.message.content,
            None => {
                return Err(anyhow::Error::new(NoChoicesError {
                    body_snippet: body[..body.len().min(500)].to_string(),
                }));
            }
        };

        if content.trim().is_empty() {
            // Output-side content-filter rejections sometimes arrive
            // as HTTP 200 with an empty content string -- the body
            // still carries the `content_filter` marker.  Soft-skip
            // these the same way as the 400-shaped input rejection.
            if body_is_content_filter(&body) {
                return Err(anyhow::Error::new(ContentFilterError {
                    body_snippet: body[..body.len().min(500)].to_string(),
                }));
            }
            attempt += 1;
            if attempt > max_retries {
                anyhow::bail!(
                    "API returned empty content after {max_retries} retries; \
                     last response body: {}",
                    &body[..body.len().min(500)]
                );
            }
            let backoff = retry_backoff_secs(attempt);
            eprintln!(
                "[retry] empty response (attempt {attempt}/{max_retries}), \
                 body snippet: {}; sleeping {backoff}s",
                &body[..body.len().min(200)]
            );
            tokio::time::sleep(std::time::Duration::from_secs(backoff)).await;
            continue;
        }

        break Ok(content);
    }
}

async fn chat_cli(command: &str, model: Option<&str>, system: &str, user: &str) -> Result<String> {
    use std::io::Write;
    use tokio::process::Command;

    anyhow::ensure!(!command.is_empty(), "empty copilot-cli command");

    let mut tmp = tempfile::NamedTempFile::new().context("failed to create temp file")?;
    writeln!(tmp, "{system}\n\n---\n\n{user}")?;
    tmp.flush()?;
    let path = tmp.path().to_string_lossy().to_string();

    // Build the full shell command line so that quoted paths and
    // arguments in `command` are handled by the shell, not by naive
    // whitespace splitting.
    let mut shell_line = command.to_string();
    shell_line.push_str(&format!(
        " -p @{path} -s --no-custom-instructions --allow-all-tools"
    ));
    if let Some(m) = model {
        shell_line.push_str(&format!(" --model '{}'", m.replace('\'', "'\\''")));
    }

    let mut cmd = if cfg!(windows) {
        let mut c = Command::new("cmd");
        c.args(["/C", &shell_line]);
        c
    } else {
        let mut c = Command::new("sh");
        c.args(["-c", &shell_line]);
        c
    };

    let output = cmd.output().await.context("failed to run copilot CLI")?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        anyhow::bail!("copilot CLI exited with {}: {stderr}", output.status);
    }
    Ok(String::from_utf8(output.stdout)
        .context("copilot CLI output is not valid UTF-8")?
        .trim()
        .to_string())
}

/// Deterministic mock: return every `nth` word from the user message.
#[cfg(test)]
fn mock_summarize(user: &str, nth: usize) -> String {
    let nth = nth.max(1);
    user.split_whitespace()
        .skip(nth - 1)
        .step_by(nth)
        .collect::<Vec<_>>()
        .join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ok_chat_response(content: &str) -> String {
        serde_json::json!({
            "choices": [{"message": {"role": "assistant", "content": content}}]
        })
        .to_string()
    }

    #[tokio::test(start_paused = true)]
    async fn chat_api_retries_on_429() {
        use wiremock::matchers::method;
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;

        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(429))
            .up_to_n_times(2)
            .expect(2)
            .mount(&server)
            .await;

        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(200).set_body_string(ok_chat_response("ok")))
            .expect(1)
            .mount(&server)
            .await;

        let ep = ApiEndpoint {
            api_url: &server.uri(),
            model: "test",
            auth: ApiAuth::None,
            rate_limits: None,
        };
        let result = chat_api(&ep, "sys", "usr", None).await.unwrap();
        assert_eq!(result, "ok");
    }

    #[tokio::test(start_paused = true)]
    async fn chat_api_retries_on_5xx() {
        use wiremock::matchers::method;
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;

        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(502))
            .up_to_n_times(1)
            .expect(1)
            .mount(&server)
            .await;

        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(200).set_body_string(ok_chat_response("recovered")))
            .expect(1)
            .mount(&server)
            .await;

        let ep = ApiEndpoint {
            api_url: &server.uri(),
            model: "test",
            auth: ApiAuth::None,
            rate_limits: None,
        };
        let result = chat_api(&ep, "sys", "usr", None).await.unwrap();
        assert_eq!(result, "recovered");
    }

    #[tokio::test(start_paused = true)]
    async fn chat_api_retries_on_404() {
        // Azure OpenAI deployments occasionally return a transient 404
        // ("DeploymentNotFound") even when the deployment is healthy and
        // a probe a moment later succeeds.  Treat 404 the same as a
        // server error and retry.
        use wiremock::matchers::method;
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;

        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(404))
            .up_to_n_times(1)
            .expect(1)
            .mount(&server)
            .await;

        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(200).set_body_string(ok_chat_response("recovered")))
            .expect(1)
            .mount(&server)
            .await;

        let ep = ApiEndpoint {
            api_url: &server.uri(),
            model: "test",
            auth: ApiAuth::None,
            rate_limits: None,
        };
        let result = chat_api(&ep, "sys", "usr", None).await.unwrap();
        assert_eq!(result, "recovered");
    }

    #[tokio::test(start_paused = true)]
    async fn chat_api_retries_on_408() {
        // Azure OpenAI Chat Completions occasionally returns a transient
        // 408 Request Timeout when the server gives up waiting for or
        // producing the response.  A single retry typically succeeds;
        // surfacing it as a hard failure aborts long pipeline runs over
        // pure infrastructure flakes.
        use wiremock::matchers::method;
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;

        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(408))
            .up_to_n_times(1)
            .expect(1)
            .mount(&server)
            .await;

        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(200).set_body_string(ok_chat_response("recovered")))
            .expect(1)
            .mount(&server)
            .await;

        let ep = ApiEndpoint {
            api_url: &server.uri(),
            model: "test",
            auth: ApiAuth::None,
            rate_limits: None,
        };
        let result = chat_api(&ep, "sys", "usr", None).await.unwrap();
        assert_eq!(result, "recovered");
    }

    #[tokio::test(start_paused = true)]
    async fn chat_api_retries_on_empty_response() {
        use wiremock::matchers::method;
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;

        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(200).set_body_string(ok_chat_response("   ")))
            .up_to_n_times(1)
            .expect(1)
            .mount(&server)
            .await;

        Mock::given(method("POST"))
            .respond_with(
                ResponseTemplate::new(200).set_body_string(ok_chat_response("real content")),
            )
            .expect(1)
            .mount(&server)
            .await;

        let ep = ApiEndpoint {
            api_url: &server.uri(),
            model: "test",
            auth: ApiAuth::None,
            rate_limits: None,
        };
        let result = chat_api(&ep, "sys", "usr", None).await.unwrap();
        assert_eq!(result, "real content");
    }

    #[tokio::test(start_paused = true)]
    async fn chat_api_gives_up_after_max_retries() {
        use wiremock::matchers::method;
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;

        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(429))
            .expect(6) // 1 initial + 5 retries
            .mount(&server)
            .await;

        let ep = ApiEndpoint {
            api_url: &server.uri(),
            model: "test",
            auth: ApiAuth::None,
            rate_limits: None,
        };
        let err = chat_api(&ep, "sys", "usr", None).await.unwrap_err();
        let msg = format!("{err:#}");
        assert!(
            msg.contains("429") && msg.contains("5 retries"),
            "unexpected error: {msg}"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn chat_api_400_bail_includes_response_body() {
        // When the chat API responds with HTTP 400 (a typed failure
        // mode like Azure's content-filter reject, malformed schema,
        // or context-length over-budget that the gateway surfaces as
        // 400 rather than as an empty `choices` array), the previous
        // code would retry five times and bail with just the status
        // code, swallowing whatever explanation the gateway provided
        // in the response body.  The body must survive into the
        // final error so CI logs are actionable.
        use wiremock::matchers::method;
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        let body =
            r#"{"error":{"message":"diagnostic_marker_xyz","code":"context_length_exceeded"}}"#;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(400).set_body_string(body))
            .expect(6) // 1 initial + 5 retries
            .mount(&server)
            .await;

        let ep = ApiEndpoint {
            api_url: &server.uri(),
            model: "test",
            auth: ApiAuth::None,
            rate_limits: None,
        };
        let err = chat_api(&ep, "sys", "usr", None).await.unwrap_err();
        let msg = format!("{err:#}");
        assert!(
            msg.contains("400") && msg.contains("5 retries"),
            "should mention 400 + retry count: {msg}"
        );
        assert!(
            msg.contains("diagnostic_marker_xyz"),
            "bail message should include the response body: {msg}"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn chat_api_400_content_filter_short_circuits_without_retry() {
        // Azure's Responsible AI content filter returns HTTP 400 with
        // `"code":"content_filter"` for input rejections.  The filter
        // verdict is deterministic, so retries are pure waste; assert
        // that `chat_api` returns a typed `ContentFilterError`
        // immediately (exactly one upstream request, no retries).
        use wiremock::matchers::method;
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        let body = r#"{"id":"x","choices":[{"index":0,"message":{"role":"assistant","content":""},"finish_reason":"content_filter","content_filter_results":{"error":{"code":"content_filter","message":"ResponsibleAI block."}}}]}"#;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(400).set_body_string(body))
            .expect(1) // no retries on content filter
            .mount(&server)
            .await;

        let ep = ApiEndpoint {
            api_url: &server.uri(),
            model: "test",
            auth: ApiAuth::None,
            rate_limits: None,
        };
        let err = chat_api(&ep, "sys", "usr", None).await.unwrap_err();
        assert!(
            is_content_filter(&err),
            "expected ContentFilterError, got: {err:#}"
        );
        let cf = err
            .chain()
            .find_map(|c| c.downcast_ref::<ContentFilterError>())
            .expect("downcast ContentFilterError");
        assert!(cf.body_snippet.contains("content_filter"));
    }

    #[tokio::test(start_paused = true)]
    async fn chat_api_200_content_filter_short_circuits_without_retry() {
        // Some output-side filter trips arrive as HTTP 200 with an
        // empty content string and a `content_filter` marker in the
        // body.  These must also short-circuit instead of going into
        // the empty-content retry loop, which would burn five rounds
        // of tokens on a deterministic rejection.
        use wiremock::matchers::method;
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        let body = r#"{"id":"x","choices":[{"index":0,"message":{"role":"assistant","content":""},"finish_reason":"content_filter"}]}"#;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(200).set_body_string(body))
            .expect(1) // no retries
            .mount(&server)
            .await;

        let ep = ApiEndpoint {
            api_url: &server.uri(),
            model: "test",
            auth: ApiAuth::None,
            rate_limits: None,
        };
        let err = chat_api(&ep, "sys", "usr", None).await.unwrap_err();
        assert!(
            is_content_filter(&err),
            "expected ContentFilterError, got: {err:#}"
        );
    }

    #[test]
    fn is_content_filter_walks_anyhow_context_chain() {
        let leaf = anyhow::Error::new(ContentFilterError {
            body_snippet: r#"{"code":"content_filter"}"#.to_string(),
        });
        let wrapped = leaf.context("human summary failed");
        assert!(is_content_filter(&wrapped));
        assert!(!is_no_choices(&wrapped));
    }

    #[test]
    fn is_content_filter_returns_false_for_unrelated_errors() {
        let err = anyhow::anyhow!("API returned 400 after 5 retries");
        assert!(!is_content_filter(&err));
    }

    #[test]
    fn body_is_content_filter_recognizes_snake_and_pascal_case() {
        assert!(body_is_content_filter(r#"{"code":"content_filter"}"#));
        assert!(body_is_content_filter(r#"{"name":"ContentFilter"}"#));
        assert!(body_is_content_filter(
            "ResponsibleAI result indicated block"
        ));
        assert!(body_is_content_filter(
            "Responsible AI policy returned block"
        ));
        assert!(!body_is_content_filter(r#"{"error":"unauthorized"}"#));
        assert!(!body_is_content_filter(
            "the body contains no filter marker"
        ));
    }

    #[tokio::test(start_paused = true)]
    async fn chat_api_bails_on_excessive_retry_after() {
        use wiremock::matchers::method;
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;

        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(429).insert_header("retry-after", "86400"))
            .expect(1)
            .mount(&server)
            .await;

        let ep = ApiEndpoint {
            api_url: &server.uri(),
            model: "test",
            auth: ApiAuth::None,
            rate_limits: None,
        };
        let err = chat_api(&ep, "sys", "usr", None).await.unwrap_err();
        let msg = format!("{err:#}");
        assert!(
            msg.contains("86400") && msg.contains("cap"),
            "unexpected error: {msg}"
        );
    }

    #[test]
    fn mock_every_fifth_word() {
        let input = "one two three four five six seven eight nine ten";
        assert_eq!(mock_summarize(input, 5), "five ten");
    }

    #[test]
    fn mock_every_word() {
        let input = "hello world";
        assert_eq!(mock_summarize(input, 1), "hello world");
    }

    #[test]
    fn mock_nth_zero_clamps_to_one() {
        let input = "a b c";
        assert_eq!(mock_summarize(input, 0), "a b c");
    }

    #[test]
    fn mock_empty_input() {
        assert_eq!(mock_summarize("", 3), "");
    }

    #[test]
    fn mock_nth_exceeds_word_count() {
        assert_eq!(mock_summarize("only three words", 10), "");
    }

    #[tokio::test]
    async fn mock_backend_chat_returns_nth_words() {
        let backend = Backend::Mock { nth_word: 3 };
        let result = backend
            .chat("ignored system prompt", "one two three four five six")
            .await
            .unwrap();
        assert_eq!(result, "three six");
    }

    #[tokio::test]
    async fn chat_api_returns_no_choices_error_on_empty_choices() {
        use wiremock::matchers::method;
        use wiremock::{Mock, MockServer, ResponseTemplate};

        let server = MockServer::start().await;
        let body = r#"{"choices": [], "usage": {"prompt_tokens": 200000}}"#;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(200).set_body_string(body))
            .expect(1)
            .mount(&server)
            .await;

        let ep = ApiEndpoint {
            api_url: &server.uri(),
            model: "test",
            auth: ApiAuth::None,
            rate_limits: None,
        };
        let err = chat_api(&ep, "sys", "usr", None).await.unwrap_err();
        assert!(is_no_choices(&err), "expected NoChoicesError, got: {err:#}");
        let nc = err
            .chain()
            .find_map(|c| c.downcast_ref::<NoChoicesError>())
            .expect("downcast NoChoicesError");
        assert!(
            nc.body_snippet.contains("prompt_tokens"),
            "body snippet should preserve response body: {}",
            nc.body_snippet
        );
    }

    #[test]
    fn is_no_choices_walks_anyhow_context_chain() {
        // Simulate what summarize::summarize_email does: wrap the
        // backend error with a context that does not mention "no
        // choices".  The detector must still find the leaf type.
        let leaf = anyhow::Error::new(NoChoicesError {
            body_snippet: "{}".to_string(),
        });
        let wrapped = leaf.context("human summary failed");
        assert!(is_no_choices(&wrapped));
    }

    #[test]
    fn is_no_choices_returns_false_for_unrelated_errors() {
        let err = anyhow::anyhow!("API returned error status");
        assert!(!is_no_choices(&err));
    }

    #[tokio::test]
    async fn mock_no_choices_backend_produces_no_choices_error() {
        let backend = Backend::MockNoChoices;
        let err = backend.chat("sys", "usr").await.unwrap_err();
        assert!(is_no_choices(&err), "expected NoChoicesError, got: {err:#}");
    }
}
