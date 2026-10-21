//! Action execution for the `run` command.
//!
//! [`execute_action`] dispatches one configured [`Action`] at a time; all
//! hand-off state lives in [`TaskContext`]: the last sent message (for
//! `click`) and one k:v `state` map holding every completed action's
//! [`OutputValue`] under its action id (default: 0-based index). An output's
//! `data` is the action's result; its `mapper` converts it into named keys.
//! Nothing is handed from one action to the next implicitly: string fields of
//! later actions reference the values with `{{ ctx.<id>.<key> }}` templates
//! (see [`template`]).

mod browserless;
mod click;
mod llm;
mod template;

use crate::config::Action;
use crate::miniapp;
use anyhow::{Context, Result, bail};
use grammers_client::Client;
use grammers_client::message::{InputMessage, Message};
use grammers_client::session::types::PeerRef;
use grammers_client::tl;

/// Persistent state shared across all actions in a task.
#[derive(Default)]
pub(super) struct TaskContext {
    /// The most recent message sent by a `text` / `dice` action (read by
    /// `click`). Kept apart from `state` because a message is not a k:v value.
    pub last_message: Option<Message>,
    /// Every completed action's output map keyed by action id. This is also
    /// the `ctx` root the templates resolve against.
    state: serde_json::Map<String, serde_json::Value>,
}

impl TaskContext {
    /// Records `output` under `id`, after applying the entry's `mapper`.
    ///
    /// The `mapper` only describes how the output is converted into k:v pairs
    /// for storage: each `key = "json_path"` entry stores the value at that
    /// path (relative to the output `data` value) under `key`. It works the
    /// same for every action.
    pub(super) fn record_output(
        &mut self,
        id: &str,
        mapper: &std::collections::BTreeMap<String, String>,
        mut output: OutputValue,
    ) -> Result<()> {
        if self.state.contains_key(id) {
            bail!("duplicate action id `{id}`: an output is already stored under this key");
        }
        apply_mapper(&mut output.data, mapper)
            .with_context(|| format!("failed to map the output of action `{id}`"))?;

        self.state
            .insert(id.to_string(), serde_json::Value::Object(output.data));
        Ok(())
    }
}

/// Converts the output into extra k:v pairs per `mapper`: `key = "json_path"`
/// stores the value at that path (relative to the output `data` value) as `key`.
/// A path that does not resolve is an error.
fn apply_mapper(
    data: &mut serde_json::Map<String, serde_json::Value>,
    mapper: &std::collections::BTreeMap<String, String>,
) -> Result<()> {
    let source = data["data"].clone();
    for (key, path) in mapper {
        // Strip an optional leading `data.` for uniformity with template paths.
        let path = path
            .strip_prefix("data.")
            .or_else(|| path.strip_prefix("$."))
            .unwrap_or(path);
        let value = template::dig_json(&source, path)
            .with_context(|| format!("mapper `{key}`: path `{path}` not found in the output"))?;
        data.insert(key.clone(), serde_json::Value::String(value));
    }
    Ok(())
}

/// What one completed action contributes to the task context: a uniform map.
///
/// Every action stores its result as `data` (the unified output shape). The
/// entry's `mapper` converts it into further named k:v pairs. Later actions reference
/// these values with `ctx.<id>.<key>` templates.
#[derive(Clone, Debug, Default)]
pub(super) struct OutputValue {
    /// The uniform output map. Key `data` holds the primary result; the entry's
    /// `mapper = { key = "json_path" }` pairs are stored alongside it.
    pub data: serde_json::Map<String, serde_json::Value>,
}

/// Returns a copy of `action` with every string field resolved against the
/// state recorded in `context` so far (`{{ctx.<id>.<key>}}` templates).
///
/// The whole action value tree goes through the template engine, so every
/// string field — including fields added later — renders for free.
pub(super) fn render_action(action: &Action, context: &TaskContext) -> Result<Action> {
    let value = serde_json::to_value(action).context("failed to serialize action for rendering")?;
    let rendered = template::render_action_value(&value, context)?;
    serde_json::from_value(rendered).context("failed to parse the rendered action")
}

