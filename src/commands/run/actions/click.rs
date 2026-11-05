use crate::ast;
use crate::config::{ClickAction, JevOptions};
use crate::telegram::TelegramConnection;
use anyhow::{Context, Result, anyhow, bail};
use grammers_client::Client;
use grammers_client::message::Message;
use grammers_client::session::types::{PeerId, PeerRef};
use grammers_client::tl;
use grammers_client::tl::types::InlineButtonTypeUrl;
use grammers_client::update::Update;
use serde_json::Value;
use std::time::Duration;
use tokio::sync::broadcast::error::RecvError;
use tokio::time::{Instant, timeout_at};
use tracing::{debug, info};

const CLICK_LOOKUP_LIMIT: usize = 30;
const CLICK_WAIT: Duration = Duration::from_secs(15);
const DEFAULT_CHOICE_PATH: &str = "/answers/choice/choice";

/// What a completed `click` action produced.
#[derive(Debug)]
pub(super) enum ClickOutcome {
    /// A callback button was clicked; carries the bot's answer text, if it replied.
    Answer(Option<String>),
    /// A URL button was matched; carries its target.
    Url(String),
}

pub(super) async fn click_inline_keyboard(
    connection: &TelegramConnection,
    peer: PeerRef,
    click: &ClickAction,
    jev: Option<&JevOptions>,
    previous_message: Option<&Message>,
) -> Result<ClickOutcome> {
    let client = &connection.client;
    let key = if click.is_jev(jev.is_some()) {
        None
    } else {
        Some(
            click
                .key_text()
                .context("key is required for click action when jev is not enabled")?,
        )
    };
    let messages = inline_keyboard_messages(connection, peer, previous_message, key).await?;

    if let Some(key) = key {
        click_inline_keyboard_key(client, peer, &messages, key).await
    } else {
        let message = &messages[0];
        let candidates = extract_inline_buttons(message.reply_markup().as_ref());
        click_with_jev(client, peer, message, candidates, click, jev).await
    }
}

async fn inline_keyboard_messages(
    connection: &TelegramConnection,
    peer: PeerRef,
    previous_message: Option<&Message>,
    key: Option<&str>,
) -> Result<Vec<Message>> {
    let ready = |messages: &[Message]| match key {
        None => !messages.is_empty(),
        Some(key) => messages.iter().any(|message| {
            matches!(
                find_inline_button(message.reply_markup().as_ref(), key),
                Some(InlineButtonMatch::Callback(_) | InlineButtonMatch::Url(_))
            )
        }),
    };

    let previous = previous_message
        .filter(|message| !extract_inline_buttons(message.reply_markup().as_ref()).is_empty())
        .cloned();
    let mut found: Vec<Message> = previous.iter().cloned().collect();
    if ready(&found) {
        return Ok(found);
    }

    found.extend(recent_inline_keyboards(&connection.client, peer).await?);
    if ready(&found) {
        return Ok(found);
    }

    let deadline = Instant::now() + CLICK_WAIT;
    let mut updates = connection.updates.lock().await;
    loop {
        match timeout_at(deadline, updates.recv()).await {
            Ok(Ok(update)) => {
                if let Some(message) = inline_keyboard_from_update(update, peer.id) {
                    found.insert(0, message);
                    if ready(&found) {
                        return Ok(found);
                    }
                }
            }
            Ok(Err(RecvError::Lagged(_))) => {}
            Ok(Err(RecvError::Closed)) | Err(_) => break,
        }
    }

    if !found.is_empty() {
        return Ok(found);
    }

    bail!(
        "no inline keyboard buttons found in the latest {CLICK_LOOKUP_LIMIT} messages or within {CLICK_WAIT:?}"
    )
}

/// Returns the messages with an inline keyboard among the latest [`CLICK_LOOKUP_LIMIT`], newest first.
async fn recent_inline_keyboards(client: &Client, peer: PeerRef) -> Result<Vec<Message>> {
    let mut messages = client.iter_messages(peer).limit(CLICK_LOOKUP_LIMIT);

    let mut found = Vec::new();
    while let Some(message) = messages.next().await? {
        if !extract_inline_buttons(message.reply_markup().as_ref()).is_empty() {
            found.push(message);
        }
    }
    Ok(found)
}

fn inline_keyboard_from_update(update: Update, peer: PeerId) -> Option<Message> {
    let message = match update {
        Update::NewMessage(message) | Update::MessageEdited(message) => message.into_inner(),
        _ => return None,
    };
    (message.peer_id() == peer
        && !extract_inline_buttons(message.reply_markup().as_ref()).is_empty())
    .then_some(message)
}

