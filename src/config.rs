use serde::de::{Error as DeError, MapAccess, Visitor};
use serde::{Deserialize, Deserializer, Serialize};
use std::fmt::Debug;

#[derive(Debug, Serialize, Deserialize, Clone)]
pub struct AppConfig {
    #[serde(default)]
    pub task: Vec<Task>,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
#[serde(deny_unknown_fields)]
pub struct Task {
    pub name: String,
    pub chat_id: String,
    pub chat_name: Option<String>,
    pub cron: Option<String>,
    pub delay: Option<u32>,
    #[serde(default)]
    pub actions: Vec<ActionEntry>,
}

/// One entry in a task's `actions` list: an action body plus optional metadata.
///
/// Parses inline tables like `{ id = "login", mapper = { code = "data" }, text = { text = "/start" } }`.
/// `id` names the action for `ctx.<id>.<key>` references and defaults to its
/// 0-based index; `mapper` turns the output into `key = "json_path"` pairs.
#[derive(Debug, Serialize, Clone)]
pub struct ActionEntry {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    /// k:v pairs extracted from the output as `key = "json_path"`.
    #[serde(default, skip_serializing_if = "std::collections::BTreeMap::is_empty")]
    pub mapper: std::collections::BTreeMap<String, String>,
    #[serde(flatten)]
    pub action: Action,
}

impl From<Action> for ActionEntry {
    fn from(action: Action) -> Self {
        Self {
            id: None,
            mapper: std::collections::BTreeMap::new(),
            action,
        }
    }
}

impl<'de> Deserialize<'de> for ActionEntry {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        const BODY_KEYS: [&str; 5] = ["text", "dice", "click", "llm", "browserless"];

        struct ActionEntryVisitor;

        impl<'de> Visitor<'de> for ActionEntryVisitor {
            type Value = ActionEntry;

            fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                formatter.write_str(
                    "an action table such as { id = \"login\", mapper = { code = \"data\" }, text = { text = \"/start\" } }",
                )
            }

            fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<ActionEntry, A::Error> {
                let mut id: Option<String> = None;
                let mut mapper: Option<std::collections::BTreeMap<String, String>> = None;
                let mut body: Option<(String, serde_json::Value)> = None;

                while let Some((key, value)) = map.next_entry::<String, serde_json::Value>()? {
                    match key.as_str() {
                        "id" => id = Some(flexible_string(value, "id")?),
                        "mapper" => {
                            mapper = Some(serde_json::from_value(value).map_err(|err| {
                                A::Error::custom(format!(
                                    "mapper must be a table of `key = \"json_path\"` entries: {err}"
                                ))
                            })?);
                        }
                        field if BODY_KEYS.contains(&field) => {
                            if let Some((existing, _)) = &body {
                                return Err(A::Error::custom(format!(
                                    "action table has multiple bodies (`{field}` after `{existing}`)"
                                )));
                            }
                            body = Some((field.to_string(), value));
                        }
                        other => {
                            return Err(A::Error::custom(format!(
                                "unknown action field `{other}`, expected one of `id`, `mapper`, or an action body ({})",
                                BODY_KEYS.join(", ")
                            )));
                        }
                    }
                }

                let (name, value) = body.ok_or_else(|| {
                    A::Error::custom(format!(
                        "action table is missing a body ({})",
                        BODY_KEYS.join(", ")
                    ))
                })?;
                let mut wrapper = serde_json::Map::new();
                wrapper.insert(name, value);
                let action = serde_json::from_value::<Action>(serde_json::Value::Object(wrapper))
                    .map_err(A::Error::custom)?;

                Ok(ActionEntry {
                    id,
                    mapper: mapper.unwrap_or_default(),
                    action,
                })
            }
        }

        deserializer.deserialize_map(ActionEntryVisitor)
    }
}

