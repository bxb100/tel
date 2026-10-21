//! Action execution for the `run` command.
//!
//! [`execute_action`] dispatches one configured [`Action`] at a time; all
//! hand-off state lives in [`TaskContext`]: the last sent message (for
//! `click`) and one k:v `state` map holding every completed action's
//! [`OutputValue`] under its action id (default: 0-based index). An output's
//! `data` is the action's result; its `mapper` converts it into named keys.
//! Nothing is handed from one action to the next implicitly: string fields of
//! later actions reference the values with `{{ ctx.<id>.<key> }}` templates,
//! parsed and evaluated by the engine in [`crate::ast`].

mod browserless;
mod click;
mod llm;

use crate::ast;
use crate::config::Action;
use crate::miniapp;
use anyhow::{Context, Result, anyhow, bail};
use grammers_client::Client;
use grammers_client::message::{InputMessage, Message};
use grammers_client::session::types::PeerRef;
use grammers_client::tl;
use serde_json::Value;

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

/// Adds one `key` per `mapper` entry holding the value at its `json_path`
/// (relative to the output `data`). A path that does not resolve is an error.
fn apply_mapper(
    data: &mut serde_json::Map<String, serde_json::Value>,
    mapper: &std::collections::BTreeMap<String, String>,
) -> Result<()> {
    let source = data["data"].clone();
    for (key, path) in mapper {
        let path = path
            .strip_prefix("data.")
            .or_else(|| path.strip_prefix("$."))
            .unwrap_or(path);
        let value = dig_json(&source, path)
            .with_context(|| format!("mapper `{key}`: path `{path}` not found in the output"))?;
        data.insert(key.clone(), serde_json::Value::String(value));
    }
    Ok(())
}

/// What one completed action contributes to the task context: a uniform map.
#[derive(Clone, Debug, Default)]
pub(super) struct OutputValue {
    /// The uniform output map. Key `data` holds the primary result; the entry's
    /// `mapper = { key = "json_path" }` pairs are stored alongside it.
    pub data: serde_json::Map<String, serde_json::Value>,
}

/// Returns a copy of `action` with every string field rendered against the
/// state recorded in `context` so far.
pub(super) fn render_action(action: &Action, context: &TaskContext) -> Result<Action> {
    let value = serde_json::to_value(action).context("failed to serialize action for rendering")?;
    let root = Value::Object(context.state.clone());
    let rendered =
        render_json(&value, &root).map_err(|err| anyhow!("template render failed: {err}"))?;
    serde_json::from_value(rendered).context("failed to parse the rendered action")
}

/// Builds the template root `{"ctx": <state>}`.
fn render_json(value: &Value, root: &Value) -> Result<Value> {
    let ctx = {
        let mut map = serde_json::Map::new();
        map.insert("ctx".to_string(), root.clone());
        Value::Object(map)
    };
    walk(value, &ctx)
}

/// Renders every string in `value` against `ctx`, keeping strings as strings.
fn walk(value: &Value, ctx: &Value) -> Result<Value> {
    Ok(match value {
        Value::String(text) => Value::String(ast::render_string(text, ctx)?),
        Value::Array(items) => Value::Array(
            items
                .iter()
                .map(|item| walk(item, ctx))
                .collect::<Result<_>>()?,
        ),
        Value::Object(map) => Value::Object(
            map.iter()
                .map(|(key, item)| Ok((key.clone(), walk(item, ctx)?)))
                .collect::<Result<_>>()?,
        ),
        other => other.clone(),
    })
}

