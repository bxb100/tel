use crate::db::{Db, SessionData};
use crate::telegram::{self, TelegramConnection};
use grammers_client::SignInError;
use inquire::{Password, PasswordDisplayMode, Text};

pub async fn execute() -> anyhow::Result<()> {
    let phone = Text::new("Phone number (e.g., +123456789):").prompt()?;

    println!("Connecting to Telegram...");
    let session_path = telegram::session_path_for_phone(&phone)?;
    let connection = TelegramConnection::open(&session_path).await?;
    let client = &connection.client;

    let token = client
        .request_login_code(&phone, &telegram::api_hash())
        .await?;
    let code = Text::new("Verification code:").prompt()?;

    let signed_in = match client.sign_in(&token, &code).await {
        Ok(user) => user,
        Err(SignInError::PasswordRequired(token)) => {
            let password = Password::new("Two-step verification password:")
                .with_display_mode(PasswordDisplayMode::Hidden)
                .prompt()?;
            client.check_password(token, password.trim()).await?
        }
        Err(e) => return Err(e.into()),
    };

    println!("Logged in as {}", signed_in.full_name());

    let db = Db::new()?;
    let session_data = SessionData {
        user_id: signed_in.id().bare_id(),
        name: signed_in.full_name(),
        session_path: session_path.to_string_lossy().into_owned(),
    };
    db.insert_session(&session_data)?;
    connection.shutdown().await;
    println!("Session stored successfully.");

    Ok(())
}
