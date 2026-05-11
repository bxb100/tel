use crate::config::{
    Action, AppConfig, ClickAction, DiceAction, LlmAction, Task, TextAction, to_inline_toml,
};
use inquire::{Confirm, Select, Text};
use std::fs;

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

        let mut actions = Vec::new();
        loop {
            let action_type =
                Select::new("Action type:", vec!["text", "dice", "click", "llm"]).prompt()?;

            let mut action = Action::default();

            match action_type {
                "text" => {
                    let text = Text::new("Text to send:").prompt()?;
                    action.text = Some(TextAction { text });
                }
                "dice" => {
                    let dice = Select::new("Dice type:", vec!["🎲", "🏀", "🎯", "⚽", "🎳", "🎰"])
                        .prompt()?;
                    action.dice = Some(DiceAction {
                        dice: dice.to_string(),
                    });
                }
                "click" => {
                    let key = Text::new("Key to click:").prompt()?;
                    action.click = Some(ClickAction { key });
                }
                "llm" => {
                    let prompt = Text::new("LLM Prompt:").prompt()?;
                    action.llm = Some(LlmAction { prompt });
                }
                _ => unreachable!(),
            }

            actions.push(action);

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
            action: actions,
        });

        let add_more_task = Confirm::new("Add another task?")
            .with_default(false)
            .prompt()?;
        if !add_more_task {
            break;
        }
    }

    let mut config = AppConfig { task: Vec::new() };
    if let Ok(content) = fs::read_to_string("tasks.toml") {
        if let Ok(mut existing_config) = toml::from_str::<AppConfig>(&content) {
            config.task.append(&mut existing_config.task);
        }
    }

    config.task.extend(tasks);

    let toml_string = to_inline_toml(&config).map_err(anyhow::Error::msg)?;
    fs::write("tasks.toml", toml_string)?;
    println!("Tasks saved to tasks.toml");

    Ok(())
}

fn optional_input(value: String) -> Option<String> {
    let value = value.trim().to_string();
    if value.is_empty() { None } else { Some(value) }
}
