//! `{{ctx.<id>.<key>}}` template support for action string parameters.
//!
//! This module is only the thin adapter between the run command's
//! [`TaskContext`] and the template engine in [`crate::ast`]: it builds the
//! `{"ctx": <state>}` root and keeps action config fields as strings. Parsing,
//! path resolution, error messages and recursive rendering all live in
//! [`crate::ast`].
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
/// Every string of the tree is rendered by [`ast::render_string`], so rendered
/// fields stay strings — action config fields are strings, and a whole-field
/// expression still surfaces the raw value as text.
pub(super) fn render_action_value(action: &Value, context: &TaskContext) -> Result<Value> {
    let mut root = serde_json::Map::new();
    root.insert("ctx".to_string(), Value::Object(context.state.clone()));
    render(action, &Value::Object(root)).map_err(|err| anyhow!("template render failed: {err}"))
}

/// Walks the action tree, rendering every string with [`ast::render_string`]
/// so the field keeps its string type.
fn render(value: &Value, ctx: &Value) -> Result<Value> {
    Ok(match value {
        Value::String(text) => Value::String(ast::render_string(text, ctx)?),
        Value::Array(items) => Value::Array(
            items
                .iter()
                .map(|item| render(item, ctx))
                .collect::<Result<_>>()?,
        ),
        Value::Object(map) => Value::Object(
            map.iter()
                .map(|(key, item)| Ok((key.clone(), render(item, ctx)?)))
                .collect::<Result<_>>()?,
        ),
        other => other.clone(),
    })
}

/// Resolves a mapper `json_path` against an output `data` value, or `None`
/// when the path does not resolve.
///
/// The path is read with the same grammar as a `{{ ... }}` expression, so
/// mapper paths and template paths behave identically.
pub(super) fn dig_json(json: &Value, path: &str) -> Option<String> {
    let expr = ast::parse_expr(path).ok()?;
    ast::eval(&expr, json).ok().map(ast::stringify_terminal)
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
    fn render_field(input: &str, context: &TaskContext) -> Result<String> {
        let rendered = render_action_value(&Value::String(input.to_string()), context)?;
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
        // GraphQL-style single braces are literal text.
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
        assert_eq!(dig_json(&json, "a.b.c").unwrap(), "deep");
        assert!(dig_json(&json, "a.missing").is_none());
    }
}
