use std::collections::HashMap;
use std::net::{SocketAddrV4, SocketAddrV6};
use std::path::Path;
use std::sync::Mutex;

use anyhow::{Context, Result};
use futures::future::BoxFuture;
use grammers_session::types::{
    ChannelKind, ChannelState, DcOption, PeerAuth, PeerId, PeerInfo, PeerKind, UpdateState,
    UpdatesState,
};
use grammers_session::{Session, SessionData};
use rusqlite::types::Type;
use rusqlite::{Connection, OptionalExtension, params};

const USER_SELF: i64 = 1;
const USER_BOT: i64 = 2;
const USER_SELF_BOT: i64 = 3;
const MEGAGROUP: i64 = 4;
const BROADCAST: i64 = 8;
const GIGAGROUP: i64 = 12;

pub struct RusqliteSession {
    conn: Mutex<Connection>,
    cache: Mutex<SessionCache>,
}

struct SessionCache {
    home_dc: i32,
    dc_options: HashMap<i32, DcOption>,
}

impl RusqliteSession {
    pub fn open<P: AsRef<Path>>(path: P) -> Result<Self> {
        let conn = Connection::open(path.as_ref()).with_context(|| {
            format!(
                "failed to open Telegram session database at {}",
                path.as_ref().display()
            )
        })?;
        init_schema(&conn)?;

        let defaults = SessionData::default();
        let home_dc = conn
            .query_row("SELECT dc_id FROM dc_home LIMIT 1", [], |row| row.get(0))
            .optional()?
            .unwrap_or(defaults.home_dc);
        let mut dc_options = defaults.dc_options;

        let mut stmt = conn.prepare("SELECT dc_id, ipv4, ipv6, auth_key FROM dc_option")?;
        let rows = stmt.query_map([], map_dc_option)?;
        for row in rows {
            let option = row?;
            dc_options.insert(option.id, option);
        }
        drop(stmt);

        Ok(Self {
            conn: Mutex::new(conn),
            cache: Mutex::new(SessionCache {
                home_dc,
                dc_options,
            }),
        })
    }
}

impl Session for RusqliteSession {
    fn home_dc_id(&self) -> i32 {
        self.cache.lock().unwrap().home_dc
    }

