use anyhow::{Result, bail};
use grammers_client::Client;
use grammers_client::message::Message;
use grammers_client::session::types::PeerRef;
use grammers_client::tl;
use grammers_client::tl::types::InlineButtonTypeUrl;
use std::time::Duration;
use tokio::time::sleep;
use tracing::info;

const CLICK_LOOKUP_LIMIT: usize = 30;
const CLICK_LOOKUP_ATTEMPTS: usize = 15;
const CLICK_LOOKUP_INTERVAL: Duration = Duration::from_secs(1);

/// Clicks the inline keyboard button matching `key` (prefix match).
///
/// Checks `previous_message` first, then scans the chat's recent messages (retrying while
/// the bot may still be composing its reply). Returns:
/// - `Ok(None)` — a callback button was clicked
/// - `Ok(Some(url))` — a URL button was matched; its target is returned for a following
///   `browserless` action to consume as `variables.url`
pub(super) async fn click_inline_keyboard(
    client: &Client,
    peer: PeerRef,
    key: &str,
    previous_message: Option<&Message>,
) -> Result<Option<String>> {
    let mut unsupported_match = None;

    enum ClickButtonResult {
        Clicked,
        Url(String),
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
                Some(InlineButtonMatch::Url(url)) => Ok(ClickButtonResult::Url(url)),
                Some(InlineButtonMatch::Unsupported(text)) => {
                    Ok(ClickButtonResult::Unsupported(text))
                }
                None => Ok(ClickButtonResult::NotFound),
            }
        }
    };

    // normal path
    if let Some(message) = previous_message {
        match click_button(message, key).await? {
            ClickButtonResult::Clicked => return Ok(None),
            ClickButtonResult::Url(url) => return Ok(Some(url)),
            ClickButtonResult::Unsupported(text) => unsupported_match = Some(text),
            ClickButtonResult::NotFound => {}
        }
    }

    // fallback path
    for attempt in 0..CLICK_LOOKUP_ATTEMPTS {
        let mut messages = client.iter_messages(peer).limit(CLICK_LOOKUP_LIMIT);
        while let Some(ref message) = messages.next().await? {
            match click_button(message, key).await? {
                ClickButtonResult::Clicked => return Ok(None),
                ClickButtonResult::Url(url) => return Ok(Some(url)),
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
        info!("{text}");
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
    Url(String),
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
    rows: &[tl::enums::KeyboardInlineButtonRow],
    key: &str,
) -> Option<InlineButtonMatch> {
    for row in rows {
        match row {
            tl::enums::KeyboardInlineButtonRow::Row(row) => {
                for button in &row.buttons {
                    match button {
                        tl::enums::KeyboardInlineButton::Button(button) => {
                            if !button.text.starts_with(key) {
                                continue;
                            }

                            return match &button.r#type {
                                tl::enums::InlineButtonType::Callback(callback) => {
                                    Some(InlineButtonMatch::Callback(callback.data.clone()))
                                }
                                tl::enums::InlineButtonType::Url(InlineButtonTypeUrl { url }) => {
                                    Some(InlineButtonMatch::Url(url.clone()))
                                }
                                _ => Some(InlineButtonMatch::Unsupported(button.text.clone())),
                            };
                        }
                    }
                }
            }
        }
    }

    None
}

#[cfg(test)]
mod tests {
    use grammers_client::tl;

    use super::{InlineButtonMatch, callback_answer_text, find_inline_button};

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
