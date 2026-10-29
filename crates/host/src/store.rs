use crate::models::*;
use anyhow::{anyhow, bail, Result};
use rusqlite::{params, Connection, OptionalExtension};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::{path::Path, sync::Mutex};
use uuid::Uuid;

pub struct Store(pub(crate) Mutex<Connection>);
fn digest(value: &str) -> String {
    hex::encode(Sha256::digest(value.as_bytes()))
}

impl Store {
    pub fn open(path: &Path) -> Result<Self> {
        let mut c = Connection::open(path)?;
        c.pragma_update(None, "journal_mode", "WAL")?;
        c.pragma_update(None, "foreign_keys", "ON")?;
        c.busy_timeout(std::time::Duration::from_secs(5))?;
        let version: i64 = c.pragma_query_value(None, "user_version", |r| r.get(0))?;
        if version > 1 {
            bail!("database schema newer than this binary");
        }
        let tx = c.transaction()?;
        tx.execute_batch("CREATE TABLE IF NOT EXISTS metadata(key TEXT PRIMARY KEY,value TEXT NOT NULL);
          CREATE TABLE IF NOT EXISTS devices(id TEXT PRIMARY KEY,name TEXT NOT NULL,role TEXT NOT NULL,token_hash TEXT NOT NULL UNIQUE,revoked INTEGER NOT NULL DEFAULT 0);
          CREATE TABLE IF NOT EXISTS pairing(hash TEXT PRIMARY KEY,expires INTEGER NOT NULL,role TEXT NOT NULL);
          CREATE TABLE IF NOT EXISTS sessions(id TEXT PRIMARY KEY,json TEXT NOT NULL);
          CREATE TABLE IF NOT EXISTS events(sequence INTEGER PRIMARY KEY AUTOINCREMENT,session_id TEXT NOT NULL REFERENCES sessions(id),kind TEXT NOT NULL,payload TEXT NOT NULL,created_at INTEGER NOT NULL);
          CREATE INDEX IF NOT EXISTS session_events ON events(session_id,sequence);
          CREATE TABLE IF NOT EXISTS requests(actor TEXT NOT NULL,id TEXT NOT NULL,operation TEXT NOT NULL,body_hash TEXT NOT NULL,result TEXT NOT NULL,PRIMARY KEY(actor,id));
          CREATE TABLE IF NOT EXISTS audit(sequence INTEGER PRIMARY KEY AUTOINCREMENT,actor TEXT NOT NULL,action TEXT NOT NULL,target TEXT NOT NULL,created_at INTEGER NOT NULL);
          CREATE TABLE IF NOT EXISTS push_registrations(device_id TEXT PRIMARY KEY REFERENCES devices(id),token TEXT NOT NULL,environment TEXT NOT NULL);
          CREATE TABLE IF NOT EXISTS automation_devices(device_id TEXT PRIMARY KEY REFERENCES devices(id),session_id TEXT NOT NULL);
          PRAGMA user_version=1;")?;
        tx.execute(
            "INSERT OR IGNORE INTO metadata VALUES ('host_id',?1)",
            [Uuid::new_v4().to_string()],
        )?;
        tx.execute(
            "INSERT OR IGNORE INTO metadata VALUES ('member_id',?1)",
            [Uuid::new_v4().to_string()],
        )?;
        crate::certificates::migrate(&tx)?;
        crate::quotas::migrate(&tx)?;
        crate::transfer::migrate(&tx)?;
        crate::team::migrate(&tx)?;
        tx.commit()?;
        Ok(Self(Mutex::new(c)))
    }
    pub fn host_id(&self) -> Result<String> {
        Ok(self.0.lock().unwrap().query_row(
            "SELECT value FROM metadata WHERE key='host_id'",
            [],
            |r| r.get(0),
        )?)
    }
    pub fn issue_pairing(&self, role: &str) -> Result<String> {
        if !["administrator", "operator", "viewer"].contains(&role) {
            bail!("invalid role");
        }
        let secret = format!("{}{}", Uuid::new_v4().simple(), Uuid::new_v4().simple());
        self.0.lock().unwrap().execute(
            "INSERT INTO pairing VALUES (?1,?2,?3)",
            params![digest(&secret), now() + 300, role],
        )?;
        Ok(secret)
    }
    pub fn pair(&self, secret: &str, name: &str) -> Result<Value> {
        if name.trim().is_empty() || name.len() > 128 {
            bail!("device name must be 1–128 characters");
        }
        let mut c = self.0.lock().unwrap();
        let tx = c.transaction()?;
        let role: String = tx
            .query_row(
                "SELECT role FROM pairing WHERE hash=?1 AND expires>?2",
                params![digest(secret), now()],
                |r| r.get(0),
            )
            .optional()?
            .ok_or_else(|| anyhow!("invalid or expired enrollment secret"))?;
        let mapped:Option<String>=tx.query_row("SELECT pm.member_id FROM pairing_members pm JOIN members m ON m.id=pm.member_id WHERE pm.hash=?1 AND m.revoked=0",[digest(secret)],|r|r.get(0)).optional()?;
        let has_mapping: bool = tx.query_row(
            "SELECT EXISTS(SELECT 1 FROM pairing_members WHERE hash=?1)",
            [digest(secret)],
            |r| r.get(0),
        )?;
        if has_mapping && mapped.is_none() {
            bail!("invalid or revoked member enrollment");
        }
        tx.execute(
            "DELETE FROM pairing_members WHERE hash=?1",
            [digest(secret)],
        )?;
        tx.execute("DELETE FROM pairing WHERE hash=?1", [digest(secret)])?;
        let id = Uuid::new_v4().to_string();
        let token = format!("{}{}", Uuid::new_v4().simple(), Uuid::new_v4().simple());
        tx.execute(
            "INSERT INTO devices(id,name,role,token_hash) VALUES (?1,?2,?3,?4)",
            params![id, name, role, digest(&token)],
        )?;
        if let Some(member) = mapped {
            tx.execute(
                "INSERT INTO device_members(device_id,member_id) VALUES(?1,?2)",
                params![id, member],
            )?;
        } else {
            crate::team::attach_primary_device(&tx, &id)?;
        }
        tx.execute(
            "INSERT INTO audit(actor,action,target,created_at) VALUES (?1,'pair',?1,?2)",
            params![id, now()],
        )?;
        tx.commit()?;
        Ok(json!({"device_id":id,"token":token,"role":role}))
    }
    pub fn authenticate(&self, token: &str) -> Result<Device> {
        self.0.lock().unwrap().query_row("SELECT d.id,d.name,CASE WHEN d.role='viewer' OR m.role='viewer' THEN 'viewer' WHEN d.role='operator' OR m.role='operator' THEN 'operator' ELSE 'administrator' END,d.revoked,m.id FROM devices d JOIN device_members dm ON dm.device_id=d.id JOIN members m ON m.id=dm.member_id WHERE d.token_hash=?1 AND d.revoked=0 AND m.revoked=0",[digest(token)],|r|Ok(Device{id:r.get(0)?,name:r.get(1)?,role:r.get(2)?,revoked:r.get(3)?,member_id:r.get(4)?})).map_err(|_|anyhow!("unauthorized device"))
    }
    pub fn devices(&self) -> Result<Value> {
        let c = self.0.lock().unwrap();
        let mut s=c.prepare("SELECT d.id,d.name,CASE WHEN d.role='viewer' OR m.role='viewer' THEN 'viewer' WHEN d.role='operator' OR m.role='operator' THEN 'operator' ELSE 'administrator' END,d.revoked OR m.revoked,m.id FROM devices d JOIN device_members dm ON dm.device_id=d.id JOIN members m ON m.id=dm.member_id")?;
        let rows = s
            .query_map([], |r| {
                Ok(Device {
                    id: r.get(0)?,
                    name: r.get(1)?,
                    role: r.get(2)?,
                    revoked: r.get(3)?,
                    member_id: r.get(4)?,
                })
            })?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(serde_json::to_value(rows)?)
    }
    pub fn device_active(&self, id: &str) -> bool {
        self.0
            .lock()
            .unwrap()
            .query_row(
                "SELECT 1 FROM devices d JOIN device_members dm ON dm.device_id=d.id JOIN members m ON m.id=dm.member_id WHERE d.id=?1 AND d.revoked=0 AND m.revoked=0 AND (NOT EXISTS(SELECT 1 FROM device_certificates c WHERE c.device_id=d.id) OR EXISTS(SELECT 1 FROM device_certificates c WHERE c.device_id=d.id AND c.expires>unixepoch()))",
                [id],
                |r| r.get::<_, i64>(0),
            )
            .optional()
            .ok()
            .flatten()
            .is_some()
    }
    pub fn is_automation_device(&self, id: &str) -> Result<bool> {
        Ok(self.0.lock().unwrap().query_row(
            "SELECT EXISTS(SELECT 1 FROM automation_devices WHERE device_id=?1)",
            [id],
            |r| r.get(0),
        )?)
    }
    pub fn mark_automation(&self, device: &str, session: &str) -> Result<()> {
        self.0.lock().unwrap().execute(
            "INSERT INTO automation_devices VALUES (?1,?2)",
            params![device, session],
        )?;
        Ok(())
    }
    pub fn revoke_abandoned_automation(&self) -> Result<()> {
        self.0.lock().unwrap().execute(
            "UPDATE devices SET revoked=1 WHERE id IN (SELECT device_id FROM automation_devices)",
            [],
        )?;
        Ok(())
    }
    pub fn revoke(&self, actor: &str, id: &str) -> Result<()> {
        let mut c = self.0.lock().unwrap();
        let tx = c.transaction()?;
        if actor == id {
            bail!("cannot revoke the current administrator device");
        }
        if tx.execute("UPDATE devices SET revoked=1 WHERE id=?1", [id])? == 0 {
            bail!("device not found");
        }
        tx.execute(
            "INSERT INTO audit(actor,action,target,created_at) VALUES (?1,'revoke',?2,?3)",
            params![actor, id, now()],
        )?;
        tx.commit()?;
        Ok(())
    }
    pub fn list(&self) -> Result<Vec<Session>> {
        let c = self.0.lock().unwrap();
        let mut s = c.prepare("SELECT json FROM sessions ORDER BY rowid DESC")?;
        let rows = s
            .query_map([], |r| r.get::<_, String>(0))?
            .collect::<Result<Vec<_>, _>>()?;
        rows.iter().map(|s| Ok(serde_json::from_str(s)?)).collect()
    }
    pub fn get(&self, id: &str) -> Result<Session> {
        let raw: String = self
            .0
            .lock()
            .unwrap()
            .query_row("SELECT json FROM sessions WHERE id=?1", [id], |r| r.get(0))
            .optional()?
            .ok_or_else(|| anyhow!("session not found"))?;
        Ok(serde_json::from_str(&raw)?)
    }
    pub fn save(&self, s: &Session) -> Result<()> {
        self.0.lock().unwrap().execute(
            "INSERT INTO sessions VALUES (?1,?2) ON CONFLICT(id) DO UPDATE SET json=excluded.json",
            params![s.id, serde_json::to_string(s)?],
        )?;
        Ok(())
    }
    pub fn append(&self, id: &str, kind: &str, payload: Value) -> Result<Event> {
        let mut c = self.0.lock().unwrap();
        let tx = c.transaction()?;
        let at = now();
        crate::quotas::reserve_event(&tx, kind, &payload)?;
        tx.execute(
            "INSERT INTO events(session_id,kind,payload,created_at) VALUES(?1,?2,?3,?4)",
            params![id, kind, payload.to_string(), at],
        )?;
        let sequence = tx.last_insert_rowid() as u64;
        tx.commit()?;
        Ok(Event {
            sequence,
            session_id: id.into(),
            kind: kind.into(),
            payload,
            created_at: at,
        })
    }
    pub fn events(&self, id: &str, after: u64) -> Result<Vec<Event>> {
        let boundary = self.retention_boundary(id)?;
        if after > 0 && boundary.is_some_and(|v| after < v) {
            bail!("history_gap: the requested event cursor expired; restart reading from zero");
        }
        let c = self.0.lock().unwrap();
        let mut q=c.prepare("SELECT sequence,kind,payload,created_at FROM events WHERE session_id=?1 AND sequence>?2 ORDER BY sequence LIMIT 1000")?;
        let rows = q
            .query_map(params![id, after], |r| {
                Ok((
                    r.get::<_, u64>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, String>(2)?,
                    r.get::<_, i64>(3)?,
                ))
            })?
            .collect::<Result<Vec<_>, _>>()?;
        let mut retained = rows
            .into_iter()
            .map(|(sequence, kind, payload, created_at)| {
                Ok(Event {
                    sequence,
                    session_id: id.into(),
                    kind,
                    payload: serde_json::from_str(&payload)?,
                    created_at,
                })
            })
            .collect::<Result<Vec<_>>>()?;
        if after == 0 {
            if let Some(sequence) = boundary {
                retained.insert(0,Event{sequence,session_id:id.into(),kind:"history_gap".into(),payload:json!({"through_sequence":sequence,"message":"Earlier events expired under the retention policy."}),created_at:now()});
            }
        }
        Ok(retained)
    }
    pub fn previous(&self, actor: &str, id: &str, op: &str, body: &Value) -> Result<Option<Value>> {
        if id.is_empty() || id.len() > 128 {
            bail!("request_id must be 1–128 characters");
        }
        let row: Option<(String, String, String)> = self
            .0
            .lock()
            .unwrap()
            .query_row(
                "SELECT operation,body_hash,result FROM requests WHERE actor=?1 AND id=?2",
                params![actor, id],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .optional()?;
        match row {
            None => Ok(None),
            Some((operation, hash, result)) => {
                if operation != op || hash != digest(&body.to_string()) {
                    bail!("request_id already used with different input");
                }
                Ok(Some(serde_json::from_str(&result)?))
            }
        }
    }
    pub fn remember(
        &self,
        actor: &str,
        id: &str,
        op: &str,
        body: &Value,
        result: &Value,
    ) -> Result<()> {
        let mut c = self.0.lock().unwrap();
        let tx = c.transaction()?;
        crate::quotas::reserve_request(&tx)?;
        tx.execute(
            "INSERT INTO requests VALUES (?1,?2,?3,?4,?5)",
            params![actor, id, op, digest(&body.to_string()), result.to_string()],
        )?;
        tx.execute(
            "INSERT INTO audit(actor,action,target,created_at) VALUES (?1,?2,?3,?4)",
            params![
                actor,
                op,
                result.get("id").and_then(Value::as_str).unwrap_or(""),
                now()
            ],
        )?;
        tx.commit()?;
        Ok(())
    }
    // Commit acceptance before spawning a provider. A crash after this transaction is
    // reported as uncertain on startup; accepted commands are never auto-replayed.
    #[allow(clippy::too_many_arguments)] // one atomic acceptance transaction owns all these records
    pub fn accept(
        &self,
        actor: &str,
        id: &str,
        op: &str,
        body: &Value,
        result: &Value,
        session: &Session,
        kind: &str,
        payload: Value,
    ) -> Result<Event> {
        let mut c = self.0.lock().unwrap();
        let tx = c.transaction()?;
        let at = now();
        crate::quotas::check_acceptance(&tx, session)?;
        crate::quotas::reserve_request(&tx)?;
        crate::quotas::reserve_event(&tx, kind, &payload)?;
        tx.execute(
            "INSERT INTO sessions VALUES (?1,?2) ON CONFLICT(id) DO UPDATE SET json=excluded.json",
            params![session.id, serde_json::to_string(session)?],
        )?;
        tx.execute(
            "INSERT INTO events(session_id,kind,payload,created_at) VALUES (?1,?2,?3,?4)",
            params![session.id, kind, payload.to_string(), at],
        )?;
        let sequence = tx.last_insert_rowid() as u64;
        tx.execute(
            "INSERT INTO requests VALUES (?1,?2,?3,?4,?5)",
            params![actor, id, op, digest(&body.to_string()), result.to_string()],
        )?;
        tx.execute(
            "INSERT INTO audit(actor,action,target,created_at) VALUES (?1,?2,?3,?4)",
            params![actor, op, session.id, at],
        )?;
        tx.commit()?;
        Ok(Event {
            sequence,
            session_id: session.id.clone(),
            kind: kind.into(),
            payload,
            created_at: at,
        })
    }
    pub fn audit(&self) -> Result<Value> {
        let c = self.0.lock().unwrap();
        let mut s = c.prepare(
            "SELECT actor,action,target,created_at FROM audit ORDER BY sequence DESC LIMIT 500",
        )?;
        let rows=s.query_map([],|r|Ok(json!({"actor":r.get::<_,String>(0)?,"action":r.get::<_,String>(1)?,"target":r.get::<_,String>(2)?,"created_at":r.get::<_,i64>(3)?})))?.collect::<Result<Vec<_>,_>>()?;
        Ok(json!(rows))
    }
    pub fn register_push(&self, device: &str, token: &str, environment: &str) -> Result<()> {
        if !(64..=200).contains(&token.len())
            || !token.bytes().all(|b| b.is_ascii_hexdigit())
            || !["sandbox", "production"].contains(&environment)
        {
            bail!("invalid push registration");
        }
        self.0.lock().unwrap().execute("INSERT INTO push_registrations VALUES (?1,?2,?3) ON CONFLICT(device_id) DO UPDATE SET token=excluded.token,environment=excluded.environment",params![device,token,environment])?;
        Ok(())
    }
    pub fn push_targets(&self) -> Result<Vec<Value>> {
        let c = self.0.lock().unwrap();
        let mut s=c.prepare("SELECT p.token,p.environment FROM push_registrations p JOIN devices d ON p.device_id=d.id WHERE d.revoked=0")?;
        let targets = s
            .query_map([], |r| {
                Ok(json!({"apns_token":r.get::<_,String>(0)?,"environment":r.get::<_,String>(1)?}))
            })?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(targets)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn store() -> Store {
        Store::open(Path::new(":memory:")).unwrap()
    }
    #[test]
    fn pairing_is_single_use_and_revocable() {
        let s = store();
        let secret = s.issue_pairing("operator").unwrap();
        let p = s.pair(&secret, "Phone").unwrap();
        assert!(s.pair(&secret, "Other").is_err());
        let token = p["token"].as_str().unwrap();
        assert_eq!(s.authenticate(token).unwrap().role, "operator");
        s.revoke("admin", p["device_id"].as_str().unwrap()).unwrap();
        assert!(s.authenticate(token).is_err());
    }
    #[test]
    fn idempotency_rejects_changed_input() {
        let s = store();
        s.remember("a", "r", "create", &json!({"a":1}), &json!({"id":"s"}))
            .unwrap();
        assert!(s
            .previous("a", "r", "create", &json!({"a":1}))
            .unwrap()
            .is_some());
        assert!(s.previous("a", "r", "create", &json!({"a":2})).is_err());
        assert!(s
            .previous("b", "r", "create", &json!({"a":1}))
            .unwrap()
            .is_none());
    }
    #[test]
    fn ordered_event_replay() {
        let s = store();
        let session = Session {
            id: "s".into(),
            provider: "codex".into(),
            workspace: "/tmp".into(),
            title: "Test".into(),
            state: "idle".into(),
            owner: "a".into(),
            created_at: now(),
            provider_session_id: None,
            shared: false,
            trusted: false,
            execution_target: Default::default(),
        };
        s.save(&session).unwrap();
        let e = s.append("s", "message", json!({"text":"one"})).unwrap();
        s.append("s", "message", json!({"text":"two"})).unwrap();
        let replay = s.events("s", e.sequence).unwrap();
        assert_eq!(replay.len(), 1);
        assert_eq!(replay[0].payload["text"], "two");
    }
}
