use std::env;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{Context, Result};
use grammers_client::sender::SenderPoolFatHandle;
use grammers_client::{Client, SenderPool};
use tokio::task::JoinHandle;

use crate::session_store::RusqliteSession;

const DEFAULT_API_ID: i32 = 9911045;
const DEFAULT_API_HASH: &str = "45ae393448c97ddbd6c74d02b31ea024";
const SESSION_DIR: &str = ".tel/sessions";

pub struct TelegramConnection {
    pub client: Client,
    handle: SenderPoolFatHandle,
    runner_task: JoinHandle<()>,
}

impl TelegramConnection {
    pub async fn open<P: AsRef<Path>>(session_path: P) -> Result<Self> {
        let session = Arc::new(RusqliteSession::open(session_path.as_ref()).with_context(
            || {
                format!(
                    "failed to open Telegram session at {}",
                    session_path.as_ref().display()
                )
            },
        )?);
        let SenderPool { runner, handle, .. } = SenderPool::new(Arc::clone(&session), api_id()?);
        let client = Client::new(handle.clone());
        let runner_task = tokio::spawn(runner.run());

        Ok(Self {
            client,
            handle,
            runner_task,
        })
    }

    pub async fn shutdown(self) {
        self.handle.quit();
        let _ = self.runner_task.await;
    }
}

pub fn api_hash() -> String {
    env::var("TG_API_HASH").unwrap_or_else(|_| DEFAULT_API_HASH.to_string())
}

fn api_id() -> Result<i32> {
    match env::var("TG_API_ID") {
        Ok(value) => value
            .parse::<i32>()
            .context("TG_API_ID must be a valid i32"),
        Err(_) => Ok(DEFAULT_API_ID),
    }
}

pub fn session_path_for_phone(phone: &str) -> Result<PathBuf> {
    let dir = ensure_session_dir()?;
    Ok(dir.join(format!("{}.sqlite", sanitize_filename(phone))))
}

pub fn ensure_session_dir() -> Result<PathBuf> {
    let dir = PathBuf::from(SESSION_DIR);
    std::fs::create_dir_all(&dir).context("failed to create Telegram session directory")?;
    Ok(dir)
}

fn sanitize_filename(value: &str) -> String {
    let sanitized: String = value
        .chars()
        .filter_map(|ch| match ch {
            'a'..='z' | 'A'..='Z' | '0'..='9' => Some(ch),
            '+' => Some('p'),
            '-' | '_' => Some(ch),
            _ => None,
        })
        .collect();

    if sanitized.is_empty() {
        "default".to_string()
    } else {
        sanitized
    }
}
