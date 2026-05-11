use anyhow::{Context, Result, bail, ensure};
use chrono::Utc;
use cron::Schedule;
use grammers_client::message::{InputMessage, Message};
use grammers_client::session::types::PeerRef;
use grammers_client::{Client, tl};
use std::path::PathBuf;
use std::str::FromStr;
use std::time::Duration;
use std::{env, fs};
use tokio::time::sleep;

use crate::config::{Action, AppConfig, Task};
use crate::db::{Db, SessionData};
use crate::telegram::TelegramConnection;

const CLICK_LOOKUP_LIMIT: usize = 30;
const CLICK_LOOKUP_ATTEMPTS: usize = 15;
const CLICK_LOOKUP_INTERVAL: Duration = Duration::from_secs(1);

pub async fn execute(
    config_path: String,
    user: Option<i64>,
    session_path: Option<String>,
    task_filter: Option<String>,
) -> Result<()> {
    let config = load_config(&config_path)?;
    let tasks = select_tasks(&config, task_filter.as_deref())?;
    let session_path = resolve_session_path(user, session_path.as_deref())?;

    let connection = TelegramConnection::open(&session_path).await?;
    let result = run_tasks(&connection.client, tasks).await;
    connection.shutdown().await;
    result
}

fn load_config(config_path: &str) -> Result<AppConfig> {
    let content = fs::read_to_string(config_path)
        .with_context(|| format!("failed to read task config from {config_path}"))?;
    let config = toml::from_str::<AppConfig>(&content)
        .with_context(|| format!("failed to parse task config from {config_path}"))?;
    ensure!(
        !config.task.is_empty(),
        "task config has no [[task]] entries"
    );
    Ok(config)
}

fn select_tasks<'a>(config: &'a AppConfig, task_filter: Option<&str>) -> Result<Vec<&'a Task>> {
    let tasks = config
        .task
        .iter()
        .filter(|task| task_filter.is_none_or(|filter| task.name == filter))
        .collect::<Vec<_>>();

    if let Some(filter) = task_filter {
        ensure!(!tasks.is_empty(), "task not found: {filter}");
    }

    Ok(tasks)
}

fn resolve_session_path(user: Option<i64>, session_path: Option<&str>) -> Result<PathBuf> {
    if let Some(session_path) = session_path {
        let path = PathBuf::from(session_path);
        ensure!(
            path.exists(),
            "--session-path must point to an existing grammers SQLite session file"
        );
        return Ok(path);
    }

    let db = Db::new()?;
    if let Some(user_id) = user {
        let session = db
            .get_session(user_id)?
            .with_context(|| format!("no stored session for user {user_id}"))?;
        return stored_session_path(session);
    }

    let sessions = db.list_sessions()?;
    match sessions.as_slice() {
        [] => bail!("no stored session; run `tel login` or pass --session-path"),
        [session] => stored_session_path(session.clone()),
        _ => bail!("multiple stored sessions; pass --user to select one"),
    }
}

fn stored_session_path(session: SessionData) -> Result<PathBuf> {
    ensure!(
        !session.session_path.trim().is_empty(),
        "stored session for user {} has no session path",
        session.user_id
    );

    let path = PathBuf::from(session.session_path);
    ensure!(
        path.exists(),
        "stored session file is missing for user {}: {}",
        session.user_id,
        path.display()
    );
    Ok(path)
}

async fn run_tasks(client: &Client, tasks: Vec<&Task>) -> Result<()> {
    ensure!(
        client.is_authorized().await?,
        "Telegram session is not authorized"
    );

    for task in tasks {
        run_task(client, task)
            .await
            .with_context(|| format!("task failed: {}", task.name))?;
    }

    Ok(())
}

async fn run_task(client: &Client, task: &Task) -> Result<()> {
    ensure!(
        !task.action.is_empty(),
        "task {} has no action entries",
        task.name
    );

    let peer = resolve_task_peer(client, task).await?;
    wait_for_task_trigger(task).await?;
    let mut previous_message = None;
    for action in &task.action {
        println!("{action:?}");
        previous_message = execute_action(client, peer, action, previous_message.as_ref()).await?;
    }

    Ok(())
}

