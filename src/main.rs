mod commands;
mod config;
mod db;
mod session_store;
mod telegram;

use clap::{Parser, Subcommand};
use commands::{gen_config, login, logout, run};

#[derive(Parser)]
#[command(name = "tel", about = "A telegram MTProto API CLI")]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// Login and store session data
    Login,
    /// Logout and remove stored data
    Logout,
    /// Generate configuration
    Gen {
        #[command(subcommand)]
        command: GenCommands,
    },
    /// Run tasks
    Run {
        /// Task config TOML path
        #[arg(long, default_value = "tasks.toml")]
        config: String,
        /// Stored login user id
        #[arg(long)]
        user: Option<i64>,
        /// Path to a grammers SQLite session file
        #[arg(long = "session-string", alias = "session-path")]
        session_string: Option<String>,
        /// Run only one task by name
        #[arg(long)]
        task: Option<String>,
    },
}

#[derive(Subcommand)]
pub enum GenCommands {
    /// Prompt for task details and save to toml
    Task,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();

    match &cli.command {
        Commands::Login => login::execute().await?,
        Commands::Logout => logout::execute().await?,
        Commands::Gen { command } => match command {
            GenCommands::Task => gen_config::execute_task().await?,
        },
        Commands::Run {
            config,
            user,
            session_string,
            task,
        } => run::execute(config.clone(), *user, session_string.clone(), task.clone()).await?,
    }

    Ok(())
}