async fn click_with_jev(
    client: &Client,
    peer: PeerRef,
    message: &Message,
    candidates: Vec<InlineCandidate>,
    click: &ClickAction,
    jev: Option<&JevOptions>,
) -> Result<ClickOutcome> {
    let button_texts: Vec<String> = candidates.iter().map(|c| c.text.clone()).collect();
    let message_text = message.text();
    let instructions = click
        .key_text()
        .unwrap_or("Select the appropriate button to click");

    let state = if message_text.trim().is_empty() {
        instructions
    } else {
        message_text
    };

    let choice_path = click
        .choice
        .as_deref()
        .or_else(|| jev.and_then(|options| options.choice.as_deref()));

    let choice =
        request_system_one_choice(state, &button_texts, instructions, choice_path, jev).await?;

    let matched = candidates
        .iter()
        .find(|c| c.text == choice)
        .or_else(|| {
            candidates
                .iter()
                .find(|c| c.text.starts_with(&choice) || choice.starts_with(&c.text))
        })
        .or_else(|| {
            candidates
                .iter()
                .find(|c| c.text.contains(&choice) || choice.contains(&c.text))
        })
        .ok_or_else(|| anyhow!("System One chose `{choice}`, but no matching button was found"))?;

    info!(
        "System One selected choice `{choice}`, clicking button `{}`",
        matched.text
    );

    match &matched.target {
        InlineButtonMatch::Callback(data) => {
            click_inline_callback(client, peer, message.id(), data.clone()).await
        }
        InlineButtonMatch::Url(url) => Ok(ClickOutcome::Url(url.clone())),
        InlineButtonMatch::Unsupported(text) => {
            bail!("matched inline keyboard button `{text}`, but this button type is not supported")
        }
    }
}

async fn request_system_one_choice(
    state: &str,
    button_texts: &[String],
    instructions: &str,
    choice_path: Option<&str>,
    jev: Option<&JevOptions>,
) -> Result<String> {
    let settings = JevSettings::resolve(jev)?;

    let mut criteria = serde_json::Map::new();
    for text in button_texts {
        criteria.insert(text.clone(), serde_json::Value::String(text.clone()));
    }

    let body = serde_json::json!({
        "model": settings.model,
        "state": state,
        "questions": {
            "choice": {
                "type": "choice",
                "instructions": instructions,
                "criteria": criteria,
            }
        }
    });

    debug!(
        "requesting System One API: base_url={} model={}, {body}",
        settings.base_url, settings.model
    );

    let response = reqwest::Client::new()
        .post(&settings.base_url)
        .bearer_auth(settings.api_key)
        .json(&body)
        .send()
        .await
        .context("failed to call System One API")?;

    let status = response.status();
    let payload = response
        .text()
        .await
        .context("failed to read System One API response body")?;

    debug!("System One API response: status={status} payload={payload}");

    let payload = serde_json::from_str::<Value>(&payload)
        .context("failed to parse System One API response JSON")?;

    parse_system_one_choice(&payload, choice_path)
}

fn parse_system_one_choice(payload: &Value, choice_path: Option<&str>) -> Result<String> {
    let path = choice_path.unwrap_or(DEFAULT_CHOICE_PATH);
    let expr =
        ast::parse_json_path(path).with_context(|| format!("invalid choice JSON path `{path}`"))?;
    let value = ast::eval(&expr, payload)
        .with_context(|| format!("choice JSON path `{path}` did not resolve"))?;

    match value {
        Value::String(value) => Ok(value.clone()),
        Value::Number(value) => Ok(value.to_string()),
        _ => bail!("choice JSON path `{path}` resolved to a non-scalar value"),
    }
}

struct JevSettings {
    api_key: String,
    base_url: String,
    model: String,
}

impl JevSettings {
    fn resolve(options: Option<&JevOptions>) -> Result<Self> {
        let options = options.cloned().unwrap_or_default();
        Ok(Self {
            api_key: setting(
                options.api_key.as_deref(),
                &["TYPESAFE_API_KEY", "JEV_API_KEY"],
            )
            .context("TYPESAFE_API_KEY or JEV_API_KEY is required for jev click")?,
            base_url: setting(
                options.base_url.as_deref(),
                &["TYPESAFE_BASE_URL", "JEV_BASE_URL"],
            )
            .unwrap_or_else(|| "https://api.typesafe.ai/v1/systemone".to_string()),
            model: setting(options.model.as_deref(), &["TYPESAFE_MODEL", "JEV_MODEL"])
                .unwrap_or_else(|| "jev-latest".to_string()),
        })
    }
}

fn setting(value: Option<&str>, env_keys: &[&str]) -> Option<String> {
    value
        .map(str::to_string)
        .or_else(|| env_keys.iter().find_map(|key| std::env::var(key).ok()))
}

struct InlineCandidate {
    text: String,
    target: InlineButtonMatch,
}