/// Accepts a string or an integer (`id = 0` and `id = "login"` both parse).
fn flexible_string<DE: DeError>(value: serde_json::Value, field: &str) -> Result<String, DE> {
    match value {
        serde_json::Value::String(text) => Ok(text),
        serde_json::Value::Number(number) => Ok(number.to_string()),
        other => Err(DE::custom(format!(
            "action field `{field}` must be a string or an integer, got: {other}"
        ))),
    }
}

/// A single task action, serialized as an externally tagged TOML inline table
/// such as `{ text = { text = "" } }`.
#[derive(Debug, Serialize, Deserialize, Clone)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum Action {
    Text(TextAction),
    Dice(DiceAction),
    Click(ClickAction),
    Llm(LlmAction),
    Browserless(BrowserlessAction),
}

#[derive(Debug, Serialize, Deserialize, Clone)]
#[serde(deny_unknown_fields)]
pub struct TextAction {
    pub text: String,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
#[serde(deny_unknown_fields)]
pub struct DiceAction {
    pub dice: String,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
#[serde(deny_unknown_fields)]
pub struct ClickAction {
    pub key: String,
}

#[derive(Debug, Serialize, Deserialize, Clone)]
#[serde(deny_unknown_fields)]
pub struct LlmAction {
    pub prompt: String,
}

/// Browserless browser action executed through a Browserless BQL (GraphQL) endpoint.
#[derive(Debug, Serialize, Deserialize, Clone)]
#[serde(deny_unknown_fields)]
pub struct BrowserlessAction {
    /// Browserless token appended to the request URL.
    pub token: String,
    /// BQL (GraphQL) query/mutation text.
    pub query: String,
    /// Optional GraphQL operation name.
    pub operation_name: String,
    /// Endpoint; defaults to the production-sfo stealth BQL URL.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    /// GraphQL variables, e.g. `{ url = "{{ ctx.open.data }}" }`.
    #[serde(default, skip_serializing_if = "std::collections::BTreeMap::is_empty")]
    pub variables: std::collections::BTreeMap<String, String>,
}

pub fn to_inline_toml(config: &AppConfig) -> String {
    let mut output = String::new();

    for task in &config.task {
        output.push_str("[[task]]\n");
        push_string_field(&mut output, "name", &task.name);
        push_string_field(&mut output, "chat_id", &task.chat_id);
        if let Some(chat_name) = task.chat_name.as_deref() {
            push_string_field(&mut output, "chat_name", chat_name);
        }
        if let Some(cron) = task.cron.as_deref() {
            push_string_field(&mut output, "cron", cron);
        }
        if let Some(delay) = task.delay {
            output.push_str(&format!("delay = {delay}\n"));
        }

        output.push_str("actions = [\n");
        for entry in &task.actions {
            output.push_str("  { ");
            if let Some(id) = &entry.id {
                output.push_str("id = ");
                output.push_str(&toml_string(id));
                output.push_str(", ");
            }
            if !entry.mapper.is_empty() {
                output.push_str("mapper = ");
                push_inline_table(&mut output, &entry.mapper);
                output.push_str(", ");
            }
            push_inline_action(&mut output, &entry.action);
            output.push_str(" },\n");
        }
        output.push_str("]\n\n");
    }

    output
}

/// Writes `{ key = "value", ... }` for a string map.
fn push_inline_table(output: &mut String, table: &std::collections::BTreeMap<String, String>) {
    let items: Vec<String> = table
        .iter()
        .map(|(key, value)| {
            let key = if is_bare_toml_key(key) {
                key.clone()
            } else {
                toml_string(key)
            };
            format!("{key} = {}", toml_string(value))
        })
        .collect();
    output.push_str("{ ");
    output.push_str(&items.join(", "));
    output.push_str(" }");
}

fn push_string_field(output: &mut String, key: &str, value: &str) {
    output.push_str(key);
    output.push_str(" = ");
    output.push_str(&toml_string(value));
    output.push('\n');
}

fn push_inline_action(output: &mut String, action: &Action) {
    match action {
        Action::Text(text) => {
            output.push_str("text = { text = ");
            output.push_str(&toml_string(&text.text));
            output.push_str(" }");
        }
        Action::Dice(dice) => {
            output.push_str("dice = { dice = ");
            output.push_str(&toml_string(&dice.dice));
            output.push_str(" }");
        }
        Action::Click(click) => {
            output.push_str("click = { key = ");
            output.push_str(&toml_string(&click.key));
            output.push_str(" }");
        }
        Action::Llm(llm) => {
            output.push_str("llm = { prompt = ");
            output.push_str(&toml_string(&llm.prompt));
            output.push_str(" }");
        }
        Action::Browserless(browserless) => {
            output.push_str("browserless = { token = ");
            output.push_str(&toml_string(&browserless.token));
            output.push_str(", query = ");
            output.push_str(&toml_string(&browserless.query));
            output.push_str(", operation_name = ");
            output.push_str(&toml_string(browserless.operation_name.as_ref()));

            if let Some(url) = browserless.url.as_ref() {
                output.push_str(", url = ");
                output.push_str(&toml_string(url));
            }
            if !browserless.variables.is_empty() {
                output.push_str(", variables = ");
                push_inline_table(output, &browserless.variables);
            }
            output.push_str(" }");
        }
    }
}

/// Whether `key` can appear unquoted in TOML (bare key).
fn is_bare_toml_key(key: &str) -> bool {
    !key.is_empty()
        && key
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
}

fn toml_string(value: &str) -> String {
    let mut output = String::with_capacity(value.len() + 2);
    output.push('"');
    for ch in value.chars() {
        match ch {
            '\u{08}' => output.push_str("\\b"),
            '\t' => output.push_str("\\t"),
            '\n' => output.push_str("\\n"),
            '\u{0c}' => output.push_str("\\f"),
            '\r' => output.push_str("\\r"),
            '"' => output.push_str("\\\""),
            '\\' => output.push_str("\\\\"),
            '\u{00}'..='\u{1f}' | '\u{7f}' => {
                output.push_str(&format!("\\u{:04X}", ch as u32));
            }
            _ => output.push(ch),
        }
    }
    output.push('"');
    output
}

#[cfg(test)]
mod tests {
    use super::{Action, AppConfig, to_inline_toml};

