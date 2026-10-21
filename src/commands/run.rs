mod actions;

use crate::config::{AppConfig, Task};
use crate::db::{Db, SessionData};
use crate::telegram::TelegramConnection;
use actions::{TaskContext, execute_action, render_action};
use anyhow::{Context, Result, bail, ensure};
use chrono::Utc;
use cron::Schedule;
use grammers_client::Client;
use grammers_client::session::types::PeerRef;
use std::path::PathBuf;
use std::str::FromStr;
use std::time::Duration;
use std::{env, fs};
use tokio::time::sleep;
use tracing::{debug, error, info};

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

    let mut context = TaskContext::default();

    for (index, entry) in task.actions.iter().enumerate() {
        // id defaults to the 0-based action index when not configured.
        let id = entry.id.clone().unwrap_or_else(|| index.to_string());

        let rendered = render_action(&entry.action, &context)?;
        let output = execute_action(client, peer, &rendered, &mut context).await?;
        context.record_output(&id, &entry.mapper, output)?;

        if let Some(sleep) = apply_delay(task).await {
            debug!(
                "Waiting {sleep}s before execute {action:?}",
                action = entry.action
            )
        }
    }

    Ok(())
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