    fn set_home_dc_id(&self, dc_id: i32) -> BoxFuture<'_, ()> {
        {
            self.cache.lock().unwrap().home_dc = dc_id;
            let conn = self.conn.lock().unwrap();
            conn.execute("DELETE FROM dc_home", []).unwrap();
            conn.execute("INSERT INTO dc_home VALUES (?1)", [dc_id])
                .unwrap();
        }
        Box::pin(async {})
    }

    fn dc_option(&self, dc_id: i32) -> Option<DcOption> {
        self.cache.lock().unwrap().dc_options.get(&dc_id).cloned()
    }

    fn set_dc_option(&self, dc_option: &DcOption) -> BoxFuture<'_, ()> {
        {
            self.cache
                .lock()
                .unwrap()
                .dc_options
                .insert(dc_option.id, dc_option.clone());

            let auth_key = dc_option.auth_key.map(Vec::from);
            let conn = self.conn.lock().unwrap();
            conn.execute(
                "INSERT OR REPLACE INTO dc_option VALUES (?1, ?2, ?3, ?4)",
                params![
                    dc_option.id,
                    dc_option.ipv4.to_string(),
                    dc_option.ipv6.to_string(),
                    auth_key
                ],
            )
            .unwrap();
        }
        Box::pin(async {})
    }

    fn peer(&self, peer: PeerId) -> BoxFuture<'_, Option<PeerInfo>> {
        let result = {
            let conn = self.conn.lock().unwrap();
            if peer.kind() == PeerKind::UserSelf {
                conn.query_row(
                    "SELECT peer_id, hash, subtype FROM peer_info WHERE subtype & ?1 != 0 LIMIT 1",
                    [USER_SELF],
                    |row| map_peer_info(row, peer),
                )
                .optional()
                .unwrap()
            } else {
                conn.query_row(
                    "SELECT peer_id, hash, subtype FROM peer_info WHERE peer_id = ?1 LIMIT 1",
                    [peer.bot_api_dialog_id()],
                    |row| map_peer_info(row, peer),
                )
                .optional()
                .unwrap()
            }
        };
        Box::pin(async move { result })
    }

    fn cache_peer(&self, peer: &PeerInfo) -> BoxFuture<'_, ()> {
        {
            let subtype = peer_subtype(peer);
            let auth = peer.auth().map(|auth| auth.hash());
            let conn = self.conn.lock().unwrap();
            conn.execute(
                "INSERT OR REPLACE INTO peer_info VALUES (?1, ?2, ?3)",
                params![peer.id().bot_api_dialog_id(), auth, subtype],
            )
            .unwrap();
        }
        Box::pin(async {})
    }

    fn updates_state(&self) -> BoxFuture<'_, UpdatesState> {
        let state = {
            let conn = self.conn.lock().unwrap();
            let mut state = conn
                .query_row(
                    "SELECT pts, qts, date, seq FROM update_state LIMIT 1",
                    [],
                    |row| {
                        Ok(UpdatesState {
                            pts: row.get(0)?,
                            qts: row.get(1)?,
                            date: row.get(2)?,
                            seq: row.get(3)?,
                            channels: Vec::new(),
                        })
                    },
                )
                .optional()
                .unwrap()
                .unwrap_or_default();

            let mut stmt = conn
                .prepare("SELECT peer_id, pts FROM channel_state")
                .unwrap();
            let channels = stmt
                .query_map([], |row| {
                    Ok(ChannelState {
                        id: row.get(0)?,
                        pts: row.get(1)?,
                    })
                })
                .unwrap()
                .collect::<rusqlite::Result<Vec<_>>>()
                .unwrap();
            state.channels = channels;
            state
        };
        Box::pin(async move { state })
    }

    fn set_update_state(&self, update: UpdateState) -> BoxFuture<'_, ()> {
        {
            let mut conn = self.conn.lock().unwrap();
            let tx = conn.transaction().unwrap();

            match update {
                UpdateState::All(updates_state) => {
                    tx.execute("DELETE FROM update_state", []).unwrap();
                    tx.execute(
                        "INSERT INTO update_state VALUES (?1, ?2, ?3, ?4)",
                        params![
                            updates_state.pts,
                            updates_state.qts,
                            updates_state.date,
                            updates_state.seq
                        ],
                    )
                    .unwrap();

                    tx.execute("DELETE FROM channel_state", []).unwrap();
                    for channel in updates_state.channels {
                        tx.execute(
                            "INSERT INTO channel_state VALUES (?1, ?2)",
                            params![channel.id, channel.pts],
                        )
                        .unwrap();
                    }
                }
                UpdateState::Primary { pts, date, seq } => {
                    upsert_update_state(&tx).unwrap();
                    tx.execute(
                        "UPDATE update_state SET pts = ?1, date = ?2, seq = ?3",
                        params![pts, date, seq],
                    )
                    .unwrap();
                }
                UpdateState::Secondary { qts } => {
                    upsert_update_state(&tx).unwrap();
                    tx.execute("UPDATE update_state SET qts = ?1", [qts])
                        .unwrap();
                }
                UpdateState::Channel { id, pts } => {
                    tx.execute(
                        "INSERT OR REPLACE INTO channel_state VALUES (?1, ?2)",
                        params![id, pts],
                    )
                    .unwrap();
                }
            }

            tx.commit().unwrap();
        }
        Box::pin(async {})
    }
}

fn init_schema(conn: &Connection) -> Result<()> {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS dc_home (
            dc_id INTEGER NOT NULL,
            PRIMARY KEY(dc_id)
        );
        CREATE TABLE IF NOT EXISTS dc_option (
            dc_id INTEGER NOT NULL,
            ipv4 TEXT NOT NULL,
            ipv6 TEXT NOT NULL,
            auth_key BLOB,
            PRIMARY KEY(dc_id)
        );
        CREATE TABLE IF NOT EXISTS peer_info (
            peer_id INTEGER NOT NULL,
            hash INTEGER,
            subtype INTEGER,
            PRIMARY KEY(peer_id)
        );
        CREATE TABLE IF NOT EXISTS update_state (
            pts INTEGER NOT NULL,
            qts INTEGER NOT NULL,
            date INTEGER NOT NULL,
            seq INTEGER NOT NULL
        );
        CREATE TABLE IF NOT EXISTS channel_state (
            peer_id INTEGER NOT NULL,
            pts INTEGER NOT NULL,
            PRIMARY KEY(peer_id)
        );",
    )?;
    Ok(())
}

fn map_dc_option(row: &rusqlite::Row<'_>) -> rusqlite::Result<DcOption> {
    let auth_key = row
        .get::<_, Option<Vec<u8>>>(3)?
        .map(|auth_key| {
            auth_key.try_into().map_err(|value: Vec<u8>| {
                rusqlite::Error::FromSqlConversionFailure(
                    3,
                    Type::Blob,
                    Box::new(std::io::Error::new(
                        std::io::ErrorKind::InvalidData,
                        format!("stored auth key must be 256 bytes, got {}", value.len()),
                    )),
                )
            })
        })
        .transpose()?;

    let ipv4_text = row.get::<_, String>(1)?;
    let ipv6_text = row.get::<_, String>(2)?;

    Ok(DcOption {
        id: row.get(0)?,
        ipv4: ipv4_text.parse::<SocketAddrV4>().map_err(|err| {
            rusqlite::Error::FromSqlConversionFailure(1, Type::Text, Box::new(err))
        })?,
        ipv6: ipv6_text.parse::<SocketAddrV6>().map_err(|err| {
            rusqlite::Error::FromSqlConversionFailure(2, Type::Text, Box::new(err))
        })?,
        auth_key,
    })
}