    #[test]
    fn parses_inline_actions_with_task_schedule() {
        let config = toml::from_str::<AppConfig>(
            r#"
            [[task]]
            name = "task_name"
            chat_id = "123"
            chat_name = "@example"
            cron = "0 0 9 * * *"
            delay = 30
            actions = [
              { text = { text = "/start" } },
              { dice = { dice = "🎲" } },
              { click = { key = "OK" } },
              { llm = { prompt = "pick one" } },
            ]
            "#,
        )
        .expect("inline task config should parse");

        let task = &config.task[0];
        assert_eq!(task.cron.as_deref(), Some("0 0 9 * * *"));
        assert_eq!(task.delay, Some(30));
        assert_eq!(task.actions.len(), 4);
        assert!(matches!(&task.actions[0].action, Action::Text(text) if text.text == "/start"));
    }

    #[test]
    fn parses_action_id_fields() {
        let config = toml::from_str::<AppConfig>(
            r#"
            [[task]]
            name = "task_name"
            chat_id = "123"
            actions = [
              { id = "open", click = { key = "📱" } },
              { id = 2, text = { text = "hello" } },
              { text = { text = "ctx.open.data" } },
            ]
            "#,
        )
        .expect("action id config should parse");

        assert_eq!(config.task[0].actions[0].id.as_deref(), Some("open"));
        assert_eq!(config.task[0].actions[1].id.as_deref(), Some("2"));
        assert_eq!(config.task[0].actions[2].id, None);

        let output = to_inline_toml(&config);
        assert!(
            output.contains("{ id = \"open\", click = { key = \"📱\" } }"),
            "output was: {output}"
        );
        assert!(
            output.contains("{ id = \"2\", text = { text = \"hello\" } }"),
            "output was: {output}"
        );

        let reparsed = toml::from_str::<AppConfig>(&output).expect("roundtrip should parse");
        assert_eq!(reparsed.task[0].actions[0].id.as_deref(), Some("open"));
        assert_eq!(reparsed.task[0].actions[1].id.as_deref(), Some("2"));
    }

