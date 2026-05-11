use rusqlite::{Connection, Result};

pub struct Db {
    conn: Connection,
}

#[derive(Debug, Clone)]
pub struct SessionData {
    pub user_id: i64,
    pub name: String,
    pub session_path: String,
}

impl Db {
    pub fn new() -> Result<Self> {
        let conn = Connection::open("session.db")?;
        conn.execute(
            "CREATE TABLE IF NOT EXISTS sessions (
                user_id INTEGER PRIMARY KEY,
                name TEXT NOT NULL,
                session_path TEXT NOT NULL
            )",
            [],
        )?;
        Self::migrate_schema(&conn)?;
        Ok(Self { conn })
    }

    fn migrate_schema(conn: &Connection) -> Result<()> {
        let mut stmt = conn.prepare("PRAGMA table_info(sessions)")?;
        let columns = stmt
            .query_map([], |row| row.get::<_, String>(1))?
            .collect::<Result<Vec<_>>>()?;

        if !columns.iter().any(|column| column == "session_path") {
            conn.execute(
                "ALTER TABLE sessions ADD COLUMN session_path TEXT NOT NULL DEFAULT ''",
                [],
            )?;
        }

        Ok(())
    }

    pub fn insert_session(&self, data: &SessionData) -> Result<()> {
        self.conn.execute(
            "INSERT OR REPLACE INTO sessions (user_id, name, session_path) VALUES (?1, ?2, ?3)",
            (&data.user_id, &data.name, &data.session_path),
        )?;
        Ok(())
    }

    pub fn remove_all(&self) -> Result<()> {
        self.conn.execute("DELETE FROM sessions", [])?;
        Ok(())
    }

    pub fn get_session(&self, user_id: i64) -> Result<Option<SessionData>> {
        let mut stmt = self
            .conn
            .prepare("SELECT user_id, name, session_path FROM sessions WHERE user_id = ?1")?;
        let mut rows = stmt.query([&user_id])?;

        if let Some(row) = rows.next()? {
            Ok(Some(SessionData {
                user_id: row.get(0)?,
                name: row.get(1)?,
                session_path: row.get(2)?,
            }))
        } else {
            Ok(None)
        }
    }

    pub fn list_sessions(&self) -> Result<Vec<SessionData>> {
        let mut stmt = self
            .conn
            .prepare("SELECT user_id, name, session_path FROM sessions ORDER BY user_id")?;
        let rows = stmt.query_map([], |row| {
            Ok(SessionData {
                user_id: row.get(0)?,
                name: row.get(1)?,
                session_path: row.get(2)?,
            })
        })?;

        rows.collect()
    }
}
