//! `{{ctx.<id>.<key>}}` template support for action string parameters,
//! powered by the hand-written AST engine in [`crate::ast`].
//!
//! Every string field of an action (text, click key, llm prompt, browserless
//! query, ...) may reference the output map of a previously completed action
//! inside `{{ ... }}`:
//!
//! - `{{ctx.<id>.data}}` — the uniform primary result every action stores
//! - `{{ctx.<id>.<key>}}` — a mapper-extracted key
//! - `{{ctx.<id>.data.goto.status}}` — a JSON path through `data`
//! - `{{ctx.<id>.data.items[0]}}` — an array subscript
//!
//! The `<id>` is the action's `id` field (default: 0-based index). Text
//! outside `{{ }}` passes through verbatim. A field consisting of a single
//! expression keeps the referenced value (stringified when the field itself is
//! a string, so the action round-trips its typed shape). Unknown paths fail
//! the task with a descriptive error instead of silently sending a
//! half-rendered message.

use super::TaskContext;
use crate::ast;
use anyhow::{Result, anyhow};
use serde_json::Value;

/// Renders every string field of the serialized `action` against the state
/// recorded in `context`.
///
/// This is the production entry point: the whole action value tree goes
/// through [`ast::render_value`], so every string field — including fields
/// added to action configs later — picks up `{{ ... }}` rendering for free.
pub(super) fn render_action_value(action: &Value, context: &TaskContext) -> Result<Value> {
    let ctx = context_value(context);
    let rendered =
        ast::render_value(action, &ctx).map_err(|err| anyhow!("template render failed: {err}"))?;
    Ok(coerce_strings(action, rendered))
}

/// Builds the template context: `{"ctx": <state>}`.
///
/// `ctx` is the engine-visible root, so `{{ctx.<id>.<key>}}` parses and
/// evaluates as one plain path into the context state.
fn context_value(context: &TaskContext) -> Value {
    let mut root = serde_json::Map::new();
    root.insert("ctx".to_string(), Value::Object(context.state.clone()));
    Value::Object(root)
}

/// Walks `(original, rendered)` pairs: positions that were strings in the
/// original stringify their rendered value.
///
/// [`ast::render_value`] preserves a single-expression field's type (a number
/// stays a number), but action config fields are strings — coercion keeps the
/// tree deserializable while whole-field expressions still surface the raw
/// value as text.
fn coerce_strings(original: &Value, rendered: Value) -> Value {
    match original {
        Value::String(_) => Value::String(stringify(&rendered)),
        Value::Array(items) => match rendered {
            Value::Array(values) => Value::Array(
                values
                    .into_iter()
                    .zip(items.iter().chain(std::iter::repeat(&Value::Null)))
                    .map(|(value, original)| coerce_strings(original, value))
                    .collect(),
            ),
            other => other,
        },
        Value::Object(fields) => match rendered {
            Value::Object(values) => Value::Object(
                values
                    .into_iter()
                    .map(|(key, value)| {
                        let original = fields.get(&key).unwrap_or(&Value::Null);
                        (key, coerce_strings(original, value))
                    })
                    .collect(),
            ),
            other => other,
        },
        _ => rendered,
    }
}

/// Renders a value as a string: strings verbatim, null as empty, numbers and
/// booleans via `to_string`, single-field objects unwrapped to their scalar.
fn stringify(value: &Value) -> String {
    match value {
        Value::Null => String::new(),
        Value::String(text) => text.clone(),
        other => stringify_terminal(other),
    }
}

/// Walks a `.`-separated path into a JSON value, stringifying the terminal.
///
/// Used by the entry-level mapper to resolve its `key = "json_path"` entries
/// against an output `data` value.
pub(super) fn dig_json(json: &Value, path: &[&str]) -> Option<String> {
    let mut current = json;
    for segment in path {
        current = current.get(segment)?;
    }
    Some(stringify_terminal(current))
}

