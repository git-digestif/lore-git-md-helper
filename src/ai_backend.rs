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
const MAX_RETRY_WAIT_SECS: u64 = 300;

/// Exponential backoff: 10s, 20s, 40s, 80s, capped at 120s.
fn retry_backoff_secs(attempt: u32) -> u64 {
    std::cmp::min(10u64 << (attempt - 1), 120)
}

/// Tracks rate limit state reported by the API via `x-ratelimit-*` headers.
pub struct RateLimitState {
    pub remaining_requests: Option<u64>,
    pub remaining_tokens: Option<u64>,
    pub last_updated: Instant,
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

    /// Send a system + user message pair and return the assistant reply.
    pub async fn chat(&self, system: &str, user: &str) -> Result<String> {
        match self {
            Backend::Api {
                api_url,
                model,
                token,
            } => chat_api(api_url, model, token.as_deref(), system, user, None).await,
            Backend::CopilotCli { command, model } => {
                chat_cli(command, model.as_deref(), system, user).await
            }
            Backend::Ollama { api_url, model } => {
                chat_api(api_url, model, None, system, user, None).await
            }
            Backend::GitHubModels {
                model,
                token,
                rate_limits,
            } => {
                chat_api(
                    GITHUB_MODELS_URL,
                    model,
                    Some(token.as_str()),
                    system,
                    user,
                    Some(rate_limits),
                )
                .await
            }
        }
    }
}

/// Shared CLI arguments for backend selection.
///
/// Embed in any clap `Args` struct with `#[command(flatten)]` to get
/// `--copilot-cli`, `--ollama`, `--github-models`, and `--model` flags.
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
    api_url: &str,
    model: &str,
    token: Option<&str>,
    system: &str,
    user: &str,
    rate_limits: Option<&Mutex<RateLimitState>>,
) -> Result<String> {
    // Preemptive rate limit back-off: if we know we are running low on
    // requests and the information is recent (within the last 60 s), sleep
    // briefly to avoid hammering the API.
    if let Some(rl) = rate_limits {
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
    let url = format!("{}/chat/completions", api_url.trim_end_matches('/'));
    let req = ChatRequest {
        model: model.to_string(),
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
    };

    let max_retries: u32 = 5;
    let mut attempt = 0;
    let response = loop {
        let mut builder = client.post(&url).json(&req);
        if let Some(t) = token {
            builder = builder.bearer_auth(t);
        }
        let resp = builder.send().await.context("API request failed")?;

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

        break resp
            .error_for_status()
            .context("API returned error status")?;
    };

    // Update rate limit state from response headers when available.
    if let Some(rl) = rate_limits {
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

    let resp: ChatResponse = response
        .json()
        .await
        .context("failed to parse API response")?;
    resp.choices
        .into_iter()
        .next()
        .map(|c| c.message.content)
        .context("no choices in API response")
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