async fn wait_for_task_trigger(task: &Task) -> Result<()> {
    if let Some(expression) = task.cron.as_deref() {
        let schedule = Schedule::from_str(expression)
            .with_context(|| format!("invalid cron expression: {expression}"))?;
        let next = schedule
            .upcoming(Utc)
            .next()
            .context("cron expression has no upcoming trigger")?;
        let wait = next
            .signed_duration_since(Utc::now())
            .to_std()
            .unwrap_or(Duration::ZERO);
        if !wait.is_zero() {
            println!("Waiting until {next} for cron trigger...");
            sleep(wait).await;
        }
    }

    if let Some(max_delay) = task.delay.filter(|delay| *delay > 0) {
        let seconds = rand::random_range(1..=u64::from(max_delay));
        println!("Waiting {seconds}s before action...");
        sleep(Duration::from_secs(seconds)).await;
    }

    Ok(())
}

async fn execute_action(
    client: &Client,
    peer: PeerRef,
    action: &Action,
    previous_message: Option<&Message>,
) -> Result<Option<Message>> {
    let message = match resolve_action(action)? {
        ResolvedAction::Text(text) => Some(client.send_message(peer, text.text.as_str()).await?),
        ResolvedAction::Dice(dice) => {
            let message = InputMessage::new().media(tl::types::InputMediaDice {
                emoticon: dice.dice.clone(),
            });
            Some(client.send_message(peer, message).await?)
        }
        ResolvedAction::Click(click) => {
            click_inline_keyboard(client, peer, &click.key, previous_message).await?
        }
        ResolvedAction::Llm(llm) => {
            execute_llm_action(client, peer, &llm.prompt, previous_message).await?
        }
    };
    Ok(message)
}

