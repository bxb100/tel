use crate::config::{
    Action, AppConfig, BrowserlessAction, ClickAction, DiceAction, Task, TextAction,
};
use crate::db::{Db, SessionData};
use crate::telegram::TelegramConnection;
use anyhow::{Context, Result, anyhow, bail, ensure};
use chrono::Utc;
use cron::Schedule;
use grammers_client::message::{InputMessage, Message};
use grammers_client::session::types::PeerRef;
use grammers_client::tl::types::InlineButtonTypeUrl;
use grammers_client::{Client, tl};
use regex_lite::Regex;
use std::path::PathBuf;
use std::str::FromStr;
use std::sync::LazyLock;
use std::time::Duration;
use std::{env, fs};
use tokio::time::sleep;
use tracing::{debug, error, info};

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
    let session_path = resolve_session_path(user, session_path)?;

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

fn resolve_session_path(user: Option<i64>, session_path: Option<String>) -> Result<PathBuf> {
    match (env::var("SESSION_PATH"), session_path) {
        (Ok(path), _) | (_, Some(path)) => {
            let path = PathBuf::from(path);
            ensure!(
                path.exists(),
                "--session-path must point to an existing grammers SQLite session file"
            );
            return Ok(path);
        }
        _ => {}
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
        if let Err(error) = run_task(client, task).await {
            error!("task {} failed: {}", task.name, error);
        } else {
            info!("✔️ Done!");
        }
    }

    Ok(())
}

async fn run_task(client: &Client, task: &Task) -> Result<()> {
    ensure!(
        !task.actions.is_empty(),
        "task {} has no action entries",
        task.name
    );

    let peer = resolve_task_peer(client, task).await?;
    wait_for_task_trigger(task).await?;

    let mut carry = Carry::None;
    for action in &task.actions {
        carry = execute_action(client, peer, action, carry).await?;
        if let Some(sleep) = apply_delay(task).await {
            debug!("Waiting {sleep}s before execute {action:?}")
        }
    }

    Ok(())
}

/// Value threaded from one action to the next. The two hand-offs are mutually
/// exclusive, so a single enum makes "a message and a URL at once" unrepresentable.
enum Carry {
    /// Nothing to hand to the next action.
    None,
    /// The last message we sent; `click` searches it for inline buttons.
    /// Boxed because `Message` is large relative to the other variants.
    Message(Box<Message>),
    /// A URL opened by a `click` on a URL button; `browserless` consumes it as `variables.url`.
    Url(String),
}

impl Carry {
    /// The carried message, if the previous action left one (for `click`).
    fn into_message(self) -> Option<Message> {
        match self {
            Carry::Message(message) => Some(*message),
            _ => None,
        }
    }

    /// The carried URL, if the previous action left one (for `browserless`).
    fn into_url(self) -> Option<String> {
        match self {
            Carry::Url(url) => Some(url),
            _ => None,
        }
    }
}

async fn execute_action(
    client: &Client,
    peer: PeerRef,
    action: &Action,
    carry: Carry,
) -> Result<Carry> {
    match action {
        Action::Text(text) => Ok(Carry::Message(Box::new(
            client.send_message(peer, text.text.as_str()).await?,
        ))),
        Action::Dice(dice) => {
            let message = InputMessage::new().media(tl::types::InputMediaDice {
                emoticon: dice.dice.clone(),
            });
            Ok(Carry::Message(Box::new(
                client.send_message(peer, message).await?,
            )))
        }
        Action::Click(click) => {
            let previous_message = carry.into_message();
            match click_inline_keyboard(client, peer, &click.key, previous_message.as_ref()).await?
            {
                Some(url) => Ok(Carry::Url(url)),
                None => Ok(Carry::None),
            }
        }
        Action::Llm(llm) => execute_llm_action(client, peer, &llm.prompt, carry).await,
        Action::Browserless(browserless) => {
            let url = match carry.into_url() {
                Some(url) => Some(resolve_url_if_miniapp(client, &url).await?),
                None => None,
            };
            execute_browserless_action(browserless, url).await?;
            Ok(Carry::None)
        }
    }
}

