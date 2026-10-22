//! The model behind the assistant: an OpenAI-compatible API, over HTTP(S).

use std::time::Duration;

use matter_peek_core::api::agent::{Http, ModelConfig, Think};

/// How long a model may take to answer.
const MODEL_TIMEOUT: Duration = Duration::from_secs(600);

/// The model `MATTER_PEEK_LLM_URL` and `MATTER_PEEK_LLM_MODEL` name, with the
/// key in `MATTER_PEEK_LLM_KEY` if the API needs one: it overrides the one
/// kept in the store. `None` without both.
pub fn model_from_env() -> Option<ModelConfig> {
    let var = |name: &str| std::env::var(name).ok().filter(|value| !value.is_empty());

    Some(ModelConfig {
        url: var("MATTER_PEEK_LLM_URL")?
            .trim_end_matches('/')
            .to_string(),
        model: var("MATTER_PEEK_LLM_MODEL")?,
        api_key: var("MATTER_PEEK_LLM_KEY"),
        think: Think::Default,
    })
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
        let headers = owned(headers);

        blocking::unblock(move || {
            let mut request = agent.post(&url);
            for (name, value) in &headers {
                request = request.header(name, value);
            }

            answer(request.send(body), &url)
        })
        .await
    }

    async fn get(&self, url: &str, headers: &[(&str, &str)]) -> Result<String, String> {
        let agent = self.0.clone();
        let url = url.to_string();
        let headers = owned(headers);

        blocking::unblock(move || {
            let mut request = agent.get(&url);
            for (name, value) in &headers {
                request = request.header(name, value);
            }

            answer(request.call(), &url)
        })
        .await
    }
}

fn owned(headers: &[(&str, &str)]) -> Vec<(String, String)> {
    headers
        .iter()
        .map(|(name, value)| (name.to_string(), value.to_string()))
        .collect()
}

/// The body of the API's answer, or why not.
fn answer(
    response: Result<ureq::http::Response<ureq::Body>, ureq::Error>,
    url: &str,
) -> Result<String, String> {
    let mut response =
        response.map_err(|e| format!("cannot reach the model's API at {url}: {e}"))?;
    let status = response.status();
    let text = response
        .body_mut()
        .read_to_string()
        .map_err(|e| format!("cannot read the model's answer: {e}"))?;

    if status.is_success() {
        return Ok(text);
    }

    // Most APIs say why in `{"error": {"message": …}}`.
    let error = serde_json::from_str::<serde_json::Value>(&text)
        .ok()
        .and_then(|body| {
            let error = &body["error"];
            error["message"]
                .as_str()
                .or_else(|| error.as_str())
                .map(str::to_string)
        });

    Err(format!(
        "the model's API answered {status}: {}",
        error.as_deref().unwrap_or_else(|| clip(&text, 300))
    ))
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
