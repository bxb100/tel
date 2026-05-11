use crate::db::Db;
use std::path::Path;

pub async fn execute() -> anyhow::Result<()> {
    let db = Db::new()?;
    for session in db.list_sessions()? {
        let session_path = Path::new(&session.session_path);
        if session_path.exists() {
            std::fs::remove_file(session_path)?;
        }
    }
    db.remove_all()?;
    println!("Logged out successfully. All session data removed.");
    Ok(())
}
