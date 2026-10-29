use crate::{
    models::{now, Device, Session},
    store::Store,
};
use anyhow::{bail, Context, Result};
use rand::RngCore;
use rusqlite::{params, Connection, OptionalExtension, Transaction};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};

pub fn migrate(tx: &Transaction<'_>) -> Result<()> {
    tx.execute_batch("CREATE TABLE IF NOT EXISTS members(id TEXT PRIMARY KEY,label TEXT NOT NULL,role TEXT NOT NULL,revoked INTEGER NOT NULL DEFAULT 0,created_at INTEGER NOT NULL);
 CREATE TABLE IF NOT EXISTS device_members(device_id TEXT PRIMARY KEY REFERENCES devices(id),member_id TEXT NOT NULL REFERENCES members(id));
 CREATE TABLE IF NOT EXISTS invitations(hash TEXT PRIMARY KEY,member_id TEXT NOT NULL,label TEXT NOT NULL,role TEXT NOT NULL,expires INTEGER NOT NULL,created_by TEXT NOT NULL,consumed_at INTEGER);
 CREATE TABLE IF NOT EXISTS pairing_members(hash TEXT PRIMARY KEY REFERENCES pairing(hash),member_id TEXT NOT NULL REFERENCES members(id));
 CREATE TABLE IF NOT EXISTS session_access(session_id TEXT NOT NULL REFERENCES sessions(id) ON DELETE CASCADE,member_id TEXT NOT NULL REFERENCES members(id),role TEXT NOT NULL,PRIMARY KEY(session_id,member_id));")?;
    let primary: String = tx.query_row(
        "SELECT value FROM metadata WHERE key='member_id'",
        [],
        |r| r.get(0),
    )?;
    tx.execute("INSERT OR IGNORE INTO members(id,label,role,created_at) VALUES(?1,'Host owner','administrator',?2)",params![primary,now()])?;
    tx.execute(
        "INSERT OR IGNORE INTO device_members(device_id,member_id) SELECT id,?1 FROM devices",
        [&primary],
    )?;
    Ok(())
}
pub fn attach_primary_device(tx: &Transaction<'_>, id: &str) -> Result<()> {
    tx.execute("INSERT INTO device_members(device_id,member_id) SELECT ?1,value FROM metadata WHERE key='member_id'",[id])?;
    Ok(())
}
pub fn cap_role(device: &str, member: &str) -> String {
    fn rank(role: &str) -> u8 {
        match role {
            "administrator" => 2,
            "operator" => 1,
            _ => 0,
        }
    }
    match rank(device).min(rank(member)) {
        2 => "administrator",
        1 => "operator",
        _ => "viewer",
    }
    .into()
}
fn admin(device: &Device) -> Result<()> {
    if device.revoked || device.role != "administrator" {
        bail!("forbidden: administrator required");
    }
    Ok(())
}
fn label(value: &str) -> Result<String> {
    let value = value.trim();
    if value.is_empty() || value.len() > 256 || value.chars().any(char::is_control) {
        bail!("label must be 1-256 bytes without control characters");
    }
    Ok(value.into())
}
fn role(value: &str) -> Result<()> {
    if !["administrator", "operator", "viewer"].contains(&value) {
        bail!("invalid member role");
    }
    Ok(())
}
fn secret() -> String {
    let mut bytes = [0u8; 32];
    rand::rngs::OsRng.fill_bytes(&mut bytes);
    hex::encode(bytes)
}
fn hash(value: &str) -> String {
    hex::encode(Sha256::digest(value.as_bytes()))
}
fn audit(tx: &Transaction<'_>, actor: &str, action: &str, target: &str) -> Result<()> {
    tx.execute(
        "INSERT INTO audit(actor,action,target,created_at) VALUES(?1,?2,?3,?4)",
        params![actor, action, target, now()],
    )?;
    Ok(())
}
fn session(tx: &Connection, id: &str) -> Result<Session> {
    let value: String =
        tx.query_row("SELECT json FROM sessions WHERE id=?1", [id], |r| r.get(0))?;
    Ok(serde_json::from_str(&value)?)
}
fn can_manage(device: &Device, session: &Session) -> Result<()> {
    if device.revoked
        || device.role == "viewer"
        || (device.member_id != session.owner && device.role != "administrator")
    {
        bail!("forbidden: session owner or administrator required");
    }
    Ok(())
}

impl Store {
    pub fn members(&self) -> Result<Value> {
        let c = self.0.lock().unwrap();
        let mut q = c.prepare(
            "SELECT id,label,role,revoked,created_at FROM members ORDER BY created_at,id",
        )?;
        let result=q.query_map([],|r|Ok(json!({"id":r.get::<_,String>(0)?,"label":r.get::<_,String>(1)?,"role":r.get::<_,String>(2)?,"revoked":r.get::<_,bool>(3)?,"created_at":r.get::<_,i64>(4)?})))?.collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(json!(result))
    }
    pub fn device_member(&self, id: &str) -> Result<Option<(String, String, bool)>> {
        let c = self.0.lock().unwrap();
        Ok(c.query_row("SELECT m.id,m.role,m.revoked FROM device_members d JOIN members m ON m.id=d.member_id WHERE d.device_id=?1",[id],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?))).optional()?)
    }
    pub fn member_enrollment(&self, actor: &Device, id: &str, requested: &str) -> Result<Value> {
        if actor.revoked || (actor.member_id != id && actor.role != "administrator") {
            bail!("forbidden: member owner or administrator required");
        }
        role(requested)?;
        let mut c = self.0.lock().unwrap();
        let tx = c.transaction()?;
        let member_role: Option<String> = tx
            .query_row(
                "SELECT role FROM members WHERE id=?1 AND revoked=0",
                [id],
                |r| r.get(0),
            )
            .optional()?;
        let member_role = member_role.context("active member not found")?;
        if cap_role(requested, &cap_role(&actor.role, &member_role)) != requested {
            bail!("forbidden: device role exceeds acting device or member role");
        }
        let value = secret();
        let expires = now() + 300;
        tx.execute(
            "INSERT INTO pairing(hash,expires,role) VALUES(?1,?2,?3)",
            params![hash(&value), expires, requested],
        )?;
        tx.execute(
            "INSERT INTO pairing_members(hash,member_id) VALUES(?1,?2)",
            params![hash(&value), id],
        )?;
        audit(&tx, &actor.id, "member.enrollment", id)?;
        tx.commit()?;
        Ok(json!({"secret":value,"member_id":id,"expires_at":expires,"role":requested}))
    }
    pub fn create_invitation(
        &self,
        actor: &Device,
        name: &str,
        member_role: &str,
        ttl: i64,
    ) -> Result<Value> {
        admin(actor)?;
        role(member_role)?;
        let name = label(name)?;
        if !(60..=604800).contains(&ttl) {
            bail!("invitation lifetime must be 60-604800 seconds");
        }
        let mut c = self.0.lock().unwrap();
        let tx = c.transaction()?;
        let used:i64=tx.query_row("SELECT (SELECT COUNT(*) FROM members WHERE revoked=0)+(SELECT COUNT(*) FROM invitations WHERE consumed_at IS NULL AND expires>?1)",[now()],|r|r.get(0))?;
        if used as u64 >= crate::quotas::limits(&tx)?.max_members {
            bail!("member quota reached");
        }
        let value = secret();
        let member = uuid::Uuid::new_v4().to_string();
        let expires = now() + ttl;
        tx.execute("INSERT INTO invitations(hash,member_id,label,role,expires,created_by) VALUES(?1,?2,?3,?4,?5,?6)",params![hash(&value),member,name,member_role,expires,actor.id])?;
        audit(&tx, &actor.id, "invitation.create", &member)?;
        tx.commit()?;
        Ok(json!({"secret":value,"member_id":member,"expires_at":expires,"expires_in":ttl}))
    }
    pub fn accept_invitation(&self, value: &str, device_name: &str) -> Result<Value> {
        if value.len() != 64 || !value.bytes().all(|b| b.is_ascii_hexdigit()) {
            bail!("invalid or expired invitation");
        }
        let device_name = label(device_name)?;
        let mut c = self.0.lock().unwrap();
        let tx = c.transaction()?;
        let invitation:Option<(String,String,String)>=tx.query_row("SELECT member_id,label,role FROM invitations WHERE hash=?1 AND expires>?2 AND consumed_at IS NULL",params![hash(value),now()],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?))).optional()?;
        let (member, name, member_role) = invitation.context("invalid or expired invitation")?;
        let active: i64 =
            tx.query_row("SELECT COUNT(*) FROM members WHERE revoked=0", [], |r| {
                r.get(0)
            })?;
        if active as u64 >= crate::quotas::limits(&tx)?.max_members {
            bail!("member quota reached");
        }
        let changed=tx.execute("UPDATE invitations SET consumed_at=?1 WHERE hash=?2 AND consumed_at IS NULL AND expires>?1",params![now(),hash(value)])?;
        if changed != 1 {
            bail!("invalid or expired invitation");
        }
        tx.execute(
            "INSERT INTO members(id,label,role,created_at) VALUES(?1,?2,?3,?4)",
            params![member, name, member_role, now()],
        )?;
        let id = uuid::Uuid::new_v4().to_string();
        let token = secret();
        tx.execute(
            "INSERT INTO devices(id,name,role,token_hash,revoked) VALUES(?1,?2,?3,?4,0)",
            params![id, device_name, member_role, hash(&token)],
        )?;
        tx.execute(
            "INSERT INTO device_members(device_id,member_id) VALUES(?1,?2)",
            params![id, member],
        )?;
        audit(&tx, &id, "invitation.accept", &member)?;
        tx.commit()?;
        Ok(json!({"token":token,"device_id":id,"member_id":member,"role":member_role}))
    }
    pub fn revoke_member(&self, actor: &Device, id: &str) -> Result<Vec<String>> {
        admin(actor)?;
        let mut c = self.0.lock().unwrap();
        let tx = c.transaction()?;
        let member: Option<(String, bool)> = tx
            .query_row("SELECT role,revoked FROM members WHERE id=?1", [id], |r| {
                Ok((r.get(0)?, r.get(1)?))
            })
            .optional()?;
        let (member_role, revoked) = member.context("member not found")?;
        if !revoked && member_role == "administrator" {
            let admins: i64 = tx.query_row(
                "SELECT COUNT(*) FROM members WHERE role='administrator' AND revoked=0",
                [],
                |r| r.get(0),
            )?;
            if admins <= 1 {
                bail!("cannot revoke the last administrator member");
            }
        }
        let ids = {
            let mut q = tx.prepare("SELECT device_id FROM device_members WHERE member_id=?1")?;
            let ids = q
                .query_map([id], |r| r.get::<_, String>(0))?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            ids
        };
        tx.execute("UPDATE members SET revoked=1 WHERE id=?1", [id])?;
        tx.execute("UPDATE devices SET revoked=1 WHERE id IN(SELECT device_id FROM device_members WHERE member_id=?1)",[id])?;
        tx.execute("DELETE FROM session_access WHERE member_id=?1", [id])?;
        audit(&tx, &actor.id, "member.revoke", id)?;
        tx.commit()?;
        Ok(ids)
    }
    pub fn session_grant(&self, session_id: &str, member_id: &str) -> Result<Option<String>> {
        let c = self.0.lock().unwrap();
        Ok(c.query_row("SELECT a.role FROM session_access a JOIN members m ON m.id=a.member_id WHERE a.session_id=?1 AND a.member_id=?2 AND m.revoked=0",params![session_id,member_id],|r|r.get(0)).optional()?)
    }
    pub fn session_access(&self, actor: &Device, id: &str) -> Result<Value> {
        let c = self.0.lock().unwrap();
        let s = session(&c, id)?;
        let granted: bool = c.query_row(
            "SELECT EXISTS(SELECT 1 FROM session_access WHERE session_id=?1 AND member_id=?2)",
            params![id, actor.member_id],
            |r| r.get(0),
        )?;
        if actor.revoked
            || (actor.member_id != s.owner && actor.role != "administrator" && !granted)
        {
            bail!("forbidden: session access required");
        }
        let manage = can_manage(actor, &s).is_ok();
        let mut q=c.prepare("SELECT a.member_id,a.role FROM session_access a JOIN members m ON m.id=a.member_id WHERE a.session_id=?1 AND m.revoked=0 ORDER BY a.member_id")?;
        let grants = q
            .query_map([id], |r| {
                Ok(json!({"member_id":r.get::<_,String>(0)?,"role":r.get::<_,String>(1)?}))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(json!({"owner":s.owner,"shared":!grants.is_empty(),"can_manage":manage,"grants":grants}))
    }
    pub fn set_session_access(
        &self,
        actor: &Device,
        id: &str,
        member_id: &str,
        permission: Option<&str>,
    ) -> Result<()> {
        if permission.is_some_and(|value| !["viewer", "operator"].contains(&value)) {
            bail!("session grant must be viewer or operator");
        }
        let mut c = self.0.lock().unwrap();
        let tx = c.transaction()?;
        let mut s = session(&tx, id)?;
        can_manage(actor, &s)?;
        if member_id == s.owner {
            bail!("the owner already has implicit access");
        }
        if let Some(permission) = permission {
            let member_role: Option<String> = tx
                .query_row(
                    "SELECT role FROM members WHERE id=?1 AND revoked=0",
                    [member_id],
                    |r| r.get(0),
                )
                .optional()?;
            let member_role = member_role.context("active member not found")?;
            if permission == "operator" && member_role == "viewer" {
                bail!("a viewer member cannot receive write access");
            }
            tx.execute("INSERT INTO session_access(session_id,member_id,role) VALUES(?1,?2,?3) ON CONFLICT(session_id,member_id) DO UPDATE SET role=excluded.role",params![id,member_id,permission])?;
        } else {
            tx.execute(
                "DELETE FROM session_access WHERE session_id=?1 AND member_id=?2",
                params![id, member_id],
            )?;
        }
        let count: i64 = tx.query_row(
            "SELECT COUNT(*) FROM session_access WHERE session_id=?1",
            [id],
            |r| r.get(0),
        )?;
        s.shared = count > 0;
        tx.execute(
            "UPDATE sessions SET json=?1 WHERE id=?2",
            params![serde_json::to_string(&s)?, id],
        )?;
        audit(&tx, &actor.id, "session.access", id)?;
        tx.commit()?;
        Ok(())
    }
    pub fn set_legacy_sharing(&self, actor: &Device, id: &str, shared: bool) -> Result<()> {
        let mut c = self.0.lock().unwrap();
        let tx = c.transaction()?;
        let mut s = session(&tx, id)?;
        can_manage(actor, &s)?;
        tx.execute("DELETE FROM session_access WHERE session_id=?1", [id])?;
        if shared {
            tx.execute("INSERT INTO session_access(session_id,member_id,role) SELECT ?1,id,'viewer' FROM members WHERE revoked=0 AND id<>?2",params![id,s.owner])?;
        }
        let count: i64 = tx.query_row(
            "SELECT COUNT(*) FROM session_access WHERE session_id=?1",
            [id],
            |r| r.get(0),
        )?;
        s.shared = count > 0;
        tx.execute(
            "UPDATE sessions SET json=?1 WHERE id=?2",
            params![serde_json::to_string(&s)?, id],
        )?;
        audit(&tx, &actor.id, "session.share", id)?;
        tx.commit()?;
        Ok(())
    }
    pub fn revoke_invitation(&self, actor: &Device, id: &str) -> Result<()> {
        admin(actor)?;
        let mut c = self.0.lock().unwrap();
        let tx = c.transaction()?;
        tx.execute(
            "UPDATE invitations SET expires=0 WHERE member_id=?1 AND consumed_at IS NULL",
            [id],
        )?;
        audit(&tx, &actor.id, "invitation.revoke", id)?;
        tx.commit()?;
        Ok(())
    }
    pub fn invitations(&self, actor: &Device) -> Result<Value> {
        admin(actor)?;
        let c = self.0.lock().unwrap();
        let mut q=c.prepare("SELECT member_id,label,role,expires,consumed_at FROM invitations ORDER BY expires DESC LIMIT 1000")?;
        let values=q.query_map([],|r|Ok(json!({"member_id":r.get::<_,String>(0)?,"label":r.get::<_,String>(1)?,"role":r.get::<_,String>(2)?,"expires_at":r.get::<_,i64>(3)?,"consumed_at":r.get::<_,Option<i64>>(4)?})))?.collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(json!(values))
    }
}

use crate::{api::ApiError, core::Core};
use axum::{
    extract::{Path, State},
    routing::{get, post},
    Extension, Json, Router,
};
use serde::Deserialize;
use std::sync::Arc;
#[derive(Deserialize)]
struct Invite {
    label: String,
    role: String,
    expires_in_seconds: Option<i64>,
}
#[derive(Deserialize)]
struct Accept {
    secret: String,
    name: String,
    csr_pem: Option<String>,
}
#[derive(Deserialize)]
struct Access {
    member_id: String,
    role: Option<String>,
    #[serde(default)]
    revoke: bool,
}
pub fn protected_router() -> Router<Arc<Core>> {
    Router::new()
        .route("/v1/members", get(members))
        .route("/v1/members/{id}/revoke", post(revoke))
        .route("/v1/members/{id}/enrollment", post(enroll_member))
        .route("/v1/invitations", get(invitations).post(invite))
        .route("/v1/invitations/{id}/revoke", post(revoke_invite))
        .route("/v1/sessions/{id}/access", get(access).post(set_access))
}
pub fn public_router() -> Router<Arc<Core>> {
    Router::new().route("/v1/invitations/accept", post(accept))
}
async fn members(State(c): State<Arc<Core>>) -> Result<Json<Value>, ApiError> {
    Ok(Json(c.store.members()?))
}
async fn invitations(
    State(c): State<Arc<Core>>,
    Extension(d): Extension<Device>,
) -> Result<Json<Value>, ApiError> {
    Ok(Json(c.store.invitations(&d)?))
}
async fn invite(
    State(c): State<Arc<Core>>,
    Extension(d): Extension<Device>,
    Json(v): Json<Invite>,
) -> Result<Json<Value>, ApiError> {
    Ok(Json(c.store.create_invitation(
        &d,
        &v.label,
        &v.role,
        v.expires_in_seconds.unwrap_or(86400),
    )?))
}
async fn accept(
    State(c): State<Arc<Core>>,
    extensions: axum::http::Extensions,
    headers: axum::http::HeaderMap,
    Json(v): Json<Accept>,
) -> Result<Json<Value>, ApiError> {
    let capable = crate::certificates::client_auth_capable(&extensions, &headers);
    crate::certificates::validate_pairing_csr(&c, capable, v.csr_pem.as_deref())?;
    Ok(Json(crate::certificates::finish_pairing(
        &c,
        c.store.accept_invitation(&v.secret, &v.name)?,
        v.csr_pem.as_deref(),
    )?))
}

async fn revoke_invite(
    State(c): State<Arc<Core>>,
    Extension(d): Extension<Device>,
    Path(id): Path<String>,
) -> Result<Json<Value>, ApiError> {
    c.store.revoke_invitation(&d, &id)?;
    Ok(Json(json!({"revoked":true})))
}
async fn revoke(
    State(c): State<Arc<Core>>,
    Extension(d): Extension<Device>,
    Path(id): Path<String>,
) -> Result<Json<Value>, ApiError> {
    let _guard = c.mutation.lock().await;
    let devices = c.store.revoke_member(&d, &id)?;
    let sessions = c.store.list()?;
    for (session_id, run) in c.runs.lock().await.iter() {
        if devices.contains(&run.device_id)
            || sessions
                .iter()
                .any(|s| s.id == *session_id && s.owner == id)
        {
            run.cancel.cancel();
        }
    }
    for s in c.store.list()? {
        if s.owner == id {
            c.terminals.stop(&s.id);
        }
    }
    c.desktop.stop();
    Ok(Json(json!({"revoked":true})))
}
async fn access(
    State(c): State<Arc<Core>>,
    Extension(d): Extension<Device>,
    Path(id): Path<String>,
) -> Result<Json<Value>, ApiError> {
    Ok(Json(c.store.session_access(&d, &id)?))
}
async fn set_access(
    State(c): State<Arc<Core>>,
    Extension(d): Extension<Device>,
    Path(id): Path<String>,
    Json(v): Json<Access>,
) -> Result<Json<Value>, ApiError> {
    let _guard = c.mutation.lock().await;
    if !v.revoke && v.role.is_none() {
        return Err(anyhow::anyhow!("grant role required").into());
    }
    c.store.set_session_access(
        &d,
        &id,
        &v.member_id,
        if v.revoke { None } else { v.role.as_deref() },
    )?;
    if v.revoke || v.role.as_deref() == Some("viewer") {
        if let Some(run) = c.runs.lock().await.get(&id) {
            if c.store
                .device_member(&run.device_id)?
                .is_some_and(|(member, _, _)| member == v.member_id)
            {
                run.cancel.cancel();
            }
        }
        c.terminals.stop(&id);
        c.desktop.stop();
    }
    Ok(Json(c.store.session_access(&d, &id)?))
}

#[derive(Deserialize)]
struct MemberEnrollment {
    role: String,
}
async fn enroll_member(
    State(c): State<Arc<Core>>,
    Extension(d): Extension<Device>,
    Path(id): Path<String>,
    Json(v): Json<MemberEnrollment>,
) -> Result<Json<Value>, ApiError> {
    Ok(Json(c.store.member_enrollment(&d, &id, &v.role)?))
}
#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::Core;
    fn setup() -> (tempfile::TempDir, std::sync::Arc<Core>, Device) {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(&dir.path().join("state.db")).unwrap();
        let pair = store
            .pair(&store.issue_pairing("administrator").unwrap(), "Owner")
            .unwrap();
        let owner = store.authenticate(pair["token"].as_str().unwrap()).unwrap();
        let c = Core::new(
            store,
            vec![dir.path().canonicalize().unwrap()],
            "Fixture".into(),
        )
        .unwrap();
        (dir, c, owner)
    }
    fn member(c: &Core, owner: &Device, name: &str, role: &str) -> (Device, String) {
        let invite = c.store.create_invitation(owner, name, role, 60).unwrap();
        let joined = c
            .store
            .accept_invitation(invite["secret"].as_str().unwrap(), name)
            .unwrap();
        let token = joined["token"].as_str().unwrap().to_owned();
        (c.store.authenticate(&token).unwrap(), token)
    }
    fn session(c: &Core, d: &Device) -> Session {
        let s = Session {
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
        };
        c.store
            .accept(
                &d.id,
                &uuid::Uuid::new_v4().to_string(),
                "create",
                &json!({}),
                &json!(s),
                &s,
                "state",
                json!({"state":"idle"}),
            )
            .unwrap();
        s
    }
    #[test]
    fn member_devices_keep_identity_and_role_cap() {
        let (_dir, c, owner) = setup();
        let (bob, _) = member(&c, &owner, "Bob", "operator");
        let secret = c
            .store
            .member_enrollment(&bob, &bob.member_id, "viewer")
            .unwrap();
        let pair = c
            .store
            .pair(secret["secret"].as_str().unwrap(), "Phone")
            .unwrap();
        let phone = c
            .store
            .authenticate(pair["token"].as_str().unwrap())
            .unwrap();
        assert_eq!(phone.member_id, bob.member_id);
        assert_eq!(phone.role, "viewer");
        assert!(c
            .store
            .member_enrollment(&bob, &bob.member_id, "administrator")
            .is_err());
        assert!(c
            .store
            .member_enrollment(&bob, &owner.member_id, "viewer")
            .is_err());
    }
    #[test]
    fn explicit_acl_read_write_sharing_snapshot_and_revocation() {
        let (_dir, c, owner) = setup();
        let s = session(&c, &owner);
        let (bob, token) = member(&c, &owner, "Bob", "operator");
        assert!(c.session(&bob, &s.id, false).is_err());
        c.store
            .set_session_access(&owner, &s.id, &bob.member_id, Some("viewer"))
            .unwrap();
        assert!(c.session(&bob, &s.id, false).is_ok());
        assert!(c.session(&bob, &s.id, true).is_err());
        assert_eq!(
            c.store.session_access(&bob, &s.id).unwrap()["can_manage"],
            false
        );
        c.store
            .set_session_access(&owner, &s.id, &bob.member_id, Some("operator"))
            .unwrap();
        assert!(c.session(&bob, &s.id, true).is_ok());
        c.store.set_legacy_sharing(&owner, &s.id, true).unwrap();
        let (carol, _) = member(&c, &owner, "Carol", "operator");
        assert!(c.session(&carol, &s.id, false).is_err());
        c.store.revoke_member(&owner, &bob.member_id).unwrap();
        assert!(c.store.authenticate(&token).is_err());
        assert!(c.session(&bob, &s.id, false).is_err());
        assert!(c.store.revoke_member(&owner, &owner.member_id).is_err());
    }
    #[test]
    fn invitations_single_use_expiring_revocable_quota_reserved() {
        let (_dir, c, owner) = setup();
        let mut policy = c.store.resource_limits().unwrap();
        policy.max_members = 2;
        c.store.set_resource_limits(&owner, policy).unwrap();
        let invite = c
            .store
            .create_invitation(&owner, "Bob", "operator", 60)
            .unwrap();
        assert!(c
            .store
            .create_invitation(&owner, "Carol", "operator", 60)
            .is_err());
        let secret = invite["secret"].as_str().unwrap();
        let joined = c.store.accept_invitation(secret, "Bob").unwrap();
        assert!(c.store.accept_invitation(secret, "Replay").is_err());
        c.store
            .revoke_member(&owner, joined["member_id"].as_str().unwrap())
            .unwrap();
        let invite = c
            .store
            .create_invitation(&owner, "Revoked", "viewer", 60)
            .unwrap();
        c.store
            .revoke_invitation(&owner, invite["member_id"].as_str().unwrap())
            .unwrap();
        assert!(c
            .store
            .accept_invitation(invite["secret"].as_str().unwrap(), "No")
            .is_err());
        let invite = c
            .store
            .create_invitation(&owner, "Expired", "viewer", 60)
            .unwrap();
        c.store
            .0
            .lock()
            .unwrap()
            .execute("UPDATE invitations SET expires=0", [])
            .unwrap();
        assert!(c
            .store
            .accept_invitation(invite["secret"].as_str().unwrap(), "No")
            .is_err());
    }
}