/// Resolves a mapper `json_path` against an output `data` value, or `None`
/// when the path does not resolve.
fn dig_json(json: &Value, path: &str) -> Option<String> {
    let expr = ast::parse_expr(path).ok()?;
    ast::eval(&expr, json).ok().map(ast::stringify_terminal)
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
    use serde_json::json;
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

    fn context_with(entries: &[(&str, Value)]) -> TaskContext {
        let mut context = TaskContext::default();
        for (key, map) in entries {
            context.state.insert(
                (*key).to_string(),
                Value::Object(map.as_object().cloned().unwrap_or_default()),
            );
        }
        context
    }

    fn map_from(pairs: &[(&str, Value)]) -> Value {
        let mut map = serde_json::Map::new();
        for (key, value) in pairs {
            map.insert((*key).to_string(), value.clone());
        }
        Value::Object(map)
    }

    fn render_field(input: &str, context: &TaskContext) -> Result<String> {
        let root = Value::Object(context.state.clone());
        let rendered = render_json(&Value::String(input.to_string()), &root)?;
        rendered
            .as_str()
            .map(str::to_string)
            .context("expected the string field to stay a string after rendering")
    }

    #[test]
    fn passes_plain_text_through() {
        let context = context_with(&[]);
        assert_eq!(
            render_field("hello world", &context).unwrap(),
            "hello world"
        );
    }

    #[test]
    fn renders_data_reference() {
        let context = context_with(&[("0", map_from(&[("data", json!("123456"))]))]);
        assert_eq!(
            render_field("code: {{ctx.0.data}}", &context).unwrap(),
            "code: 123456"
        );
    }

    #[test]
    fn accepts_whitespace_inside_braces() {
        let context = context_with(&[("login", map_from(&[("data", json!("654321"))]))]);
        assert_eq!(
            render_field("code: {{ ctx.login.data }}", &context).unwrap(),
            "code: 654321"
        );
    }

    #[test]
    fn renders_mapper_key() {
        let context = context_with(&[(
            "scrape",
            map_from(&[
                ("data", json!({ "text": { "text": "123456" } })),
                ("code", json!("123456")),
            ]),
        )]);
        assert_eq!(
            render_field("{{ctx.scrape.code}}", &context).unwrap(),
            "123456"
        );
    }

    #[test]
    fn renders_nested_data_path() {
        let context = context_with(&[(
            "0",
            map_from(&[("data", json!({ "goto": { "status": 200 } }))]),
        )]);
        assert_eq!(
            render_field("{{ctx.0.data.goto.status}}", &context).unwrap(),
            "200"
        );
    }

    #[test]
    fn renders_array_subscript() {
        let context = context_with(&[("0", map_from(&[("data", json!({ "items": [1, 2, 3] }))]))]);
        assert_eq!(
            render_field("{{ctx.0.data.items[0]}}", &context).unwrap(),
            "1"
        );
    }

    #[test]
    fn mixed_text_and_expressions() {
        let context = context_with(&[
            ("0", map_from(&[("data", json!("first"))])),
            ("1", map_from(&[("data", json!("second"))])),
        ]);
        assert_eq!(
            render_field("{{ctx.0.data}} and {{ctx.1.data}}", &context).unwrap(),
            "first and second"
        );
    }

    #[test]
    fn whole_field_expression_stringifies_objects() {
        let context = context_with(&[(
            "0",
            map_from(&[("data", json!({ "goto": { "status": 200 } }))]),
        )]);
        assert_eq!(
            render_field("{{ctx.0.data}}", &context).unwrap(),
            r#"{"goto":{"status":200}}"#
        );
    }

    #[test]
    fn unwraps_single_field_object() {
        let context = context_with(&[("0", map_from(&[("data", json!({ "text": "654321" }))]))]);
        assert_eq!(render_field("{{ctx.0.data}}", &context).unwrap(), "654321");
    }

    #[test]
    fn null_value_renders_empty() {
        let context = context_with(&[("0", map_from(&[("data", json!(null))]))]);
        assert_eq!(render_field("[{{ctx.0.data}}]", &context).unwrap(), "[]");
    }

    #[test]
    fn leaves_single_braces_untouched() {
        let context = context_with(&[("0", map_from(&[("data", json!("x"))]))]);
        assert_eq!(
            render_field("mutation { goto { status } }", &context).unwrap(),
            "mutation { goto { status } }"
        );
    }

    #[test]
    fn bare_reference_without_braces_is_literal() {
        let context = context_with(&[("0", map_from(&[("data", json!("x"))]))]);
        assert_eq!(
            render_field("plain ctx.0.data text", &context).unwrap(),
            "plain ctx.0.data text"
        );
    }

    #[test]
    fn unknown_id_fails() {
        let context = context_with(&[("0", map_from(&[("data", json!("x"))]))]);
        let err = render_field("{{ctx.missing.data}}", &context)
            .unwrap_err()
            .to_string();
        assert!(err.contains("cannot resolve"), "got: {err}");
    }

    #[test]
    fn unknown_key_fails() {
        let context = context_with(&[("0", map_from(&[("data", json!("x"))]))]);
        let err = render_field("{{ctx.0.nope}}", &context)
            .unwrap_err()
            .to_string();
        assert!(err.contains("cannot resolve"), "got: {err}");
    }

    #[test]
    fn unclosed_braces_fail() {
        let context = context_with(&[]);
        let err = render_field("hello {{task", &context)
            .unwrap_err()
            .to_string();
        assert!(err.contains("unclosed"), "got: {err}");
    }

    #[test]
    fn empty_expression_fails() {
        let context = context_with(&[]);
        let err = render_field("{{}}", &context).unwrap_err().to_string();
        assert!(err.contains("expected identifier"), "got: {err}");
    }

    #[test]
    fn renders_every_string_in_an_action_value() {
        let action = json!({
            "text": {
                "text": "验证码 {{ctx.scrape.code}}",
                "note": "id 是 {{ctx.scrape.data.id}}"
            }
        });
        let context = context_with(&[(
            "scrape",
            map_from(&[("data", json!({ "id": 7 })), ("code", json!("123456"))]),
        )]);
        let root = Value::Object(context.state.clone());
        let rendered = render_json(&action, &root).unwrap();
        assert_eq!(rendered["text"]["text"].as_str().unwrap(), "验证码 123456");
        assert_eq!(rendered["text"]["note"].as_str().unwrap(), "id 是 7");
    }

    #[test]
    fn dig_json_walks_nested_paths() {
        let json = json!({ "a": { "b": { "c": "deep" } } });
        assert_eq!(dig_json(&json, "a.b.c").unwrap(), "deep");
        assert!(dig_json(&json, "a.missing").is_none());
    }
}
