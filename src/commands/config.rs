use crate::ast;
use crate::config::{Action, AppConfig, DICE_EMOTICONS};
use anyhow::{Context, Result, bail, ensure};
use cron::Schedule;
use std::collections::HashSet;
use std::fs;
use std::str::FromStr;
use tracing::info;

pub fn execute(validate: bool, path: &str) -> Result<()> {
    if !validate {
        bail!("no action specified; use --validate to validate the configuration file");
    }

    validate_config(path)?;
    info!("Configuration `{path}` is valid.");
    Ok(())
}

pub fn validate_config(path: &str) -> Result<AppConfig> {
    let content = fs::read_to_string(path)
        .with_context(|| format!("failed to read config file from {path}"))?;
    let config = toml::from_str::<AppConfig>(&content)
        .with_context(|| format!("failed to parse TOML from {path}"))?;

    ensure!(
        !config.task.is_empty(),
        "configuration has no [[task]] entries"
    );

    let jev_enabled = config.jev.is_some();
    if let Some(jev) = &config.jev {
        ensure!(
            !jev.is_empty(),
            "`[jev]` must define at least one of api_key, base_url, model, or choice"
        );
        if let Some(choice) = &jev.choice {
            ast::parse_json_path(choice)
                .with_context(|| format!("invalid global jev choice path `{choice}`"))?;
        }
    }

    for task in &config.task {
        ensure!(!task.name.trim().is_empty(), "task has an empty name");
        ensure!(
            !task.chat_id.trim().is_empty()
                || task
                    .chat_name
                    .as_deref()
                    .is_some_and(|n| !n.trim().is_empty()),
            "task `{}` must specify chat_id or chat_name",
            task.name
        );

        if let Some(cron) = &task.cron {
            Schedule::from_str(cron).with_context(|| {
                format!("task `{}`: invalid cron expression `{cron}`", task.name)
            })?;
        }

        ensure!(
            !task.actions.is_empty(),
            "task `{}` has no action entries",
            task.name
        );

        let mut seen_ids = HashSet::new();

        for (index, entry) in task.actions.iter().enumerate() {
            if let Some(id) = &entry.id {
                ensure!(
                    seen_ids.insert(id.clone()),
                    "task `{}`: duplicate action id `{id}`",
                    task.name
                );
            }

            for (key, path) in &entry.mapper {
                ast::parse_data_path(path).with_context(|| {
                    format!(
                        "task `{}`: invalid mapper path `{path}` for key `{key}`",
                        task.name
                    )
                })?;
            }

            validate_action(&entry.action, &task.name, index, jev_enabled)?;
        }
    }

    Ok(config)
}

