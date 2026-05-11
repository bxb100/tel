use serde::{Deserialize, Serialize};
use std::fmt::{Debug, Display, Formatter};

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
    pub action: Vec<Action>,
}

#[derive(Debug, Serialize, Deserialize, Clone, Default)]
#[serde(deny_unknown_fields)]
pub struct Action {
    pub text: Option<TextAction>,
    pub dice: Option<DiceAction>,
    pub click: Option<ClickAction>,
    pub llm: Option<LlmAction>,
}

impl Display for Action {
    fn fmt(&self, f: &mut Formatter<'_>) -> std::fmt::Result {
        let out: Box<dyn Debug> = match self {
            Action { text: Some(a), .. } => Box::new(a),
            Action { dice: Some(a), .. } => Box::new(a),
            Action { click: Some(a), .. } => Box::new(a),
            Action { llm: Some(a), .. } => Box::new(a),
            Action { .. } => return Err(std::fmt::Error),
        };
        write!(f, "{out:?}")
    }
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

pub fn to_inline_toml(config: &AppConfig) -> Result<String, String> {
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

        output.push_str("action = [\n");
        for action in &task.action {
            output.push_str("  ");
            push_inline_action(&mut output, action)?;
            output.push_str(",\n");
        }
        output.push_str("]\n\n");
    }

    Ok(output)
}

fn push_string_field(output: &mut String, key: &str, value: &str) {
    output.push_str(key);
    output.push_str(" = ");
    output.push_str(&toml_string(value));
    output.push('\n');
}

fn push_inline_action(output: &mut String, action: &Action) -> Result<(), String> {
    let mut action_count = 0;

    if let Some(text) = action.text.as_ref() {
        action_count += 1;
        output.push_str("{ text = { text = ");
        output.push_str(&toml_string(&text.text));
        output.push_str(" } }");
    }

    if let Some(dice) = action.dice.as_ref() {
        action_count += 1;
        output.push_str("{ dice = { dice = ");
        output.push_str(&toml_string(&dice.dice));
        output.push_str(" } }");
    }

    if let Some(click) = action.click.as_ref() {
        action_count += 1;
        output.push_str("{ click = { key = ");
        output.push_str(&toml_string(&click.key));
        output.push_str(" } }");
    }

    if let Some(llm) = action.llm.as_ref() {
        action_count += 1;
        output.push_str("{ llm = { prompt = ");
        output.push_str(&toml_string(&llm.prompt));
        output.push_str(" } }");
    }

    match action_count {
        1 => Ok(()),
        0 => Err("task action must contain one action body".to_string()),
        _ => Err("task action must contain only one action body".to_string()),
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
    use super::{AppConfig, to_inline_toml};

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
            action = [
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
        assert_eq!(task.action.len(), 4);
        assert_eq!(task.action[0].text.as_ref().unwrap().text, "/start");
    }

    #[test]
    fn serializes_actions_as_inline_tables() {
        let config = toml::from_str::<AppConfig>(
            r#"
            [[task]]
            name = "task_name"
            chat_id = "123"
            action = [
              { text = { text = "hello" } },
            ]
            "#,
        )
        .expect("inline task config should parse");

        let output = to_inline_toml(&config).expect("config should serialize");
        assert!(output.contains("action = [\n  { text = { text = \"hello\" } },\n]"));
        assert!(!output.contains("[[task.action]]"));
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
