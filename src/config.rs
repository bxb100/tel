use serde::{Deserialize, Serialize};
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
    pub actions: Vec<Action>,
}

/// A single task action.
///
/// Serialized as an externally tagged TOML inline table, e.g. `{ text = { text = "" } }`.
/// The enum makes "exactly one action body" a parse-time guarantee instead of a runtime check.
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
///
/// Mirrors the shape of the documented curl request:
/// `POST https://production-sfo.browserless.io/stealth/bql?token=<token>&proxy=residential&blockConsentModals=true`
/// with a JSON body of `{ "query": ..., "variables": ..., "operationName": ... }`.
#[derive(Debug, Serialize, Deserialize, Clone)]
#[serde(deny_unknown_fields)]
pub struct BrowserlessAction {
    /// Browserless token appended to the request URL.
    pub token: String,
    /// BQL (GraphQL) query/mutation text.
    pub query: String,
    /// Optional GraphQL operation name.
    pub operation_name: String,
    /// Override the Browserless endpoint (defaults to production-sfo stealth BQL).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
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
        for action in &task.actions {
            output.push_str("  ");
            push_inline_action(&mut output, action);
            output.push_str(",\n");
        }
        output.push_str("]\n\n");
    }

    output
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
            output.push_str("{ text = { text = ");
            output.push_str(&toml_string(&text.text));
            output.push_str(" } }");
        }
        Action::Dice(dice) => {
            output.push_str("{ dice = { dice = ");
            output.push_str(&toml_string(&dice.dice));
            output.push_str(" } }");
        }
        Action::Click(click) => {
            output.push_str("{ click = { key = ");
            output.push_str(&toml_string(&click.key));
            output.push_str(" } }");
        }
        Action::Llm(llm) => {
            output.push_str("{ llm = { prompt = ");
            output.push_str(&toml_string(&llm.prompt));
            output.push_str(" } }");
        }
        Action::Browserless(browserless) => {
            output.push_str("{ browserless = { token = ");
            output.push_str(&toml_string(&browserless.token));
            output.push_str(", query = ");
            output.push_str(&toml_string(&browserless.query));
            output.push_str(", operation_name = ");
            output.push_str(&toml_string(browserless.operation_name.as_ref()));

            if let Some(url) = browserless.url.as_ref() {
                output.push_str(", url = ");
                output.push_str(&toml_string(url));
            }
            output.push_str(" } }");
        }
    }
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
        assert!(matches!(&task.actions[0], Action::Text(text) if text.text == "/start"));
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

        let Action::Browserless(action) = &config.task[0].actions[0] else {
            panic!("expected a browserless action");
        };
        assert_eq!(action.token, "tok");
        assert_eq!(action.operation_name, "zpr");
        assert_eq!(action.url, None);

        let output = to_inline_toml(&config);
        assert!(output.contains("browserless = { token = \"tok\""));
        assert!(output.contains("operation_name = \"zpr\""));

        let reparsed = toml::from_str::<AppConfig>(&output).expect("roundtrip should parse");
        let Action::Browserless(action) = &reparsed.task[0].actions[0] else {
            panic!("expected a browserless action");
        };
        assert_eq!(action.token, "tok");
        assert_eq!(action.operation_name, "zpr");
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