fn extract_inline_buttons(markup: Option<&tl::enums::ReplyMarkup>) -> Vec<InlineCandidate> {
    match markup {
        Some(tl::enums::ReplyMarkup::ReplyInlineMarkup(markup)) => {
            let mut candidates = Vec::new();
            for row in &markup.rows {
                let tl::enums::KeyboardInlineButtonRow::Row(row) = row;
                for button in &row.buttons {
                    let tl::enums::KeyboardInlineButton::Button(button) = button;
                    let target = match &button.r#type {
                        tl::enums::InlineButtonType::Callback(callback) => {
                            InlineButtonMatch::Callback(callback.data.clone())
                        }
                        tl::enums::InlineButtonType::Url(InlineButtonTypeUrl { url }) => {
                            InlineButtonMatch::Url(url.clone())
                        }
                        _ => InlineButtonMatch::Unsupported(button.text.clone()),
                    };
                    candidates.push(InlineCandidate {
                        text: button.text.clone(),
                        target,
                    });
                }
            }
            candidates
        }
        _ => Vec::new(),
    }
}

/// Clicks the inline keyboard button matching `key` (prefix match).
async fn click_inline_keyboard_key(
    client: &Client,
    peer: PeerRef,
    messages: &[Message],
    key: &str,
) -> Result<ClickOutcome> {
    let mut unsupported = None;
    for message in messages {
        match find_inline_button(message.reply_markup().as_ref(), key) {
            Some(InlineButtonMatch::Callback(data)) => {
                return click_inline_callback(client, peer, message.id(), data).await;
            }
            Some(InlineButtonMatch::Url(url)) => return Ok(ClickOutcome::Url(url)),
            Some(InlineButtonMatch::Unsupported(text)) => unsupported = Some(text),
            None => {}
        }
    }

    if let Some(text) = unsupported {
        bail!("matched inline keyboard button `{text}`, but this button type is not supported")
    }

    bail!(
        "no inline keyboard button found for key `{key}` in the latest {CLICK_LOOKUP_LIMIT} messages"
    )
}

async fn click_inline_callback(
    client: &Client,
    peer: PeerRef,
    message_id: i32,
    data: Vec<u8>,
) -> Result<ClickOutcome> {
    let answer = client
        .invoke(&tl::functions::messages::GetBotCallbackAnswer {
            game: false,
            peer: peer.into(),
            msg_id: message_id,
            data: Some(data),
            password: None,
        })
        .await?;
    callback_answer_outcome(answer)
}

fn callback_answer_outcome(answer: tl::enums::messages::BotCallbackAnswer) -> Result<ClickOutcome> {
    let tl::enums::messages::BotCallbackAnswer::Answer(answer) = answer;
    let text = answer.message.filter(|message| !message.trim().is_empty());
    if answer.alert {
        bail!(
            "click received a modal alert: {}",
            text.as_deref().unwrap_or("no message")
        );
    }
    if let Some(url) = answer.url {
        return Ok(ClickOutcome::Url(url));
    }
    if let Some(text) = text.as_deref() {
        info!("{text}");
    }
    Ok(ClickOutcome::Answer(text))
}

enum InlineButtonMatch {
    Callback(Vec<u8>),
    Url(String),
    Unsupported(String),
}

fn find_inline_button(
    markup: Option<&tl::enums::ReplyMarkup>,
    key: &str,
) -> Option<InlineButtonMatch> {
    extract_inline_buttons(markup)
        .into_iter()
        .find(|candidate| candidate.text.starts_with(key))
        .map(|candidate| candidate.target)
}

#[cfg(test)]
mod tests {
    use grammers_client::tl;

    use super::{ClickOutcome, InlineButtonMatch, callback_answer_outcome, find_inline_button};