async fn apply_delay(task: &Task) -> Option<u64> {
    if let Some(max_delay) = task.delay.filter(|delay| *delay > 0) {
        let seconds = rand::random_range(1..=u64::from(max_delay));
        sleep(Duration::from_secs(seconds)).await;
        Some(seconds)
    } else {
        None
    }
}

struct MiniappLink {
    bot: String,
    short_name: String,
    start_param: Option<String>,
}

static MINIAPP_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"^https?://t\.me/(?P<bot>[^/?#]+)/(?P<app>[^/?#]+)(?:[?#]|$)").unwrap()
});
static STARTAPP_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"[?&]startapp=(?P<v>[^&#]*)").unwrap());

fn parse_miniapp_link(url: &str) -> Option<MiniappLink> {
    let caps = MINIAPP_RE.captures(url.trim())?;
    let short_name = &caps["app"];
    if short_name.bytes().all(|b| b.is_ascii_digit()) {
        return None; // t.me/频道/123 是帖子链接，不是 miniapp
    }
    let start_param = STARTAPP_RE
        .captures(url)
        .map(|c| c["v"].to_string())
        .filter(|v| !v.is_empty());
    Some(MiniappLink {
        bot: caps["bot"].to_string(),
        short_name: short_name.to_string(),
        start_param,
    })
}

async fn resolve_url_if_miniapp(client: &Client, url: &str) -> Result<String> {
    let Some(link) = parse_miniapp_link(url) else {
        return Ok(url.to_string()); // 不是 miniapp 链接，原样返回
    };

    let peer = client
        .resolve_username(&link.bot)
        .await
        .with_context(|| format!("failed to resolve miniapp bot @{}", link.bot))?
        .with_context(|| format!("miniapp bot @{} not found", link.bot))?;
    let bot_ref = peer
        .to_ref()
        .await
        .map_err(|err| anyhow!("{}", err))?
        .with_context(|| format!("cannot use @{} as a peer", link.bot))?;

    let result = client
        .invoke(&tl::functions::messages::RequestAppWebView {
            write_allowed: false,
            compact: false,
            fullscreen: false,
            peer: (&bot_ref).into(),
            app: tl::enums::InputBotApp::ShortName(tl::types::InputBotAppShortName {
                bot_id: (&bot_ref).into(),
                short_name: link.short_name.clone(),
            }),
            start_param: link.start_param,
            theme_params: None,
            platform: "android".to_string(),
        })
        .await
        .with_context(|| {
            format!(
                "failed to resolve miniapp url @{}/{}",
                link.bot, link.short_name
            )
        })?;

    match result {
        tl::enums::WebViewResult::Url(url) => Ok(url.url),
        // _ => bail!("unexpected webview result for @{}/{}", link.bot, link.short_name),
    }
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
            info!("Waiting until {next} for cron trigger...");
            sleep(wait).await;
        }
    }

    Ok(())
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
            let id_matches = dialog.peer_id().bot_api_dialog_id_unchecked().to_string() == chat_id;
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
        Some(peer) => Ok(Some(peer.to_ref().await.unwrap().with_context(|| {
            format!("resolved @{username}, but it cannot be used as a peer")
        })?)),
        None => Ok(None),
    }
}

//noinspection HttpUrlsUsage
fn trim_username(value: &str) -> &str {
    value
        .trim()
        .trim_start_matches('@')
        .trim_start_matches("https://t.me/")
        .trim_start_matches("http://t.me/")
        .trim_start_matches("t.me/")
}