/// Renders a JSON terminal: strings verbatim, scalars via `to_string`, and
/// single-field objects unwrapped to their scalar value.
fn stringify_terminal(value: &Value) -> String {
    match value {
        Value::String(text) => text.clone(),
        Value::Object(map) => {
            // A single-field object like `{"text": "123456"}` unwraps to the value.
            if map.len() == 1 {
                let inner = map.values().next().expect("single field");
                if !inner.is_object() && !inner.is_array() {
                    return stringify_terminal(inner);
                }
            }
            value.to_string()
        }
        _ => value.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use anyhow::Context as _;
    use serde_json::json;

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

    /// Renders one string through the production entry point.
    fn render(input: &str, context: &TaskContext) -> Result<String> {
        let rendered = render_action_value(&Value::String(input.to_string()), context)?;
        rendered
            .as_str()
            .map(str::to_string)
            .context("expected the string field to stay a string after rendering")
    }

    #[test]
    fn passes_plain_text_through() {
        let context = context_with(&[]);
        assert_eq!(render("hello world", &context).unwrap(), "hello world");
    }

    #[test]
    fn renders_data_reference() {
        let context = context_with(&[("0", map_from(&[("data", json!("123456"))]))]);
        assert_eq!(
            render("code: {{ctx.0.data}}", &context).unwrap(),
            "code: 123456"
        );
    }

    #[test]
    fn accepts_whitespace_inside_braces() {
        let context = context_with(&[("login", map_from(&[("data", json!("654321"))]))]);
        assert_eq!(
            render("code: {{ ctx.login.data }}", &context).unwrap(),
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
        assert_eq!(render("{{ctx.scrape.code}}", &context).unwrap(), "123456");
    }

    #[test]
    fn renders_nested_data_path() {
        let context = context_with(&[(
            "0",
            map_from(&[("data", json!({ "goto": { "status": 200 } }))]),
        )]);
        assert_eq!(
            render("{{ctx.0.data.goto.status}}", &context).unwrap(),
            "200"
        );
    }

    #[test]
    fn renders_array_subscript() {
        let context = context_with(&[("0", map_from(&[("data", json!({ "items": [1, 2, 3] }))]))]);
        assert_eq!(render("{{ctx.0.data.items[0]}}", &context).unwrap(), "1");
    }

    #[test]
    fn mixed_text_and_expressions() {
        let context = context_with(&[
            ("0", map_from(&[("data", json!("first"))])),
            ("1", map_from(&[("data", json!("second"))])),
        ]);
        assert_eq!(
            render("{{ctx.0.data}} and {{ctx.1.data}}", &context).unwrap(),
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
            render("{{ctx.0.data}}", &context).unwrap(),
            r#"{"goto":{"status":200}}"#
        );
    }

    #[test]
    fn unwraps_single_field_object() {
        let context = context_with(&[("0", map_from(&[("data", json!({ "text": "654321" }))]))]);
        assert_eq!(render("{{ctx.0.data}}", &context).unwrap(), "654321");
    }

    #[test]
    fn null_value_renders_empty() {
        let context = context_with(&[("0", map_from(&[("data", json!(null))]))]);
        assert_eq!(render("[{{ctx.0.data}}]", &context).unwrap(), "[]");
    }

    #[test]
    fn leaves_single_braces_untouched() {
        let context = context_with(&[("0", map_from(&[("data", json!("x"))]))]);
        // GraphQL-style single braces are literal text.
        assert_eq!(
            render("mutation { goto { status } }", &context).unwrap(),
            "mutation { goto { status } }"
        );
    }

    #[test]
    fn bare_reference_without_braces_is_literal() {
        let context = context_with(&[("0", map_from(&[("data", json!("x"))]))]);
        assert_eq!(
            render("plain ctx.0.data text", &context).unwrap(),
            "plain ctx.0.data text"
        );
    }

    #[test]
    fn unknown_id_fails() {
        let context = context_with(&[("0", map_from(&[("data", json!("x"))]))]);
        let err = render("{{ctx.missing.data}}", &context)
            .unwrap_err()
            .to_string();
        assert!(err.contains("cannot resolve"), "got: {err}");
    }

    #[test]
    fn unknown_key_fails() {
        let context = context_with(&[("0", map_from(&[("data", json!("x"))]))]);
        let err = render("{{ctx.0.nope}}", &context).unwrap_err().to_string();
        assert!(err.contains("cannot resolve"), "got: {err}");
    }

    #[test]
    fn unclosed_braces_fail() {
        let context = context_with(&[]);
        let err = render("hello {{task", &context).unwrap_err().to_string();
        assert!(err.contains("unclosed"), "got: {err}");
    }

    #[test]
    fn empty_expression_fails() {
        let context = context_with(&[]);
        let err = render("{{}}", &context).unwrap_err().to_string();
        assert!(err.contains("expected identifier"), "got: {err}");
    }

    #[test]
    fn renders_every_string_in_an_action_value() {
        // The production path renders nested config trees, not just top fields.
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
        let rendered = render_action_value(&action, &context).unwrap();
        assert_eq!(rendered["text"]["text"].as_str().unwrap(), "验证码 123456");
        assert_eq!(rendered["text"]["note"].as_str().unwrap(), "id 是 7");
    }

    #[test]
    fn dig_json_walks_nested_paths() {
        let json = json!({ "a": { "b": { "c": "deep" } } });
        assert_eq!(dig_json(&json, &["a", "b", "c"]).unwrap(), "deep");
        assert!(dig_json(&json, &["a", "missing"]).is_none());
    }

    #[test]
    fn stringify_terminal_unwraps_scalar_objects() {
        let json = json!({ "only": "value" });
        assert_eq!(stringify_terminal(&json), "value");
    }
}