fn map_peer_info(row: &rusqlite::Row<'_>, requested: PeerId) -> rusqlite::Result<PeerInfo> {
    let peer_id = row.get::<_, i64>(0)?;
    let auth = row.get::<_, Option<i64>>(1)?.map(PeerAuth::from_hash);
    let subtype = row.get::<_, Option<i64>>(2)?;

    Ok(match requested.kind() {
        PeerKind::User | PeerKind::UserSelf => PeerInfo::User {
            id: PeerId::user_unchecked(peer_id).bare_id(),
            auth,
            bot: subtype.map(|subtype| subtype & USER_BOT != 0),
            is_self: subtype.map(|subtype| subtype & USER_SELF != 0),
        },
        PeerKind::Chat => PeerInfo::Chat {
            id: requested.bare_id(),
        },
        PeerKind::Channel => PeerInfo::Channel {
            id: requested.bare_id(),
            auth,
            kind: subtype.and_then(channel_kind_from_subtype),
        },
    })
}

fn peer_subtype(peer: &PeerInfo) -> Option<i64> {
    match peer {
        PeerInfo::User { bot, is_self, .. } => {
            match (bot.unwrap_or_default(), is_self.unwrap_or_default()) {
                (true, true) => Some(USER_SELF_BOT),
                (true, false) => Some(USER_BOT),
                (false, true) => Some(USER_SELF),
                (false, false) => None,
            }
        }
        PeerInfo::Chat { .. } => None,
        PeerInfo::Channel { kind, .. } => kind.map(|kind| match kind {
            ChannelKind::Megagroup => MEGAGROUP,
            ChannelKind::Broadcast => BROADCAST,
            ChannelKind::Gigagroup => GIGAGROUP,
        }),
    }
}

fn channel_kind_from_subtype(subtype: i64) -> Option<ChannelKind> {
    if subtype & GIGAGROUP == GIGAGROUP {
        Some(ChannelKind::Gigagroup)
    } else if subtype & BROADCAST != 0 {
        Some(ChannelKind::Broadcast)
    } else if subtype & MEGAGROUP != 0 {
        Some(ChannelKind::Megagroup)
    } else {
        None
    }
}

fn upsert_update_state(tx: &rusqlite::Transaction<'_>) -> Result<()> {
    let exists = tx
        .query_row("SELECT 1 FROM update_state LIMIT 1", [], |_| Ok(()))
        .optional()?
        .is_some();
    if !exists {
        tx.execute("INSERT INTO update_state VALUES (0, 0, 0, 0)", [])?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use grammers_session::types::{PeerAuth, PeerId};

    use super::*;

    #[tokio::test]
    async fn persists_self_peer_without_libsql() {
        let session = RusqliteSession::open(":memory:").unwrap();
        assert_eq!(session.peer(PeerId::self_user()).await, None);

        let peer = PeerInfo::User {
            id: 1,
            auth: Some(PeerAuth::from_hash(42)),
            bot: Some(false),
            is_self: Some(true),
        };
        session.cache_peer(&peer).await;

        assert_eq!(session.peer(PeerId::self_user()).await, Some(peer.clone()));
        assert_eq!(session.peer(PeerId::user_unchecked(1)).await, Some(peer));
    }

    #[test]
    fn rejects_invalid_auth_key_size() {
        let conn = Connection::open_in_memory().unwrap();
        init_schema(&conn).unwrap();
        conn.execute(
            "INSERT INTO dc_option VALUES (1, '149.154.175.50:443', '[2001:67c:4e8:f002::a]:443', ?1)",
            [vec![1_u8; 8]],
        )
        .unwrap();

        let row_result = conn.query_row(
            "SELECT dc_id, ipv4, ipv6, auth_key FROM dc_option",
            [],
            map_dc_option,
        );

        assert!(row_result.is_err());
    }

    #[test]
    fn update_state_upsert_keeps_existing_secondary_state() {
        let session = RusqliteSession::open(":memory:").unwrap();
        futures::executor::block_on(session.set_update_state(UpdateState::Secondary { qts: 7 }));
        futures::executor::block_on(session.set_update_state(UpdateState::Primary {
            pts: 1,
            date: 2,
            seq: 3,
        }));

        let state = futures::executor::block_on(session.updates_state());
        assert_eq!(state.pts, 1);
        assert_eq!(state.qts, 7);
        assert_eq!(state.date, 2);
        assert_eq!(state.seq, 3);
    }
}