enum ResolvedAction<'a> {
    Text(&'a crate::config::TextAction),
    Dice(&'a crate::config::DiceAction),
    Click(&'a crate::config::ClickAction),
    Llm(&'a crate::config::LlmAction),
}

fn resolve_action(action: &Action) -> Result<ResolvedAction<'_>> {
    let mut selected = Vec::new();
    if let Some(text) = action.text.as_ref() {
        selected.push(ResolvedAction::Text(text));
    }
    if let Some(dice) = action.dice.as_ref() {
        selected.push(ResolvedAction::Dice(dice));
    }
    if let Some(click) = action.click.as_ref() {
        selected.push(ResolvedAction::Click(click));
    }
    if let Some(llm) = action.llm.as_ref() {
        selected.push(ResolvedAction::Llm(llm));
    }

    match selected.len() {
        1 => Ok(selected.remove(0)),
        0 => bail!("task action must contain one action body"),
        _ => bail!("task action must contain only one action body"),
    }
}

async fn resolve_task_peer(client: &Client, task: &Task) -> Result<PeerRef> {
    let chat_id = task.chat_id.trim();
    if !chat_id.is_empty() {
        if let Some(peer) = find_dialog_peer(client, Some(chat_id), None).await? {
            return Ok(peer);
        }

        if let Some(peer) = resolve_username_peer(client, chat_id).await? {
            return Ok(peer);
        }
    }

    if let Some(chat_name) = task.chat_name.as_deref() {
        if let Some(peer) = find_dialog_peer(client, None, Some(chat_name.trim())).await? {
            return Ok(peer);
        }

        if let Some(peer) = resolve_username_peer(client, chat_name).await? {
            return Ok(peer);
        }
    }

    bail!(
        "failed to resolve chat for task {}; chat_id={}, chat_name={}",
        task.name,
        task.chat_id,
        task.chat_name.as_deref().unwrap_or("")
    )
}

async fn find_dialog_peer(
    client: &Client,
    chat_id: Option<&str>,
    chat_name: Option<&str>,
) -> Result<Option<PeerRef>> {
    let mut dialogs = client.iter_dialogs().limit(500);
    while let Some(dialog) = dialogs.next().await? {
        let peer = dialog.peer();

        if let Some(chat_id) = chat_id {
            let id_matches = dialog.peer_id().bot_api_dialog_id().to_string() == chat_id;
            let username_matches = peer
                .username()
                .is_some_and(|username| username.eq_ignore_ascii_case(trim_username(chat_id)));
            if id_matches || username_matches {
                return Ok(Some(dialog.peer_ref()));
            }
        }

        if let Some(chat_name) = chat_name {
            let name_matches = peer.name().is_some_and(|name| name == chat_name);
            let username_matches = peer
                .username()
                .is_some_and(|username| username.eq_ignore_ascii_case(trim_username(chat_name)));
            if name_matches || username_matches {
                return Ok(Some(dialog.peer_ref()));
            }
        }
    }

    Ok(None)
}

async fn resolve_username_peer(client: &Client, value: &str) -> Result<Option<PeerRef>> {
    let username = trim_username(value);
    if username.is_empty()
        || username.parse::<i64>().is_ok()
        || username.contains(char::is_whitespace)
    {
        return Ok(None);
    }

    let peer = client.resolve_username(username).await?;
    match peer {
        Some(peer) => Ok(Some(peer.to_ref().await.with_context(|| {
            format!("resolved @{username}, but it cannot be used as a peer")
        })?)),
        None => Ok(None),
    }
}

fn trim_username(value: &str) -> &str {
    value
        .trim()
        .trim_start_matches('@')
        .trim_start_matches("https://t.me/")
        .trim_start_matches("http://t.me/")
        .trim_start_matches("t.me/")
}

async fn click_inline_keyboard(
    client: &Client,
    peer: PeerRef,
    key: &str,
    previous_message: Option<&Message>,
) -> Result<Option<Message>> {
    let mut unsupported_match = None;

    enum ClickButtonResult {
        Clicked,
        Unsupported(String),
        NotFound,
    }

    let click_button = |message: &Message, key: &str| {
        let message_id = message.id();
        let matched = find_inline_button(message.reply_markup().as_ref(), key);

        async move {
            match matched {
                Some(InlineButtonMatch::Callback(data)) => {
                    click_inline_callback(client, peer, message_id, data).await?;
                    Ok::<ClickButtonResult, anyhow::Error>(ClickButtonResult::Clicked)
                }
                Some(InlineButtonMatch::Unsupported(text)) => {
                    Ok(ClickButtonResult::Unsupported(text))
                }
                None => Ok(ClickButtonResult::NotFound),
            }
        }
    };

    if let Some(message) = previous_message {
        match click_button(message, key).await? {
            ClickButtonResult::Clicked => return Ok(None),
            ClickButtonResult::Unsupported(text) => unsupported_match = Some(text),
            ClickButtonResult::NotFound => {}
        }
    }

    for attempt in 0..CLICK_LOOKUP_ATTEMPTS {
        let mut messages = client.iter_messages(peer).limit(CLICK_LOOKUP_LIMIT);
        while let Some(ref message) = messages.next().await? {
            match click_button(message, key).await? {
                ClickButtonResult::Clicked => return Ok(None),
                ClickButtonResult::Unsupported(text) => unsupported_match = Some(text),
                ClickButtonResult::NotFound => {}
            }
        }

        if attempt + 1 < CLICK_LOOKUP_ATTEMPTS {
            sleep(CLICK_LOOKUP_INTERVAL).await;
        }
    }

    if let Some(text) = unsupported_match {
        bail!("matched inline keyboard button `{text}`, but this button type is not supported")
    }

    bail!(
        "no inline keyboard callback button found for key `{key}` in the latest {CLICK_LOOKUP_LIMIT} messages"
    )
}

async fn click_inline_callback(
    client: &Client,
    peer: PeerRef,
    message_id: i32,
    data: Vec<u8>,
) -> Result<()> {
    let answer = client
        .invoke(&tl::functions::messages::GetBotCallbackAnswer {
            game: false,
            peer: peer.into(),
            msg_id: message_id,
            data: Some(data),
            password: None,
        })
        .await?;
    if let Some(text) = callback_answer_text(answer) {
        println!("{text}");
    }
    Ok(())
}

fn callback_answer_text(answer: tl::enums::messages::BotCallbackAnswer) -> Option<String> {
    match answer {
        tl::enums::messages::BotCallbackAnswer::Answer(answer) => {
            answer.message.filter(|message| !message.trim().is_empty())
        }
    }
}

enum InlineButtonMatch {
    Callback(Vec<u8>),
    Unsupported(String),
}

fn find_inline_button(
    markup: Option<&tl::enums::ReplyMarkup>,
    key: &str,
) -> Option<InlineButtonMatch> {
    match markup {
        Some(tl::enums::ReplyMarkup::ReplyInlineMarkup(markup)) => {
            find_inline_button_in_rows(&markup.rows, key)
        }
        Some(
            tl::enums::ReplyMarkup::ReplyKeyboardHide(_)
            | tl::enums::ReplyMarkup::ReplyKeyboardForceReply(_)
            | tl::enums::ReplyMarkup::ReplyKeyboardMarkup(_),
        )
        | None => None,
    }
}

fn find_inline_button_in_rows(
    rows: &[tl::enums::KeyboardButtonRow],
    key: &str,
) -> Option<InlineButtonMatch> {
    for row in rows {
        match row {
            tl::enums::KeyboardButtonRow::Row(row) => {
                for button in &row.buttons {
                    let text = button.text();
                    if text != key {
                        continue;
                    }

                    return match button {
                        tl::enums::KeyboardButton::Callback(button) => {
                            Some(InlineButtonMatch::Callback(button.data.clone()))
                        }
                        _ => Some(InlineButtonMatch::Unsupported(text)),
                    };
                }
            }
        }
    }

    None
}

async fn execute_llm_action(
    client: &Client,
    peer: PeerRef,
    prompt: &str,
    previous_message: Option<&Message>,
) -> Result<Option<Message>> {
    let action = request_llm_action(prompt).await?;
    let message = match action {
        GeneratedAction::Text(text) => Some(client.send_message(peer, text).await?),
        GeneratedAction::Dice(dice) => {
            let message = InputMessage::new().media(tl::types::InputMediaDice { emoticon: dice });
            Some(client.send_message(peer, message).await?)
        }
        GeneratedAction::Click(key) => {
            click_inline_keyboard(client, peer, &key, previous_message).await?
        }
    };
    Ok(message)
}

enum GeneratedAction {
    Text(String),
    Dice(String),
    Click(String),
}

async fn request_llm_action(prompt: &str) -> Result<GeneratedAction> {
    let api_key =
        env::var("OPENAI_API_KEY").context("OPENAI_API_KEY is required for llm action")?;
    let model = env::var("OPENAI_MODEL").context("OPENAI_MODEL is required for llm action")?;
    let base_url =
        env::var("OPENAI_BASE_URL").unwrap_or_else(|_| "https://api.openai.com/v1".to_string());
    let url = format!("{}/chat/completions", base_url.trim_end_matches('/'));

    let body = serde_json::json!({
        "model": model,
        "messages": [
            {
                "role": "system",
                "content": "Choose exactly one Telegram task action. Call task_action with action_type and options. Do not answer with text content."
            },
            {
                "role": "user",
                "content": prompt
            }
        ],
        "tools": [
            {
                "type": "function",
                "function": {
                    "name": "task_action",
                    "description": "Select the next Telegram action to execute.",
                    "parameters": {
                        "type": "object",
                        "properties": {
                            "action_type": {
                                "type": "string",
                                "enum": ["text", "dice", "click"]
                            },
                            "options": {
                                "type": "object",
                                "properties": {
                                    "text": { "type": "string" },
                                    "dice": {
                                        "type": "string",
                                        "enum": ["🎲", "🏀", "🎯", "⚽", "🎳", "🎰"]
                                    },
                                    "key": { "type": "string" }
                                },
                                "additionalProperties": true
                            }
                        },
                        "required": ["action_type", "options"],
                        "additionalProperties": false
                    }
                }
            }
        ],
        "tool_choice": {
            "type": "function",
            "function": { "name": "task_action" }
        }
    });

    let response = reqwest::Client::new()
        .post(url)
        .bearer_auth(api_key)
        .json(&body)
        .send()
        .await
        .context("failed to call OpenAI-compatible chat completions endpoint")?;

    let status = response.status();
    let payload = response
        .text()
        .await
        .context("failed to read llm response body")?;

    ensure!(
        status.is_success(),
        "llm endpoint returned {status}: {payload}"
    );

    let payload = serde_json::from_str::<serde_json::Value>(&payload)
        .context("failed to parse llm response JSON")?;
    parse_llm_action(payload)
}

fn parse_llm_action(payload: serde_json::Value) -> Result<GeneratedAction> {
    let arguments = payload
        .pointer("/choices/0/message/tool_calls/0/function/arguments")
        .and_then(|value| value.as_str())
        .context("llm response did not include task_action tool arguments")?;
    let arguments = serde_json::from_str::<serde_json::Value>(arguments)
        .context("failed to parse task_action arguments")?;

    let action_type = arguments
        .get("action_type")
        .and_then(|value| value.as_str())
        .context("task_action.action_type is required")?;
    let options = arguments
        .get("options")
        .context("task_action.options is required")?;

    match action_type {
        "text" => options
            .get("text")
            .and_then(|value| value.as_str())
            .map(|text| GeneratedAction::Text(text.to_string()))
            .context("task_action.options.text is required for text action"),
        "dice" => options
            .get("dice")
            .and_then(|value| value.as_str())
            .map(|dice| GeneratedAction::Dice(dice.to_string()))
            .context("task_action.options.dice is required for dice action"),
        "click" => options
            .get("key")
            .and_then(|value| value.as_str())
            .map(|key| GeneratedAction::Click(key.to_string()))
            .context("task_action.options.key is required for click action"),
        other => bail!("unsupported llm action_type: {other}"),
    }
}

#[cfg(test)]
mod tests {
    use grammers_client::tl;

    use super::{
        GeneratedAction, InlineButtonMatch, callback_answer_text, find_inline_button,
        parse_llm_action,
    };

    #[test]
    fn parses_llm_tool_call_text_action() {
        let payload = serde_json::json!({
            "choices": [
                {
                    "message": {
                        "tool_calls": [
                            {
                                "function": {
                                    "arguments": "{\"action_type\":\"text\",\"options\":{\"text\":\"hello\"}}"
                                }
                            }
                        ]
                    }
                }
            ]
        });

        let action = parse_llm_action(payload).expect("tool call should parse");
        match action {
            GeneratedAction::Text(text) => assert_eq!(text, "hello"),
            _ => panic!("expected text action"),
        }
    }

    #[test]
    fn finds_inline_callback_button_by_text() {
        let markup = tl::types::ReplyInlineMarkup {
            rows: vec![
                tl::types::KeyboardButtonRow {
                    buttons: vec![
                        tl::types::KeyboardButtonCallback {
                            requires_password: false,
                            style: None,
                            text: "🎯 签到".to_string(),
                            data: b"checkin".to_vec(),
                        }
                        .into(),
                    ],
                }
                .into(),
            ],
        }
        .into();

        let matched = find_inline_button(Some(&markup), "🎯 签到");
        match matched {
            Some(InlineButtonMatch::Callback(data)) => assert_eq!(data, b"checkin"),
            _ => panic!("expected inline callback button"),
        }
    }

    #[test]
    fn ignores_reply_keyboard_buttons_for_click() {
        let markup = tl::types::ReplyKeyboardMarkup {
            resize: false,
            single_use: false,
            selective: false,
            persistent: false,
            rows: vec![
                tl::types::KeyboardButtonRow {
                    buttons: vec![
                        tl::types::KeyboardButton {
                            style: None,
                            text: "🎯 签到".to_string(),
                        }
                        .into(),
                    ],
                }
                .into(),
            ],
            placeholder: None,
        }
        .into();

        assert!(find_inline_button(Some(&markup), "🎯 签到").is_none());
    }

    #[test]
    fn extracts_non_empty_callback_answer_text() {
        let answer = tl::types::messages::BotCallbackAnswer {
            alert: false,
            has_url: false,
            native_ui: false,
            message: Some("checked in".to_string()),
            url: None,
            cache_time: 0,
        }
        .into();

        assert_eq!(callback_answer_text(answer).as_deref(), Some("checked in"));
    }

    #[test]
    fn ignores_blank_callback_answer_text() {
        let answer = tl::types::messages::BotCallbackAnswer {
            alert: false,
            has_url: false,
            native_ui: false,
            message: Some("   ".to_string()),
            url: None,
            cache_time: 0,
        }
        .into();

        assert!(callback_answer_text(answer).is_none());
    }
}