pub(super) async fn execute_action(
    client: &Client,
    peer: PeerRef,
    action: &Action,
    context: &mut TaskContext,
) -> Result<OutputValue> {
    match action {
        Action::Text(text) => {
            let message = client.send_message(peer, text.text.as_str()).await?;
            let output = OutputValue {
                data: output_data(message.text()),
            };
            context.last_message = Some(message);
            Ok(output)
        }
        Action::Dice(dice) => {
            let message = InputMessage::new().media(tl::types::InputMediaDice {
                emoticon: dice.dice.clone(),
            });
            let message = client.send_message(peer, message).await?;
            let output = OutputValue {
                data: output_data(dice.dice.as_str()),
            };
            context.last_message = Some(message);
            Ok(output)
        }
        Action::Click(click) => {
            let previous_message = context.last_message.clone();
            let outcome =
                click::click_inline_keyboard(client, peer, &click.key, previous_message.as_ref())
                    .await?;
            // `data` is the callback answer text or the opened URL button link.
            let value = match outcome {
                click::ClickOutcome::Answer(answer) => answer.unwrap_or_default(),
                click::ClickOutcome::Url(url) => url,
            };
            Ok(OutputValue {
                data: output_data(value),
            })
        }
        Action::Llm(llm) => llm::execute_llm_action(client, peer, &llm.prompt, context).await,
        Action::Browserless(browserless) => {
            let mut variables = browserless.variables.clone();
            for value in variables.values_mut() {
                *value = miniapp::resolve_url_if_miniapp(client, value).await?;
            }
            browserless::execute_browserless_action(browserless, &variables).await
        }
    }
}

/// Builds the uniform `{ "data": <value> }` output map.
fn output_data(value: impl Into<serde_json::Value>) -> serde_json::Map<String, serde_json::Value> {
    let mut map = serde_json::Map::new();
    map.insert("data".to_string(), value.into());
    map
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::BrowserlessAction;
    use std::collections::BTreeMap;

    fn browserless(url: Option<&str>, variables: BTreeMap<String, String>) -> Action {
        Action::Browserless(BrowserlessAction {
            token: String::new(),
            query: String::new(),
            operation_name: String::new(),
            url: url.map(str::to_string),
            variables,
        })
    }

    fn output(value: impl Into<serde_json::Value>) -> OutputValue {
        OutputValue {
            data: output_data(value),
        }
    }

    fn render_browserless(action: Action, context: &TaskContext) -> BrowserlessAction {
        let Action::Browserless(rendered) = render_action(&action, context).unwrap() else {
            unreachable!()
        };
        rendered
    }

    #[test]
    fn record_output_stores_mapper_pairs_under_the_action_id() {
        let mut context = TaskContext::default();
        let mapper = BTreeMap::from([("code".to_string(), "a.b".to_string())]);

        context
            .record_output(
                "scrape",
                &mapper,
                output(serde_json::json!({ "a": { "b": "deep" } })),
            )
            .unwrap();

        assert_eq!(context.state["scrape"]["code"], "deep");
    }

    #[test]
    fn record_output_fails_when_a_mapper_path_is_missing() {
        let mut context = TaskContext::default();
        let mapper = BTreeMap::from([("code".to_string(), "a.missing".to_string())]);

        let result =
            context.record_output("scrape", &mapper, output(serde_json::json!({ "a": {} })));

        assert!(result.is_err());
    }

    #[test]
    fn record_output_rejects_duplicate_ids() {
        let mut context = TaskContext::default();
        let mapper = BTreeMap::new();

        context.record_output("0", &mapper, output("x")).unwrap();
        assert!(context.record_output("0", &mapper, output("y")).is_err());
    }

    #[test]
    fn browserless_url_template_reuses_a_mapped_reconnect_endpoint() {
        let mut context = TaskContext::default();
        let mapper = BTreeMap::from([(
            "url".to_string(),
            "data.reconnect.browserQLEndpoint".to_string(),
        )]);
        let data =
            serde_json::json!({ "reconnect": { "browserQLEndpoint": "https://bql.example/s1" } });
        context
            .record_output("page", &mapper, output(data))
            .unwrap();

        let rendered = render_browserless(
            browserless(Some("{{ ctx.page.url }}"), BTreeMap::new()),
            &context,
        );

        assert_eq!(rendered.url.as_deref(), Some("https://bql.example/s1"));
    }

    #[test]
    fn browserless_variables_are_rendered_from_the_context() {
        let mut context = TaskContext::default();
        context
            .record_output("open", &BTreeMap::new(), output("https://one.example"))
            .unwrap();
        let variables = BTreeMap::from([("url".to_string(), "{{ ctx.open.data }}".to_string())]);

        let rendered = render_browserless(browserless(None, variables), &context);

        assert_eq!(rendered.variables["url"], "https://one.example");
    }
}