    #[test]
    fn parses_and_serializes_entry_mapper_for_any_action() {
        let config = toml::from_str::<AppConfig>(
            r#"
            [[task]]
            name = "task_name"
            chat_id = "123"
            actions = [
              { id = "scrape", mapper = { code = "data.text.text", status = "data.goto.status" }, browserless = { token = "tok", query = "mutation S { text(selector: \".code\") { text } }", operation_name = "S" } },
              { id = "reply", mapper = { msg = "data" }, text = { text = "ok" } },
            ]
            "#,
        )
        .expect("entry mapper config should parse");

        let mut expected_scrape = std::collections::BTreeMap::new();
        expected_scrape.insert("code".to_string(), "data.text.text".to_string());
        expected_scrape.insert("status".to_string(), "data.goto.status".to_string());
        assert_eq!(config.task[0].actions[0].mapper, expected_scrape);

        let mut expected_reply = std::collections::BTreeMap::new();
        expected_reply.insert("msg".to_string(), "data".to_string());
        assert_eq!(config.task[0].actions[1].mapper, expected_reply);

        let output = to_inline_toml(&config);
        assert!(
            output
                .contains("mapper = { code = \"data.text.text\", status = \"data.goto.status\" }"),
            "output was: {output}"
        );
        assert!(
            output.contains("mapper = { msg = \"data\" }"),
            "output was: {output}"
        );

        let reparsed = toml::from_str::<AppConfig>(&output).expect("roundtrip should parse");
        assert_eq!(reparsed.task[0].actions[0].mapper, expected_scrape);
        assert_eq!(reparsed.task[0].actions[1].mapper, expected_reply);
    }

    #[test]
    fn rejects_to_field_in_action_entry() {
        let error = toml::from_str::<AppConfig>(
            r#"
            [[task]]
            name = "task_name"
            chat_id = "123"
            actions = [
              { id = "x", to = "code", text = { text = "/start" } },
            ]
            "#,
        )
        .expect_err("to field should be rejected after removal");

        assert!(error.to_string().contains("to"));
    }

    #[test]
    fn rejects_unknown_action_entry_fields() {
        let error = toml::from_str::<AppConfig>(
            r#"
            [[task]]
            name = "task_name"
            chat_id = "123"
            actions = [
              { wrong = "x", text = { text = "/start" } },
            ]
            "#,
        )
        .expect_err("unknown action entry fields should be rejected");

        assert!(error.to_string().contains("wrong"));
    }

    #[test]
    fn rejects_action_entry_with_multiple_bodies() {
        let error = toml::from_str::<AppConfig>(
            r#"
            [[task]]
            name = "task_name"
            chat_id = "123"
            actions = [
              { text = { text = "/start" }, click = { key = "OK" } },
            ]
            "#,
        )
        .expect_err("multiple action bodies should be rejected");

        assert!(error.to_string().contains("multiple bodies"));
    }

    #[test]
    fn rejects_action_entry_without_body() {
        let error = toml::from_str::<AppConfig>(
            r#"
            [[task]]
            name = "task_name"
            chat_id = "123"
            actions = [
              { id = "only-meta" },
            ]
            "#,
        )
        .expect_err("action entries without a body should be rejected");

        assert!(error.to_string().contains("missing a body"));
    }

    #[test]
    fn serializes_actions_as_inline_tables() {
        let config = toml::from_str::<AppConfig>(
            r#"
            [[task]]
            name = "task_name"
            chat_id = "123"
            actions = [
              { text = { text = "hello" } },
            ]
            "#,
        )
        .expect("inline task config should parse");

        let output = to_inline_toml(&config);
        assert!(output.contains("actions = [\n  { text = { text = \"hello\" } },\n]"));
        assert!(!output.contains("[[task.actions]]"));
    }

