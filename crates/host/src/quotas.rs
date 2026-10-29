use crate::{
    models::{now, Device, Session},
    store::Store,
};
use anyhow::{bail, Result};
use rusqlite::{params, Connection, OptionalExtension, Transaction};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Limits {
    pub max_members: u64,
    pub max_sessions: u64,
    pub max_concurrent_runs: u64,
    pub max_event_bytes: u64,
    pub max_requests: u64,
    pub max_transfer_bytes: u64,
    pub max_staging_bytes: u64,
    pub retention_seconds: i64,
}
impl Default for Limits {
    fn default() -> Self {
        Self {
            max_members: 1000,
            max_sessions: 10000,
            max_concurrent_runs: 20,
            max_event_bytes: 256 * 1024 * 1024,
            max_requests: 1_000_000,
            max_transfer_bytes: 64 * 1024 * 1024,
            max_staging_bytes: 512 * 1024 * 1024,
            retention_seconds: 30 * 86400,
        }
    }
}
impl Limits {
    pub fn validate(&self) -> Result<()> {
        if !(1..=10000).contains(&self.max_members)
            || !(1..=1_000_000).contains(&self.max_sessions)
            || !(1..=100).contains(&self.max_concurrent_runs)
            || !(1024 * 1024..=64 * 1024 * 1024 * 1024).contains(&self.max_event_bytes)
            || !(1..=10_000_000).contains(&self.max_requests)
            || !(1..=1024 * 1024 * 1024).contains(&self.max_transfer_bytes)
            || self.max_staging_bytes < self.max_transfer_bytes
            || self.max_staging_bytes > 16 * 1024 * 1024 * 1024
            || !(60..=10 * 365 * 86400).contains(&self.retention_seconds)
        {
            bail!("invalid quota or retention configuration");
        }
        Ok(())
    }
}
pub fn migrate(tx: &Transaction<'_>) -> Result<()> {
    tx.execute_batch("CREATE TABLE IF NOT EXISTS resource_policy(id INTEGER PRIMARY KEY CHECK(id=1),json TEXT NOT NULL);
 CREATE TABLE IF NOT EXISTS resource_usage(name TEXT PRIMARY KEY,value INTEGER NOT NULL CHECK(value>=0));
 CREATE TABLE IF NOT EXISTS retention_boundaries(session_id TEXT PRIMARY KEY,through_sequence INTEGER NOT NULL);
 CREATE TABLE IF NOT EXISTS transfer_reservations(id TEXT PRIMARY KEY,session_id TEXT NOT NULL,member_id TEXT NOT NULL,bytes INTEGER NOT NULL,created_at INTEGER NOT NULL);")?;
    tx.execute(
        "INSERT OR IGNORE INTO resource_policy(id,json) VALUES(1,?1)",
        [serde_json::to_string(&Limits::default())?],
    )?;
    tx.execute("INSERT OR IGNORE INTO resource_usage(name,value) SELECT 'event_bytes',COALESCE(SUM(LENGTH(CAST(payload AS BLOB))+LENGTH(CAST(kind AS BLOB))),0) FROM events",[])?;
    Ok(())
}
pub fn limits(c: &Connection) -> Result<Limits> {
    let value: Option<String> = c
        .query_row("SELECT json FROM resource_policy WHERE id=1", [], |r| {
            r.get(0)
        })
        .optional()?;
    let limits = match value {
        Some(value) => serde_json::from_str(&value)?,
        None => Limits::default(),
    };
    Ok(limits)
}
fn count(c: &Connection, sql: &str) -> Result<u64> {
    let count: i64 = c.query_row(sql, [], |r| r.get(0))?;
    Ok(count.max(0) as u64)
}
pub fn check_acceptance(tx: &Transaction<'_>, s: &Session) -> Result<()> {
    let policy = limits(tx)?;
    let exists: bool = tx.query_row(
        "SELECT EXISTS(SELECT 1 FROM sessions WHERE id=?1)",
        [&s.id],
        |r| r.get(0),
    )?;
    if !exists && count(tx, "SELECT COUNT(*) FROM sessions")? >= policy.max_sessions {
        bail!("session quota reached; prune retained archived sessions or raise the limit");
    }
    if ["running", "interrupting"].contains(&s.state.as_str()) {
        let running:i64=tx.query_row("SELECT COUNT(*) FROM sessions WHERE id<>?1 AND json_extract(json,'$.state') IN('running','interrupting')",[&s.id],|r|r.get(0))?;
        if running.max(0) as u64 >= policy.max_concurrent_runs {
            bail!("concurrent agent quota reached");
        }
    }
    Ok(())
}
pub fn reserve_request(tx: &Transaction<'_>) -> Result<()> {
    if count(tx, "SELECT COUNT(*) FROM requests")? >= limits(tx)?.max_requests {
        bail!("idempotency record quota reached; raise the limit rather than replaying recorded commands");
    }
    Ok(())
}
pub fn reserve_event(tx: &Transaction<'_>, kind: &str, payload: &Value) -> Result<()> {
    let bytes = (serde_json::to_vec(payload)?.len() + kind.len()) as u64;
    let critical = ["state", "error", "approval_resolved"].contains(&kind);
    if bytes > 4 * 1024 * 1024 || (critical && bytes > 8192) {
        bail!("event exceeds storage record size limit");
    }
    let policy = limits(tx)?;
    let used = count(
        tx,
        "SELECT value FROM resource_usage WHERE name='event_bytes'",
    )?;
    let ceiling = if critical {
        policy.max_event_bytes
    } else {
        policy
            .max_event_bytes
            .saturating_sub((policy.max_event_bytes / 16).min(1024 * 1024))
    };
    if used.checked_add(bytes).is_none_or(|value| value > ceiling) {
        bail!("event storage quota reached; execution must stop before accepting further output");
    }
    tx.execute(
        "UPDATE resource_usage SET value=value+?1 WHERE name='event_bytes'",
        [bytes as i64],
    )?;
    Ok(())
}
impl Store {
    pub fn resource_limits(&self) -> Result<Limits> {
        let c = self.0.lock().unwrap();
        limits(&c)
    }
    pub fn set_resource_limits(&self, actor: &Device, value: Limits) -> Result<()> {
        if actor.revoked || actor.role != "administrator" {
            bail!("forbidden: administrator required");
        }
        value.validate()?;
        let mut c = self.0.lock().unwrap();
        let tx = c.transaction()?;
        let usage=[(value.max_members,"SELECT COUNT(*) FROM members WHERE revoked=0"),(value.max_sessions,"SELECT COUNT(*) FROM sessions"),(value.max_requests,"SELECT COUNT(*) FROM requests"),(value.max_concurrent_runs,"SELECT COUNT(*) FROM sessions WHERE json_extract(json,'$.state') IN('running','interrupting')"),(value.max_event_bytes,"SELECT value FROM resource_usage WHERE name='event_bytes'"),(value.max_staging_bytes,"SELECT COALESCE(SUM(bytes),0) FROM transfer_reservations")];
        for (ceiling, sql) in usage {
            if count(&tx, sql)? > ceiling {
                bail!("quota cannot be reduced below current usage");
            }
        }
        tx.execute(
            "UPDATE resource_policy SET json=?1 WHERE id=1",
            [serde_json::to_string(&value)?],
        )?;
        tx.execute("INSERT INTO audit(actor,action,target,created_at) VALUES(?1,'resource-policy.update','worker',?2)",params![actor.id,now()])?;
        tx.commit()?;
        Ok(())
    }
    pub fn resource_usage(&self) -> Result<Value> {
        let c = self.0.lock().unwrap();
        Ok(
            json!({"event_bytes":count(&c,"SELECT value FROM resource_usage WHERE name='event_bytes'")?,"sessions":count(&c,"SELECT COUNT(*) FROM sessions")?,"requests":count(&c,"SELECT COUNT(*) FROM requests")?,"members":count(&c,"SELECT COUNT(*) FROM members WHERE revoked=0")?,"running":count(&c,"SELECT COUNT(*) FROM sessions WHERE json_extract(json,'$.state') IN('running','interrupting')")?,"staging_bytes":count(&c,"SELECT COALESCE(SUM(bytes),0) FROM transfer_reservations")?}),
        )
    }
    pub fn reserve_transfer(&self, actor: &Device, session_id: &str, bytes: u64) -> Result<String> {
        let mut c = self.0.lock().unwrap();
        let tx = c.transaction()?;
        let policy = limits(&tx)?;
        if count(&tx, "SELECT COUNT(*) FROM transfer_reservations")? >= 128
            || bytes > policy.max_transfer_bytes
            || count(
                &tx,
                "SELECT COALESCE(SUM(bytes),0) FROM transfer_reservations",
            )?
            .checked_add(bytes)
            .is_none_or(|used| used > policy.max_staging_bytes)
        {
            bail!("file transfer quota reached");
        }
        let id = uuid::Uuid::new_v4().to_string();
        tx.execute("INSERT INTO transfer_reservations(id,session_id,member_id,bytes,created_at) VALUES(?1,?2,?3,?4,?5)",params![id,session_id,actor.member_id,bytes as i64,now()])?;
        tx.commit()?;
        Ok(id)
    }
    pub fn release_transfer(&self, id: &str) -> Result<()> {
        self.0
            .lock()
            .unwrap()
            .execute("DELETE FROM transfer_reservations WHERE id=?1", [id])?;
        Ok(())
    }
    pub fn retention_boundary(&self, session_id: &str) -> Result<Option<u64>> {
        let c = self.0.lock().unwrap();
        Ok(c.query_row(
            "SELECT through_sequence FROM retention_boundaries WHERE session_id=?1",
            [session_id],
            |r| r.get::<_, i64>(0),
        )
        .optional()?
        .map(|v| v.max(0) as u64))
    }
    pub fn prune_retention(&self, actor: &Device) -> Result<Value> {
        if actor.revoked || actor.role != "administrator" {
            bail!("forbidden: administrator required");
        }
        let mut c = self.0.lock().unwrap();
        let tx = c.transaction()?;
        let cutoff = now() - limits(&tx)?.retention_seconds;
        let ids = {
            let mut q=tx.prepare("SELECT id FROM sessions WHERE json_extract(json,'$.state') NOT IN('running','interrupting') AND id NOT IN(SELECT session_id FROM transfer_reservations)")?;
            let result = q
                .query_map([], |r| r.get::<_, String>(0))?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            result
        };
        let mut deleted = 0u64;
        let mut reclaimed = 0i64;
        for id in &ids {
            let boundary: Option<i64> = tx.query_row(
                "SELECT MAX(sequence) FROM events WHERE session_id=?1 AND created_at<?2",
                params![id, cutoff],
                |r| r.get(0),
            )?;
            let bytes:i64=tx.query_row("SELECT COALESCE(SUM(LENGTH(CAST(payload AS BLOB))+LENGTH(CAST(kind AS BLOB))),0) FROM events WHERE session_id=?1 AND created_at<?2",params![id,cutoff],|r|r.get(0))?;
            if let Some(boundary) = boundary {
                tx.execute("INSERT INTO retention_boundaries(session_id,through_sequence) VALUES(?1,?2) ON CONFLICT(session_id) DO UPDATE SET through_sequence=MAX(through_sequence,excluded.through_sequence)",params![id,boundary])?;
            }
            deleted += tx.execute(
                "DELETE FROM events WHERE session_id=?1 AND created_at<?2",
                params![id, cutoff],
            )? as u64;
            reclaimed += bytes;
        }
        let archives = {
            let mut q=tx.prepare("SELECT id FROM sessions WHERE json_extract(json,'$.state')='archived' AND CAST(json_extract(json,'$.created_at') AS INTEGER)<?1 AND NOT EXISTS(SELECT 1 FROM events e WHERE e.session_id=sessions.id AND e.created_at>=?1) AND id NOT IN(SELECT session_id FROM transfer_reservations)")?;
            let values = q
                .query_map([cutoff], |r| r.get::<_, String>(0))?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            values
        };
        for id in &archives {
            tx.execute("DELETE FROM session_access WHERE session_id=?1", [id])?;
            tx.execute("UPDATE devices SET revoked=1 WHERE id IN(SELECT device_id FROM automation_devices WHERE session_id=?1)",[id])?;
            tx.execute("DELETE FROM automation_devices WHERE session_id=?1", [id])?;
            tx.execute("DELETE FROM events WHERE session_id=?1", [id])?;
            tx.execute("DELETE FROM sessions WHERE id=?1", [id])?;
        }
        tx.execute(
            "UPDATE resource_usage SET value=MAX(0,value-?1) WHERE name='event_bytes'",
            [reclaimed],
        )?;
        tx.execute(
            "DELETE FROM invitations WHERE expires<?1 AND consumed_at IS NULL",
            [cutoff],
        )?;
        tx.execute("INSERT INTO audit(actor,action,target,created_at) VALUES(?1,'retention.prune','worker',?2)",params![actor.id,now()])?;
        tx.commit()?;
        Ok(
            json!({"events_removed":deleted,"event_bytes_reclaimed":reclaimed,"archived_sessions_removed":archives.len(),"idempotency_records_preserved":true}),
        )
    }
}
use crate::{api::ApiError, core::Core};
use axum::{
    extract::State,
    routing::{get, post},
    Extension, Json, Router,
};
use std::sync::Arc;
pub fn router() -> Router<Arc<Core>> {
    Router::new()
        .route("/v1/limits", get(get_limits).post(set_limits))
        .route("/v1/retention/prune", post(prune))
        .route("/v1/resources", get(resources).post(set_resources))
}
async fn get_limits(
    State(c): State<Arc<Core>>,
    Extension(d): Extension<Device>,
) -> Result<Json<Value>, ApiError> {
    if d.role != "administrator" {
        return Err(anyhow::anyhow!("forbidden: administrator required").into());
    }
    Ok(Json(
        json!({"limits":c.store.resource_limits()?,"usage":c.store.resource_usage()?}),
    ))
}
async fn set_limits(
    State(c): State<Arc<Core>>,
    Extension(d): Extension<Device>,
    Json(limits): Json<Limits>,
) -> Result<Json<Value>, ApiError> {
    let _guard = c.mutation.lock().await;
    c.store.set_resource_limits(&d, limits)?;
    Ok(Json(
        json!({"limits":c.store.resource_limits()?,"usage":c.store.resource_usage()?}),
    ))
}
async fn prune(
    State(c): State<Arc<Core>>,
    Extension(d): Extension<Device>,
) -> Result<Json<Value>, ApiError> {
    let _guard = c.mutation.lock().await;
    Ok(Json(c.store.prune_retention(&d)?))
}

async fn resources(
    State(c): State<Arc<Core>>,
    Extension(d): Extension<Device>,
) -> Result<Json<Value>, ApiError> {
    Ok(Json(
        json!({"policy":c.store.resource_limits()?,"usage":c.store.resource_usage()?,"can_manage":d.role=="administrator"}),
    ))
}
#[derive(Deserialize)]
struct PolicyUpdate {
    policy: Limits,
}
async fn set_resources(
    State(c): State<Arc<Core>>,
    Extension(d): Extension<Device>,
    Json(v): Json<PolicyUpdate>,
) -> Result<Json<Value>, ApiError> {
    let _guard = c.mutation.lock().await;
    c.store.set_resource_limits(&d, v.policy)?;
    Ok(Json(
        json!({"policy":c.store.resource_limits()?,"usage":c.store.resource_usage()?,"can_manage":true}),
    ))
}
#[cfg(test)]
mod tests {
    use super::*;
    fn setup() -> (tempfile::TempDir, Store, Device) {
        let dir = tempfile::tempdir().unwrap();
        let s = Store::open(&dir.path().join("state.db")).unwrap();
        let pair = s
            .pair(&s.issue_pairing("administrator").unwrap(), "Owner")
            .unwrap();
        let d = s.authenticate(pair["token"].as_str().unwrap()).unwrap();
        (dir, s, d)
    }
    fn session(d: &Device) -> Session {
        Session {
            id: uuid::Uuid::new_v4().to_string(),
            provider: "codex".into(),
            workspace: "/fixture".into(),
            title: "Fixture".into(),
            state: "idle".into(),
            owner: d.member_id.clone(),
            created_at: now(),
            provider_session_id: None,
            shared: false,
            trusted: false,
            execution_target: Default::default(),
        }
    }
    fn accept(s: &Store, d: &Device, v: &Session, id: &str) -> Result<()> {
        s.accept(
            &d.id,
            id,
            "create",
            &json!({}),
            &json!(v),
            v,
            "state",
            json!({"state":v.state}),
        )?;
        Ok(())
    }
    #[test]
    fn session_running_limits_and_failed_acceptance_are_atomic() {
        let (_dir, s, d) = setup();
        let mut l = s.resource_limits().unwrap();
        l.max_sessions = 1;
        l.max_concurrent_runs = 1;
        s.set_resource_limits(&d, l.clone()).unwrap();
        let mut first = session(&d);
        first.state = "running".into();
        accept(&s, &d, &first, "one").unwrap();
        let mut second = session(&d);
        assert!(accept(&s, &d, &second, "two").is_err());
        assert!(s
            .previous(&d.id, "two", "create", &json!({}))
            .unwrap()
            .is_none());
        l.max_sessions = 10;
        s.set_resource_limits(&d, l).unwrap();
        second.state = "running".into();
        assert!(accept(&s, &d, &second, "three").is_err());
        assert_eq!(s.list().unwrap().len(), 1);
    }
    #[test]
    fn event_quota_reserves_capacity_for_final_state() {
        let (_dir, s, d) = setup();
        let mut l = s.resource_limits().unwrap();
        l.max_event_bytes = 1024 * 1024;
        s.set_resource_limits(&d, l).unwrap();
        let v = session(&d);
        accept(&s, &d, &v, "one").unwrap();
        let mut blocked = false;
        for _ in 0..100 {
            if s.append(&v.id, "provider_event", json!({"text":"x".repeat(32768)}))
                .is_err()
            {
                blocked = true;
                break;
            }
        }
        assert!(blocked);
        s.append(&v.id, "state", json!({"state":"failed"})).unwrap();
        assert!(s.resource_usage().unwrap()["event_bytes"].as_u64().unwrap() <= 1024 * 1024);
    }
    #[test]
    fn retention_preserves_running_events_and_idempotency_records() {
        let (_dir, s, d) = setup();
        let mut l = s.resource_limits().unwrap();
        l.retention_seconds = 60;
        s.set_resource_limits(&d, l).unwrap();
        let mut idle = session(&d);
        idle.created_at = now() - 120;
        accept(&s, &d, &idle, "idle").unwrap();
        let mut active = session(&d);
        active.created_at = now() - 120;
        active.state = "running".into();
        accept(&s, &d, &active, "active").unwrap();
        s.0.lock()
            .unwrap()
            .execute("UPDATE events SET created_at=?1", [now() - 120])
            .unwrap();
        assert_eq!(s.prune_retention(&d).unwrap()["events_removed"], 1);
        assert_eq!(s.events(&idle.id, 0).unwrap()[0].kind, "history_gap");
        assert_eq!(s.events(&active.id, 0).unwrap().len(), 1);
        assert!(s
            .previous(&d.id, "idle", "create", &json!({}))
            .unwrap()
            .is_some());
        assert!(s
            .previous(&d.id, "active", "create", &json!({}))
            .unwrap()
            .is_some());
    }
}
