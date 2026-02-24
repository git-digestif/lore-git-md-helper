//! Pluggable AI backend for chat-style interactions.
//!
//! Provides a `Backend` whose `chat(system, user)` method sends a
//! message pair to a language model and returns the assistant reply.

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

const DEFAULT_API_URL: &str = "https://models.inference.ai.azure.com";
const DEFAULT_MODEL: &str = "gpt-4o";

/// An AI chat backend.
pub enum Backend {
    /// OpenAI-compatible chat completions API.
    Api {
        api_url: String,
        model: String,
        token: Option<String>,
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

    /// Send a system + user message pair and return the assistant reply.
    pub async fn chat(&self, system: &str, user: &str) -> Result<String> {
        match self {
            Backend::Api {
                api_url,
                model,
                token,
            } => chat_api(api_url, model, token.as_deref(), system, user).await,
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
) -> Result<String> {
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
    let mut builder = client.post(&url).json(&req);
    if let Some(t) = token {
        builder = builder.bearer_auth(t);
    }
    let resp: ChatResponse = builder
        .send()
        .await
        .context("API request failed")?
        .error_for_status()
        .context("API returned error status")?
        .json()
        .await
        .context("failed to parse API response")?;
    resp.choices
        .into_iter()
        .next()
        .map(|c| c.message.content)
        .context("no choices in API response")
}