fn validate_action(
    action: &Action,
    task_name: &str,
    index: usize,
    jev_enabled: bool,
) -> Result<()> {
    match action {
        Action::Text(text) => {
            ast::parse_template(&text.text).with_context(|| {
                format!("task `{task_name}`, action #{index}: invalid text template")
            })?;
        }
        Action::Dice(dice) => {
            ensure!(
                DICE_EMOTICONS.contains(&dice.dice.as_str()),
                "task `{task_name}`, action #{index}: unsupported dice emoticon `{}`",
                dice.dice
            );
        }
        Action::Click(click) => {
            if !click.is_jev(jev_enabled) {
                ensure!(
                    click.key_text().is_some(),
                    "task `{task_name}`, action #{index}: click action requires `key` when JEV is not enabled"
                );
            }
            if let Some(choice) = &click.choice {
                ensure!(
                    !choice.trim().is_empty(),
                    "task `{task_name}`, action #{index}: click action choice cannot be empty"
                );
                ast::parse_json_path(choice).with_context(|| {
                    format!("task `{task_name}`, action #{index}: invalid choice path `{choice}`")
                })?;
            }
        }
        Action::Llm(llm) => {
            ensure!(
                !llm.prompt.trim().is_empty(),
                "task `{task_name}`, action #{index}: llm prompt cannot be empty"
            );
        }
        Action::Browserless(browserless) => {
            ensure!(
                !browserless.token.trim().is_empty(),
                "task `{task_name}`, action #{index}: browserless token cannot be empty"
            );
            ensure!(
                !browserless.query.trim().is_empty(),
                "task `{task_name}`, action #{index}: browserless query cannot be empty"
            );
            if let Some(url) = &browserless.url {
                ast::parse_template(url).with_context(|| {
                    format!("task `{task_name}`, action #{index}: invalid browserless url template")
                })?;
            }
            for (key, val) in &browserless.variables {
                ast::parse_template(val).with_context(|| {
                    format!(
                        "task `{task_name}`, action #{index}: invalid browserless variable `{key}` template"
                    )
                })?;
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::validate_config;
    use std::fs;

    #[test]
    fn validates_valid_config() {
        let dir = std::env::temp_dir();
        let path = dir.join("valid_tasks.toml");
        fs::write(
            &path,
            r#"
            [[task]]
            name = "valid_task"
            chat_id = "123"
            actions = [
                { text = { text = "hello" } },
                { click = { key = "OK" } },
            ]
            "#,
        )
        .unwrap();

        let result = validate_config(path.to_str().unwrap());
        assert!(result.is_ok());
        let _ = fs::remove_file(path);
    }

    #[test]
    fn fails_on_duplicate_action_id() {
        let dir = std::env::temp_dir();
        let path = dir.join("dup_id_tasks.toml");
        fs::write(
            &path,
            r#"
            [[task]]
            name = "task1"
            chat_id = "123"
            actions = [
                { id = "login", text = { text = "/start" } },
                { id = "login", click = { key = "OK" } },
            ]
            "#,
        )
        .unwrap();

        let err = validate_config(path.to_str().unwrap()).unwrap_err();
        assert!(err.to_string().contains("duplicate action id `login`"));
        let _ = fs::remove_file(path);
    }

    #[test]
    fn fails_on_invalid_cron() {
        let dir = std::env::temp_dir();
        let path = dir.join("bad_cron_tasks.toml");
        fs::write(
            &path,
            r#"
            [[task]]
            name = "task1"
            chat_id = "123"
            cron = "invalid_cron"
            actions = [
                { text = { text = "/start" } },
            ]
            "#,
        )
        .unwrap();

        let err = validate_config(path.to_str().unwrap()).unwrap_err();
        assert!(err.to_string().contains("invalid cron expression"));
        let _ = fs::remove_file(path);
    }

    #[test]
    fn click_without_key_valid_if_jev() {
        let dir = std::env::temp_dir();
        let path = dir.join("jev_tasks.toml");
        fs::write(
            &path,
            r#"
            [jev]
            api_key = "test_key"

            [[task]]
            name = "task1"
            chat_id = "123"
            actions = [
                { click = {} },
            ]
            "#,
        )
        .unwrap();

        let result = validate_config(path.to_str().unwrap());
        assert!(result.is_ok());
        let _ = fs::remove_file(path);
    }

    #[test]
    fn fails_on_empty_jev_options() {
        let dir = std::env::temp_dir();
        let path = dir.join("empty_jev_tasks.toml");
        fs::write(
            &path,
            r#"
            [jev]

            [[task]]
            name = "task1"
            chat_id = "123"
            actions = [
                { click = { jev = true } },
            ]
            "#,
        )
        .unwrap();

        let err = validate_config(path.to_str().unwrap()).unwrap_err();
        assert!(err.to_string().contains("must define at least one"));
        let _ = fs::remove_file(path);
    }

    #[test]
    fn fails_on_invalid_choice_path() {
        let dir = std::env::temp_dir();
        let path = dir.join("invalid_choice_tasks.toml");
        fs::write(
            &path,
            r#"
            [jev]
            api_key = "test_key"

            [[task]]
            name = "task1"
            chat_id = "123"
            actions = [
                { click = { jev = true, choice = "answers..choice" } },
            ]
            "#,
        )
        .unwrap();

        let err = validate_config(path.to_str().unwrap()).unwrap_err();
        assert!(err.to_string().contains("invalid choice path"));
        let _ = fs::remove_file(path);
    }
}