    #[test]
    fn parses_and_serializes_browserless_action() {
        let config = toml::from_str::<AppConfig>(
            r#"
            [[task]]
            name = "task_name"
            chat_id = "123"
            actions = [
              { browserless = { token = "tok", query = "mutation zpr($url: String!) { goto(url: $url) { status } }", operation_name = "zpr" } },
            ]
            "#,
        )
        .expect("browserless task config should parse");

        let Action::Browserless(action) = &config.task[0].actions[0].action else {
            panic!("expected a browserless action");
        };
        assert_eq!(action.token, "tok");
        assert_eq!(action.operation_name, "zpr");
        assert_eq!(action.url, None);

        let output = to_inline_toml(&config);
        assert!(output.contains("browserless = { token = \"tok\""));
        assert!(output.contains("operation_name = \"zpr\""));

        let reparsed = toml::from_str::<AppConfig>(&output).expect("roundtrip should parse");
        let Action::Browserless(action) = &reparsed.task[0].actions[0].action else {
            panic!("expected a browserless action");
        };
        assert_eq!(action.token, "tok");
        assert_eq!(action.operation_name, "zpr");
    }

    #[test]
    fn parses_and_serializes_browserless_variables() {
        let config = toml::from_str::<AppConfig>(
            r#"
            [[task]]
            name = "task_name"
            chat_id = "123"
            actions = [
              { browserless = { token = "tok", query = "mutation G($url: String!) { goto(url: $url) { status } }", operation_name = "G", variables = { url = "{{ ctx.open.data }}" } } },
            ]
            "#,
        )
        .expect("browserless variables should parse");

        let Action::Browserless(action) = &config.task[0].actions[0].action else {
            panic!("expected a browserless action");
        };
        assert_eq!(action.variables["url"], "{{ ctx.open.data }}");

        let output = to_inline_toml(&config);
        assert!(
            output.contains("variables = { url = \"{{ ctx.open.data }}\" }"),
            "output was: {output}"
        );
        let reparsed = toml::from_str::<AppConfig>(&output).expect("roundtrip should parse");
        let Action::Browserless(action) = &reparsed.task[0].actions[0].action else {
            panic!("expected a browserless action");
        };
        assert_eq!(action.variables["url"], "{{ ctx.open.data }}");
    }

    #[test]
    fn rejects_removed_browserless_session_fields() {
        let error = toml::from_str::<AppConfig>(
            r#"
            [[task]]
            name = "task_name"
            chat_id = "123"
            actions = [
              { browserless = { token = "tok", query = "mutation S { status }", operation_name = "S", create_session = true } },
            ]
            "#,
        )
        .expect_err("create_session should be rejected after removal");
        assert!(error.to_string().contains("create_session"));

        let error = toml::from_str::<AppConfig>(
            r#"
            [[task]]
            name = "task_name"
            chat_id = "123"
            actions = [
              { browserless = { token = "tok", query = "mutation S { status }", operation_name = "S", use_session = true } },
            ]
            "#,
        )
        .expect_err("use_session should be rejected after removal");
        assert!(error.to_string().contains("use_session"));

        let error = toml::from_str::<AppConfig>(
            r#"
            [[task]]
            name = "task_name"
            chat_id = "123"
            actions = [
              { browserless = { token = "tok", query = "mutation S { status }", operation_name = "S", session_timeout = 60000 } },
            ]
            "#,
        )
        .expect_err("session_timeout should be rejected after removal");
        assert!(error.to_string().contains("session_timeout"));
    }

    #[test]
    fn rejects_action_level_schedule_fields() {
        let error = toml::from_str::<AppConfig>(
            r#"
            [[task]]
            name = "task_name"
            chat_id = "123"
            action = [
              { cron = "0 0 9 * * *", text = { text = "/start" } },
            ]
            "#,
        )
        .expect_err("action-level schedule fields should be rejected");

        assert!(error.to_string().contains("cron"));
    }
}
