use crate::config::{Action, ClickAction, DiceAction, TextAction};
use anyhow::{Context, Result, bail, ensure};
use grammers_client::Client;
use grammers_client::session::types::PeerRef;
use std::env;

use super::{OutputValue, TaskContext, execute_action};

/// Asks the LLM to choose the next action, then runs it through the same executor
/// as configured actions so state hand-off (e.g. `click` reading the previous
/// message) behaves identically.
pub(super) async fn execute_llm_action(
    client: &Client,
    peer: PeerRef,
    prompt: &str,
    context: &mut TaskContext,
) -> Result<OutputValue> {
    let action = request_llm_action(prompt).await?;
    Box::pin(execute_action(client, peer, &action, context)).await
}

async fn request_llm_action(prompt: &str) -> Result<Action> {
    let api_key =
        env::var("OPENAI_API_KEY").context("OPENAI_API_KEY is required for llm action")?;
    let model = env::var("OPENAI_MODEL").context("OPENAI_MODEL is required for llm action")?;
    let base_url =
        env::var("OPENAI_BASE_URL").unwrap_or_else(|_| "https://api.openai.com/v1".to_string());
    let url = format!("{}/chat/completions", base_url.trim_end_matches('/'));

    let body = serde_json::json!({
        "model": model,
        "messages": [
            {
                "role": "system",
                "content": "Choose exactly one Telegram task action. Call task_action with action_type and options. Do not answer with text content."
            },
            {
                "role": "user",
                "content": prompt
            }
        ],
        "tools": [
            {
                "type": "function",
                "function": {
                    "name": "task_action",
                    "description": "Select the next Telegram action to execute.",
                    "parameters": {
                        "type": "object",
                        "properties": {
                            "action_type": {
                                "type": "string",
                                "enum": ["text", "dice", "click"]
                            },
                            "options": {
                                "type": "object",
                                "properties": {
                                    "text": { "type": "string" },
                                    "dice": {
                                        "type": "string",
                                        "enum": ["🎲", "🏀", "🎯", "⚽", "🎳", "🎰"]
                                    },
                                    "key": { "type": "string" }
                                },
                                "additionalProperties": true
                            }
                        },
                        "required": ["action_type", "options"],
                        "additionalProperties": false
                    }
                }
            }
        ],
        "tool_choice": {
            "type": "function",
            "function": { "name": "task_action" }
        }
    });

    let response = reqwest::Client::new()
        .post(url)
        .bearer_auth(api_key)
        .json(&body)
        .send()
        .await
        .context("failed to call OpenAI-compatible chat completions endpoint")?;

    let status = response.status();
    let payload = response
        .text()
        .await
        .context("failed to read llm response body")?;

    ensure!(
        status.is_success(),
        "llm endpoint returned {status}: {payload}"
    );

    let payload = serde_json::from_str::<serde_json::Value>(&payload)
        .context("failed to parse llm response JSON")?;
    parse_llm_action(payload)
}

fn parse_llm_action(payload: serde_json::Value) -> Result<Action> {
    let arguments = payload
        .pointer("/choices/0/message/tool_calls/0/function/arguments")
        .and_then(|value| value.as_str())
        .context("llm response did not include task_action tool arguments")?;
    let arguments = serde_json::from_str::<serde_json::Value>(arguments)
        .context("failed to parse task_action arguments")?;

    let action_type = arguments
        .get("action_type")
        .and_then(|value| value.as_str())
        .context("task_action.action_type is required")?;
    let options = arguments
        .get("options")
        .context("task_action.options is required")?;

    match action_type {
        "text" => options
            .get("text")
            .and_then(|value| value.as_str())
            .map(|text| {
                Action::Text(TextAction {
                    text: text.to_string(),
                })
            })
            .context("task_action.options.text is required for text action"),
        "dice" => options
            .get("dice")
            .and_then(|value| value.as_str())
            .map(|dice| {
                Action::Dice(DiceAction {
                    dice: dice.to_string(),
                })
            })
            .context("task_action.options.dice is required for dice action"),
        "click" => options
            .get("key")
            .and_then(|value| value.as_str())
            .map(|key| {
                Action::Click(ClickAction {
                    key: key.to_string(),
                })
            })
            .context("task_action.options.key is required for click action"),
        other => bail!("unsupported llm action_type: {other}"),
    }
}

#[cfg(test)]
mod tests {
    use super::parse_llm_action;
    use crate::config::Action;

    #[test]
    fn parses_llm_tool_call_text_action() {
        let payload = serde_json::json!({
            "choices": [
                {
                    "message": {
                        "tool_calls": [
                            {
                                "function": {
                                    "arguments": "{\"action_type\":\"text\",\"options\":{\"text\":\"hello\"}}"
                                }
                            }
                        ]
                    }
                }
            ]
        });

        let action = parse_llm_action(payload).expect("tool call should parse");
        match action {
            Action::Text(text) => assert_eq!(text.text, "hello"),
            _ => panic!("expected text action"),
        }
    }
}