/// Clicks the inline keyboard button matching `key` (prefix match).
///
/// Checks `previous_message` first, then scans the chat's recent messages (retrying while
/// the bot may still be composing its reply). Returns:
/// - `Ok(None)` — a callback button was clicked
/// - `Ok(Some(url))` — a URL button was matched; its target is returned for a following
///   `browserless` action to consume as `variables.url`
async fn click_inline_keyboard(
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

/// Asks the LLM to choose the next action, then runs it through the same executor
/// as configured actions so state hand-off (e.g. `click` reading the previous
/// message) behaves identically.
async fn execute_llm_action(
    client: &Client,
    peer: PeerRef,
    prompt: &str,
    carry: Carry,
) -> Result<Carry> {
    let action = request_llm_action(prompt).await?;
    Box::pin(execute_action(client, peer, &action, carry)).await
}

/// Not test
async fn request_llm_action(prompt: &str) -> Result<Action> {
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

fn parse_llm_action(payload: serde_json::Value) -> Result<Action> {
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
            .map(|text| {
                Action::Text(TextAction {
                    text: text.to_string(),
                })
            })
            .context("task_action.options.text is required for text action"),
        "dice" => options
            .get("dice")
            .and_then(|value| value.as_str())
            .map(|dice| {
                Action::Dice(DiceAction {
                    dice: dice.to_string(),
                })
            })
            .context("task_action.options.dice is required for dice action"),
        "click" => options
            .get("key")
            .and_then(|value| value.as_str())
            .map(|key| {
                Action::Click(ClickAction {
                    key: key.to_string(),
                })
            })
            .context("task_action.options.key is required for click action"),
        other => bail!("unsupported llm action_type: {other}"),
    }
}

const DEFAULT_BROWSERLESS_URL: &str = "https://production-sfo.browserless.io/stealth/bql";

/// Executes a browserless browser action by POSTing a GraphQL (BQL) request to Browserless.
///
/// Request shape (mirrors the documented curl example):
///   POST <url>?token=<token>&blockConsentModals=true&emulationOs=android&emulatedDevice=pixel-8
///   Content-Type: application/json
///   {"query": ..., "variables": ..., "operationName": ...}
async fn execute_browserless_action(
    action: &BrowserlessAction,
    url_from_button: Option<String>,
) -> Result<()> {
    debug!("{:?}", url_from_button);

    let url = browserless_request_url(
        action.url.as_deref().unwrap_or(DEFAULT_BROWSERLESS_URL),
        &action.token,
    )?;

    let body = browserless_request_body(action, url_from_button);

    let response = reqwest::Client::new()
        .post(url)
        .json(&body)
        .timeout(Duration::from_mins(3))
        .send()
        .await
        .context("failed to call browserless bql endpoint")?;

    let status = response.status();
    let payload = response
        .text()
        .await
        .context("failed to read browserless response body")?;

    ensure!(
        status.is_success(),
        "browserless bql returned {status}: {payload:#}"
    );

    debug!("browserless return {}", payload);

    Ok(())
}

/// Builds the GraphQL request body: `{"query": ..., "variables": ..., "operationName": ...}`.
/// `variables.url` is only included when a preceding `click` opened a URL button.
fn browserless_request_body(
    action: &BrowserlessAction,
    url_from_button: Option<String>,
) -> serde_json::Value {
    match url_from_button {
        Some(url) => serde_json::json!({
            "query": action.query,
            "variables": { "url": url },
            "operationName": action.operation_name,
        }),
        None => serde_json::json!({
            "query": action.query,
            "operationName": action.operation_name,
        }),
    }
}

/// Builds `<base>?token=<token>&blockConsentModals=true&emulationOs=android&emulatedDevice=pixel-8`.
fn browserless_request_url(base: &str, token: &str) -> Result<String> {
    let mut url =
        reqwest::Url::parse(base).with_context(|| format!("invalid browserless url: {base}"))?;
    url.query_pairs_mut()
        .append_pair("token", token)
        .append_pair("blockConsentModals", "true")
        .append_pair("emulationOs", "android")
        .append_pair("emulatedDevice", "pixel-8");
    Ok(url.to_string())
}

#[cfg(test)]
mod tests {
    use grammers_client::tl;

    use super::{
        InlineButtonMatch, browserless_request_body, browserless_request_url, callback_answer_text,
        find_inline_button, parse_llm_action,
    };
    use crate::config::{Action, BrowserlessAction};

    #[test]
    fn builds_default_browserless_request_url() {
        let url = browserless_request_url(
            "https://production-sfo.browserless.io/stealth/bql",
            "my-token",
        )
        .expect("url should build");
        assert_eq!(
            url,
            "https://production-sfo.browserless.io/stealth/bql?token=my-token&blockConsentModals=true&emulationOs=android&emulatedDevice=pixel-8"
        );
    }

    #[test]
    fn encodes_token_in_browserless_request_url() {
        let url = browserless_request_url(
            "https://production-sfo.browserless.io/stealth/bql",
            "tok en&1",
        )
        .expect("url should build");
        assert!(url.contains("token=tok+en%261"), "url was: {url}");
    }

    #[test]
    fn builds_browserless_request_body_with_url_from_button() {
        let action = BrowserlessAction {
            token: "tok".to_string(),
            query: "mutation zpr($url: String!) { goto(url: $url) { status } }".to_string(),
            operation_name: "zpr".to_string(),
            url: None,
        };

        let body =
            browserless_request_body(&action, Some("https://example.com/dashboard".to_string()));
        assert_eq!(
            body,
            serde_json::json!({
                "query": "mutation zpr($url: String!) { goto(url: $url) { status } }",
                "variables": { "url": "https://example.com/dashboard" },
                "operationName": "zpr",
            })
        );
    }

    #[test]
    fn builds_browserless_request_body_without_url() {
        let action = BrowserlessAction {
            token: "tok".to_string(),
            query: "query { text(selector: \"body\") { text } }".to_string(),
            operation_name: String::new(),
            url: None,
        };

        let body = browserless_request_body(&action, None);
        assert_eq!(
            body,
            serde_json::json!({
                "query": "query { text(selector: \"body\") { text } }",
                "operationName": "",
            })
        );
        assert!(body.get("variables").is_none());
    }

    #[tokio::test]
    async fn posts_browserless_request_to_bql_endpoint() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        use tokio::net::TcpListener;

        use super::execute_browserless_action;

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();

        let server = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut buffer = Vec::new();
            // read one request
            let mut chunk = [0u8; 4096];
            loop {
                let n = socket.read(&mut chunk).await.unwrap();
                if n == 0 {
                    break;
                }
                buffer.extend_from_slice(&chunk[..n]);
                let text = String::from_utf8_lossy(&buffer).to_string();
                let headers_end = text.find("\r\n\r\n").map(|i| i + 4).unwrap_or(0);
                let content_length = text
                    .lines()
                    .find(|l| l.to_ascii_lowercase().starts_with("content-length"))
                    .and_then(|l| l.split(':').nth(1))
                    .and_then(|v| v.trim().parse::<usize>().ok());
                if content_length.is_some_and(|len| buffer.len() >= headers_end + len) {
                    break;
                }
            }
            let request = String::from_utf8_lossy(&buffer).to_string();
            let response =
                b"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 2\r\n\r\n{}";
            socket.write_all(response).await.unwrap();
            socket.flush().await.unwrap();
            request
        });

        let action = BrowserlessAction {
            token: "my-secret-token".to_string(),
            query: "mutation zpr($url: String!) { goto(url: $url) { status } }".to_string(),
            operation_name: "zpr".to_string(),
            url: Some(format!("http://{addr}/stealth/bql")),
        };

        execute_browserless_action(&action, Some("https://example.com/dashboard".to_string()))
            .await
            .expect("browserless action should succeed");

        let request = server.await.unwrap();
        let request_line = request.lines().next().unwrap();
        assert!(
            request_line.starts_with("POST /stealth/bql?token=my-secret-token&blockConsentModals=true&emulationOs=android&emulatedDevice=pixel-8 HTTP/"),
            "request line was: {request_line}"
        );
        assert!(
            request.contains("content-type: application/json")
                || request.contains("Content-Type: application/json")
        );
        assert!(request.contains("\"operationName\":\"zpr\""));
        assert!(request.contains("\"variables\":{\"url\":\"https://example.com/dashboard\"}"));
    }

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
            Action::Text(text) => assert_eq!(text.text, "hello"),
            _ => panic!("expected text action"),
        }
    }

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
