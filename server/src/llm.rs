//! The model behind the assistant: an OpenAI-compatible API, over HTTP(S).

use std::time::Duration;

use matter_peek_core::api::agent::{Http, Think};

/// How long a model may take to answer.
const MODEL_TIMEOUT: Duration = Duration::from_secs(600);

/// A model, and its API.
#[derive(Clone, Debug)]
pub struct LlmConfig {
    /// The API, e.g. `http://localhost:11434/v1`.
    pub url: String,
    /// The model, as the API names it.
    pub model: String,
    pub api_key: Option<String>,
    pub think: Think,
}

impl LlmConfig {
    /// The model `MATTER_PEEK_LLM_URL` and `MATTER_PEEK_LLM_MODEL` name, with
    /// the key in `MATTER_PEEK_LLM_KEY` if the API needs one; `None` without
    /// both.
    pub fn from_env() -> Option<Self> {
        let var = |name: &str| std::env::var(name).ok().filter(|value| !value.is_empty());

        Some(Self {
            url: var("MATTER_PEEK_LLM_URL")?,
            model: var("MATTER_PEEK_LLM_MODEL")?,
            api_key: var("MATTER_PEEK_LLM_KEY"),
            think: Think::Default,
        })
    }
}

/// The model's API over HTTP(S): blocking calls, on the thread pool.
#[derive(Clone)]
pub struct Ureq(ureq::Agent);

impl Ureq {
    pub fn new() -> Self {
        let config = ureq::Agent::config_builder()
            // The body of an error status says why.
            .http_status_as_error(false)
            .timeout_global(Some(MODEL_TIMEOUT))
            .build();

        Self(config.into())
    }
}

impl Default for Ureq {
    fn default() -> Self {
        Self::new()
    }
}

impl Http for Ureq {
    async fn post(
        &self,
        url: &str,
        headers: &[(&str, &str)],
        body: String,
    ) -> Result<String, String> {
        let agent = self.0.clone();
        let url = url.to_string();
        let headers: Vec<(String, String)> = headers
            .iter()
            .map(|(name, value)| (name.to_string(), value.to_string()))
            .collect();

        blocking::unblock(move || {
            let mut request = agent.post(&url);
            for (name, value) in &headers {
                request = request.header(name, value);
            }

            let mut response = request
                .send(body)
                .map_err(|e| format!("cannot reach the model at {url}: {e}"))?;
            let status = response.status();
            let text = response
                .body_mut()
                .read_to_string()
                .map_err(|e| format!("cannot read the model's answer: {e}"))?;

            if status.is_success() {
                Ok(text)
            } else {
                Err(format!(
                    "the model's API answered {status}: {}",
                    clip(&text, 300)
                ))
            }
        })
        .await
    }
}

/// At most `max` bytes of `text`, cut at a character.
pub(crate) fn clip(text: &str, max: usize) -> &str {
    if text.len() <= max {
        return text;
    }

    let mut end = max;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    &text[..end]
}
