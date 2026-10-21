//! The OpenAI Chat Completions API, which most LLM providers and local model
//! servers speak: OpenAI, OpenRouter, Groq, Mistral, Gemini's compatible
//! endpoint, Ollama, LM Studio, vLLM, …

use alloc::format;
use alloc::string::{String, ToString};
use alloc::vec::Vec;

use serde_json::{json, Value};

use super::{Http, Message, Provider, Reply, Think, ToolCall, ToolSpec, Usage};

/// A model behind an OpenAI-compatible API.
pub struct OpenAi<H> {
    http: H,
    url: String,
    model: String,
    api_key: Option<String>,
}

impl<H: Http> OpenAi<H> {
    /// `model` at `base_url`, as the provider documents it (e.g.
    /// `http://localhost:11434/v1`), with `api_key` if it needs one.
    pub fn new(http: H, base_url: &str, model: &str, api_key: Option<String>) -> Self {
        Self {
            http,
            url: format!("{}/chat/completions", base_url.trim_end_matches('/')),
            model: model.to_string(),
            api_key,
        }
    }

    fn body(&self, messages: &[Message], tools: &[ToolSpec], think: Think) -> Value {
        let mut body = json!({
            "model": self.model,
            "messages": messages.iter().map(message).collect::<Vec<_>>(),
            "tools": tools.iter().map(tool).collect::<Vec<_>>(),
        });
        if think == Think::Off {
            body["reasoning_effort"] = json!("none");
        }

        body
    }
}

impl<H: Http> Provider for OpenAi<H> {
    async fn complete(
        &self,
        messages: &[Message],
        tools: &[ToolSpec],
        think: Think,
    ) -> Result<Reply, String> {
        let body = self.body(messages, tools, think).to_string();

        let authorization = self.api_key.as_ref().map(|key| format!("Bearer {key}"));
        let mut headers = alloc::vec![("Content-Type", "application/json")];
        if let Some(authorization) = &authorization {
            headers.push(("Authorization", authorization));
        }

        let response = self.http.post(&self.url, &headers, body).await?;
        reply(&response)
    }
}

fn message(message: &Message) -> Value {
    match message {
        Message::System(text) => json!({ "role": "system", "content": text }),
        Message::User(text) => json!({ "role": "user", "content": text }),
        Message::Assistant { text, calls } if calls.is_empty() => {
            json!({ "role": "assistant", "content": text })
        }
        Message::Assistant { text, calls } => json!({
            "role": "assistant",
            "content": text,
            "tool_calls": calls.iter().map(|call| json!({
                "id": call.id,
                "type": "function",
                "function": { "name": call.name, "arguments": call.arguments },
            })).collect::<Vec<_>>(),
        }),
        Message::Tool { call_id, result } => {
            json!({ "role": "tool", "tool_call_id": call_id, "content": result })
        }
    }
}

fn tool(tool: &ToolSpec) -> Value {
    json!({
        "type": "function",
        "function": {
            "name": tool.name,
            "description": tool.description,
            "parameters": tool.parameters,
        },
    })
}

/// The reply in a Chat Completions response.
fn reply(response: &str) -> Result<Reply, String> {
    let response: Value = serde_json::from_str(response)
        .map_err(|e| format!("the model's answer is not JSON: {e}"))?;

    if let Some(error) = response.get("error") {
        let message = error["message"].as_str().map(str::to_string);
        return Err(format!(
            "the model's API refused: {}",
            message.unwrap_or_else(|| error.to_string())
        ));
    }

    let message = &response["choices"][0]["message"];
    if message.is_null() {
        return Err("the model's answer has no message".into());
    }

    let text = |value: &Value| value.as_str().unwrap_or_default().trim().to_string();
    let thinking = match &message["reasoning"] {
        Value::Null => text(&message["reasoning_content"]),
        reasoning => text(reasoning),
    };

    let calls = message["tool_calls"]
        .as_array()
        .into_iter()
        .flatten()
        .enumerate()
        .map(|(index, call)| ToolCall {
            id: call["id"]
                .as_str()
                .map(str::to_string)
                .unwrap_or_else(|| format!("call_{index}")),
            name: text(&call["function"]["name"]),
            // A string, as the API defines; some send the object itself.
            arguments: match &call["function"]["arguments"] {
                Value::String(arguments) => arguments.clone(),
                Value::Null => "{}".into(),
                arguments => arguments.to_string(),
            },
        })
        .collect();

    let tokens = |key: &str| response["usage"][key].as_u64().unwrap_or(0) as u32;

    Ok(Reply {
        text: text(&message["content"]),
        thinking,
        calls,
        usage: Usage {
            input_tokens: tokens("prompt_tokens"),
            output_tokens: tokens("completion_tokens"),
        },
    })
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn reply_with_tool_calls_and_thinking() {
        // As Ollama answers.
        let reply = reply(
            r#"{"choices":[{"message":{"role":"assistant","content":"","reasoning":"Look first.",
                "tool_calls":[{"id":"call_ryf3","index":0,"type":"function",
                "function":{"name":"read_network","arguments":"{\"node_id\":112251}"}}]},
                "finish_reason":"tool_calls"}],"usage":{"prompt_tokens":383,"completion_tokens":170}}"#,
        )
        .unwrap();

        assert_eq!(reply.text, "");
        assert_eq!(reply.thinking, "Look first.");
        assert_eq!(
            reply.calls,
            [ToolCall {
                id: "call_ryf3".into(),
                name: "read_network".into(),
                arguments: r#"{"node_id":112251}"#.into(),
            }]
        );
        assert_eq!(
            reply.usage,
            Usage {
                input_tokens: 383,
                output_tokens: 170
            }
        );
    }

    #[test]
    fn reply_with_object_arguments_and_no_ids() {
        let reply = reply(
            r#"{"choices":[{"message":{"content":"Done.","tool_calls":[
                {"function":{"name":"list_devices","arguments":{}}}]}}]}"#,
        )
        .unwrap();

        assert_eq!(reply.text, "Done.");
        assert_eq!(reply.calls[0].id, "call_0");
        assert_eq!(reply.calls[0].arguments, "{}");
    }

    #[test]
    fn api_errors() {
        let error = reply(r#"{"error":{"message":"model not found"}}"#).unwrap_err();
        assert!(error.contains("model not found"), "{error}");

        assert!(reply("<html>").is_err());
        assert!(reply(r#"{"choices":[]}"#).is_err());
    }

    #[test]
    fn messages_and_tools_as_the_api_wants_them() {
        let call = ToolCall {
            id: "c1".into(),
            name: "peek_device".into(),
            arguments: r#"{"node_id":1}"#.into(),
        };

        assert_eq!(
            message(&Message::Assistant {
                text: String::new(),
                calls: alloc::vec![call],
            }),
            json!({
                "role": "assistant",
                "content": "",
                "tool_calls": [{
                    "id": "c1",
                    "type": "function",
                    "function": { "name": "peek_device", "arguments": "{\"node_id\":1}" },
                }],
            })
        );
        assert_eq!(
            message(&Message::Tool {
                call_id: "c1".into(),
                result: "ok".into(),
            }),
            json!({ "role": "tool", "tool_call_id": "c1", "content": "ok" })
        );
        assert_eq!(
            tool(&ToolSpec {
                name: "list_devices".into(),
                description: "List them.".into(),
                parameters: json!({ "type": "object" }),
                read_only: true,
            }),
            json!({
                "type": "function",
                "function": {
                    "name": "list_devices",
                    "description": "List them.",
                    "parameters": { "type": "object" },
                },
            })
        );
    }
}