    #[test]
    fn finds_inline_callback_button_by_text() {
        let markup = tl::types::ReplyInlineMarkup {
            force_reply: false,
            rows: vec![
                tl::types::KeyboardInlineButtonRow {
                    buttons: vec![
                        tl::types::KeyboardInlineButton {
                            style: None,
                            text: "🎯 签到".to_string(),
                            r#type: tl::enums::InlineButtonType::Callback(
                                tl::types::InlineButtonTypeCallback {
                                    requires_password: false,
                                    data: b"checkin".to_vec(),
                                },
                            ),
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
    fn finds_inline_url_button_by_prefix() {
        let markup = tl::types::ReplyInlineMarkup {
            force_reply: false,
            rows: vec![
                tl::types::KeyboardInlineButtonRow {
                    buttons: vec![
                        tl::types::KeyboardInlineButton {
                            style: None,
                            text: "打开网页".to_string(),
                            r#type: tl::enums::InlineButtonType::Url(
                                tl::types::InlineButtonTypeUrl {
                                    url: "https://example.com/dashboard".to_string(),
                                },
                            ),
                        }
                        .into(),
                    ],
                }
                .into(),
            ],
        }
        .into();

        let matched = find_inline_button(Some(&markup), "打开网页");
        match matched {
            Some(InlineButtonMatch::Url(url)) => assert_eq!(url, "https://example.com/dashboard"),
            _ => panic!("expected inline url button"),
        }
    }

    #[test]
    fn ignores_reply_keyboard_buttons_for_click() {
        let markup = tl::types::ReplyKeyboardMarkup {
            resize: false,
            single_use: false,
            selective: false,
            persistent: false,
            force_reply: false,
            rows: vec![
                tl::types::KeyboardButtonRow {
                    buttons: vec![
                        tl::types::KeyboardButton {
                            style: None,
                            text: "🎯 签到".to_string(),
                            r#type: tl::enums::ButtonType::Default,
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

        match callback_answer_outcome(answer).unwrap() {
            ClickOutcome::Answer(text) => assert_eq!(text.as_deref(), Some("checked in")),
            ClickOutcome::Url(url) => panic!("expected an answer, got url {url}"),
        }
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

        match callback_answer_outcome(answer).unwrap() {
            ClickOutcome::Answer(text) => assert!(text.is_none()),
            ClickOutcome::Url(url) => panic!("expected an answer, got url {url}"),
        }
    }

    #[test]
    fn returns_the_url_of_a_callback_answer() {
        let answer = tl::types::messages::BotCallbackAnswer {
            alert: false,
            has_url: true,
            native_ui: false,
            message: None,
            url: Some("https://example.com/app".to_string()),
            cache_time: 0,
        }
        .into();

        match callback_answer_outcome(answer).unwrap() {
            ClickOutcome::Url(url) => assert_eq!(url, "https://example.com/app"),
            ClickOutcome::Answer(_) => panic!("expected a url"),
        }
    }

    #[test]
    fn modal_alert_stops_click_with_an_error() {
        let answer = tl::types::messages::BotCallbackAnswer {
            alert: true,
            has_url: false,
            native_ui: false,
            message: Some("Please verify your account".to_string()),
            url: None,
            cache_time: 0,
        }
        .into();

        let err = callback_answer_outcome(answer).unwrap_err().to_string();
        assert!(err.contains("modal alert"), "got: {err}");
        assert!(err.contains("Please verify your account"), "got: {err}");
    }

    #[test]
    fn extracts_all_inline_buttons() {
        let markup = tl::types::ReplyInlineMarkup {
            force_reply: false,
            rows: vec![
                tl::types::KeyboardInlineButtonRow {
                    buttons: vec![
                        tl::types::KeyboardInlineButton {
                            style: None,
                            text: "Option A".to_string(),
                            r#type: tl::enums::InlineButtonType::Callback(
                                tl::types::InlineButtonTypeCallback {
                                    requires_password: false,
                                    data: b"a".to_vec(),
                                },
                            ),
                        }
                        .into(),
                        tl::types::KeyboardInlineButton {
                            style: None,
                            text: "Option B".to_string(),
                            r#type: tl::enums::InlineButtonType::Url(
                                tl::types::InlineButtonTypeUrl {
                                    url: "https://example.com/b".to_string(),
                                },
                            ),
                        }
                        .into(),
                    ],
                }
                .into(),
            ],
        }
        .into();

        let candidates = super::extract_inline_buttons(Some(&markup));
        assert_eq!(candidates.len(), 2);
        assert_eq!(candidates[0].text, "Option A");
        assert!(matches!(
            candidates[0].target,
            super::InlineButtonMatch::Callback(_)
        ));
        assert_eq!(candidates[1].text, "Option B");
        assert!(matches!(
            candidates[1].target,
            super::InlineButtonMatch::Url(_)
        ));
    }

    #[test]
    fn parses_system_one_choice_response() {
        let json = serde_json::json!({
            "model": "jev-latest",
            "answers": {
                "choice": {
                    "type": "choice",
                    "choice": "Option A",
                    "confidence": 0.95
                }
            }
        });

        let choice = super::parse_system_one_choice(&json, None).unwrap();
        assert_eq!(choice, "Option A");
    }

    #[test]
    fn parses_system_one_choice_with_custom_path() {
        let json = serde_json::json!({
            "result": {
                "answers": {
                    "choice": {
                        "choice": "Custom Option"
                    }
                }
            }
        });

        let choice =
            super::parse_system_one_choice(&json, Some("/result/answers/choice/choice")).unwrap();
        assert_eq!(choice, "Custom Option");
    }

    #[test]
    fn parses_system_one_choice_with_dotted_path() {
        let json = serde_json::json!({
            "result": {
                "choice": "Dotted Option"
            }
        });

        let choice = super::parse_system_one_choice(&json, Some("result.choice")).unwrap();
        assert_eq!(choice, "Dotted Option");
    }
}
