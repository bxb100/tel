use crate::config::{
    Action, ActionEntry, AppConfig, BrowserlessAction, ClickAction, DiceAction, LlmAction, Task,
    TextAction, to_inline_toml,
};
use inquire::{Confirm, Editor, Select, Text};
use std::fs;
use tracing::info;

pub async fn execute_task() -> anyhow::Result<()> {
    let mut tasks = Vec::new();

    loop {
        let name = Text::new("Task name:").prompt()?;
        let chat_id = Text::new("Chat ID:").prompt()?;
        let chat_name = Text::new("Chat name (optional):").prompt()?;
        let chat_name = optional_input(chat_name);
        let cron = Text::new("Cron expression (optional):").prompt()?;
        let cron = optional_input(cron);
        let delay_str = Text::new("Delay (optional):").prompt()?;
        let delay = match optional_input(delay_str) {
            Some(delay) => Some(delay.parse::<u32>()?),
            None => None,
        };

        let mut actions: Vec<ActionEntry> = Vec::new();
        loop {
            let action_type = Select::new(
                "Action type:",
                vec!["text", "dice", "click", "llm", "browserless"],
            )
            .prompt()?;

            let action = match action_type {
                "text" => {
                    let text = Text::new("Text to send:").prompt()?;
                    Action::Text(TextAction { text })
                }
                "dice" => {
                    let dice = Select::new("Dice type:", vec!["🎲", "🏀", "🎯", "⚽", "🎳", "🎰"])
                        .prompt()?;
                    Action::Dice(DiceAction {
                        dice: dice.to_string(),
                    })
                }
                "click" => {
                    let key = Text::new("Key to click:").prompt()?;
                    Action::Click(ClickAction { key })
                }
                "llm" => {
                    let prompt = Text::new("LLM Prompt:").prompt()?;
                    Action::Llm(LlmAction { prompt })
                }
                "browserless" => {
                    let token = Text::new("Browserless token:").prompt()?;
                    let query = Editor::new("BQL query (press 'e' to open your editor):")
                        .with_file_extension("graphql")
                        .prompt()?;
                    let operation_name = Text::new("Operation name:")
                        .with_placeholder("NewTab")
                        .prompt()?;
                    let url = Text::new("Browserless URL (optional):")
                        .with_placeholder("https://production-sfo.browserless.io/stealth/bql")
                        .prompt_skippable()?;

                    Action::Browserless(BrowserlessAction {
                        token,
                        query,
                        operation_name,
                        url,
                        variables: Default::default(),
                    })
                }
                _ => unreachable!(),
            };

            let id = optional_input(Text::new("Action id (optional):").prompt()?);
            let mapper = prompt_mapper()?;
            actions.push(ActionEntry { id, mapper, action });

            let add_more = Confirm::new("Add another action to this task?")
                .with_default(false)
                .prompt()?;
            if !add_more {
                break;
            }
        }

        tasks.push(Task {
            name,
            chat_id,
            chat_name,
            cron,
            delay,
            actions,
        });

        let add_more_task = Confirm::new("Add another task?")
            .with_default(false)
            .prompt()?;
        if !add_more_task {
            break;
        }
    }

    let mut config = AppConfig { task: Vec::new() };
    if let Ok(content) = fs::read_to_string("tasks.toml")
        && let Ok(mut existing_config) = toml::from_str::<AppConfig>(&content)
    {
        config.task.append(&mut existing_config.task);
    }

    config.task.extend(tasks);

    let toml_string = to_inline_toml(&config);
    fs::write("tasks.toml", toml_string)?;
    info!("Tasks saved to tasks.toml");

    Ok(())
}

/// Prompts for mapper entries (`key = "json_path"`, both optional) until the
/// user stops adding them.
fn prompt_mapper() -> anyhow::Result<std::collections::BTreeMap<String, String>> {
    let mut mapper = std::collections::BTreeMap::new();
    loop {
        let add = Confirm::new("Add a mapper entry (key -> json_path)?")
            .with_default(false)
            .prompt()?;
        if !add {
            break;
        }
        let key = Text::new("Mapper key:").prompt()?;
        let path = Text::new("JSON path (relative to the action output `data`):").prompt()?;
        mapper.insert(key, path);
    }
    Ok(mapper)
}

fn optional_input(value: String) -> Option<String> {
    let value = value.trim().to_string();
    if value.is_empty() { None } else { Some(value) }
}
