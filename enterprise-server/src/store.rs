//! An isolated, single-host enterprise prototype. Every request serializes
//! authentication, authorization and acceptance in one SQLite write transaction.
mod schema;
#[cfg(test)]
mod tests;

use crate::{error::ApiError, model::*};
use chrono::{SecondsFormat, Utc};
use rusqlite::{params, Connection, OptionalExtension, Row, TransactionBehavior};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::{path::Path, time::Duration};
use uuid::Uuid;

const TOKEN_LIFETIME_SECONDS: i64 = 14 * 24 * 60 * 60;

pub struct Store {
    connection: Connection,
}

struct Actor {
    org: String,
    user: String,
    device: String,
    role: String,
    active: bool,
}

fn id() -> String {
    Uuid::new_v4().to_string()
}
fn timestamp() -> String {
    Utc::now().to_rfc3339_opts(SecondsFormat::Millis, true)
}
fn digest(value: &[u8]) -> Vec<u8> {
    Sha256::digest(value).to_vec()
}
fn uuid(value: &str) -> Result<String, ApiError> {
    Uuid::parse_str(value)
        .map(|id| id.to_string())
        .map_err(|_| ApiError::InvalidArgument)
}
fn text(value: &str, limit: usize, required: bool) -> Result<String, ApiError> {
    if value.chars().count() > limit || (required && value.trim().is_empty()) {
        return Err(ApiError::InvalidArgument);
    }
    Ok(if required { value.trim() } else { value }.to_owned())
}
fn admin(actor: &Actor) -> Result<(), ApiError> {
    if matches!(actor.role.as_str(), "owner" | "admin") {
        Ok(())
    } else {
        Err(ApiError::PermissionDenied)
    }
}

impl Store {
    pub fn open(path: impl AsRef<Path>) -> Result<Self, ApiError> {
        let connection = Connection::open(path)?;
        connection.busy_timeout(Duration::from_secs(5))?;
        // Refuse an existing desktop or unrelated database before changing it.
        let tables: i64 = connection.query_row(
            "SELECT count(*) FROM sqlite_master WHERE type='table' AND name NOT LIKE 'sqlite_%'",
            [],
            |r| r.get(0),
        )?;
        if tables > 0 {
            let version =
                connection.query_row("SELECT version FROM enterprise_schema", [], |r| {
                    r.get::<_, i64>(0)
                })?;
            if version != 1 {
                return Err(ApiError::InternalError);
            }
        }
        connection.pragma_update(None, "foreign_keys", "ON")?;
        connection.pragma_update(None, "journal_mode", "WAL")?;
        connection.execute_batch(schema::SCHEMA)?;
        Ok(Self { connection })
    }

    pub fn bootstrap(
        &mut self,
        organization: &str,
        owner: &str,
        device: &str,
    ) -> Result<Value, ApiError> {
        let organization = text(organization, 200, true)?;
        let owner = text(owner, 200, true)?;
        let device_name = text(device, 200, true)?;
        let tx = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let org = id();
        let user = id();
        tx.execute(
            "INSERT INTO organizations(id,name) VALUES(?1,?2)",
            params![org, organization],
        )?;
        tx.execute(
            "INSERT INTO users(id,org_id,name,role,status) VALUES(?1,?2,?3,'owner','active')",
            params![user, org, owner],
        )?;
        let issued = issue_device(&tx, &org, &user, &device_name)?;
        let actor = Actor {
            org: org.clone(),
            user: user.clone(),
            device: issued["device"]["id"].as_str().unwrap().to_owned(),
            role: "owner".into(),
            active: true,
        };
        audit(
            &tx,
            &actor,
            "organization.bootstrap",
            &org,
            "accepted",
            None,
        )?;
        let result = json!({"organization":{"id":org,"name":organization},"user":member(&tx,&actor.org,&user)?,"device":issued["device"],"token":issued["token"],"expires_at":issued["expires_at"]});
        tx.commit()?;
        Ok(result)
    }

    /// Host-trusted recovery for the local CLI only; never expose over HTTP.
    /// It issues credentials only for an existing active owner in this org.
    pub fn recover_owner(
        &mut self,
        organization_id: &str,
        user_id: &str,
        device_name: &str,
    ) -> Result<Value, ApiError> {
        let org = uuid(organization_id)?;
        let user = uuid(user_id)?;
        let device_name = text(device_name, 200, true)?;
        let tx = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let owner = member(&tx, &org, &user)?;
        if owner["role"] != "owner" || owner["status"] != "active" {
            return Err(ApiError::PermissionDenied);
        }
        let organization: String = tx.query_row(
            "SELECT name FROM organizations WHERE id=?1",
            [&org],
            |row| row.get(0),
        )?;
        let issued = issue_device(&tx, &org, &user, &device_name)?;
        let actor = Actor {
            org: org.clone(),
            user: user.clone(),
            device: issued["device"]["id"].as_str().unwrap().to_owned(),
            role: "owner".into(),
            active: true,
        };
        audit(
            &tx,
            &actor,
            "device.owner_recovery",
            &actor.device,
            "accepted",
            None,
        )?;
        let result = json!({"organization":{"id":org,"name":organization},"user":owner,
            "device":issued["device"],"token":issued["token"],"expires_at":issued["expires_at"]});
        tx.commit()?;
        Ok(result)
    }

    pub fn execute(&mut self, token: &str, action: Action) -> Result<Value, ApiError> {
        let tx = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let actor = authenticate(&tx, token)?;
        if !actor.active {
            audit(&tx, &actor, "authentication", "", "denied", None)?;
            tx.commit()?;
            return Err(ApiError::Unauthenticated);
        }
        let (name, resource, operation_id, mutates) = action_info(&tx, &actor, &action)?;
        // Keep rejected requests auditable without committing partial mutations.
        tx.execute_batch("SAVEPOINT request_action")?;
        let result = dispatch(&tx, &actor, action);
        match &result {
            Ok(value) => {
                tx.execute_batch("RELEASE request_action")?;
                if mutates && value.get("replayed").and_then(Value::as_bool) != Some(true) {
                    let created_resource = match name {
                        "workspace.create" | "board.create" => value.get("id"),
                        "member.create" => value.get("member").and_then(|v| v.get("id")),
                        "device.create" => value.get("device").and_then(|v| v.get("id")),
                        _ => None,
                    }
                    .and_then(Value::as_str);
                    audit(
                        &tx,
                        &actor,
                        name,
                        created_resource.unwrap_or(&resource),
                        "accepted",
                        operation_id.as_deref(),
                    )?;
                }
            }
            Err(error) => {
                tx.execute_batch("ROLLBACK TO request_action; RELEASE request_action")?;
                let outcome = match error {
                    ApiError::PermissionDenied | ApiError::Unauthenticated | ApiError::NotFound => {
                        "denied"
                    }
                    _ => "rejected",
                };
                audit(
                    &tx,
                    &actor,
                    name,
                    &resource,
                    outcome,
                    operation_id.as_deref(),
                )?;
            }
        }
        tx.commit()?;
        result
    }
}

fn authenticate(db: &Connection, token: &str) -> Result<Actor, ApiError> {
    if token.is_empty() || token.len() > 512 {
        return Err(ApiError::Unauthenticated);
    }
    db.query_row(
        "SELECT u.org_id,u.id,d.id,u.role,u.status='active' AND d.revoked=0 AND d.expires_at>?2
         FROM devices d JOIN users u ON u.org_id=d.org_id AND u.id=d.user_id WHERE d.token_hash=?1",
        params![digest(token.as_bytes()), Utc::now().timestamp()],
        |r| {
            Ok(Actor {
                org: r.get(0)?,
                user: r.get(1)?,
                device: r.get(2)?,
                role: r.get(3)?,
                active: r.get(4)?,
            })
        },
    )
    .optional()?
    .ok_or(ApiError::Unauthenticated)
}

fn issue_device(db: &Connection, org: &str, user: &str, name: &str) -> Result<Value, ApiError> {
    let device = id();
    let token = format!("{}{}", Uuid::new_v4().simple(), Uuid::new_v4().simple());
    let expires = Utc::now().timestamp() + TOKEN_LIFETIME_SECONDS;
    db.execute("INSERT INTO devices(id,org_id,user_id,name,token_hash,expires_at) VALUES(?1,?2,?3,?4,?5,?6)", params![device,org,user,name,digest(token.as_bytes()),expires])?;
    Ok(
        json!({"device":{"id":device,"user_id":user,"name":name,"revoked":false},"token":token,"expires_at":expires}),
    )
}

fn member_row(r: &Row<'_>) -> rusqlite::Result<Value> {
    Ok(
        json!({"id":r.get::<_,String>(0)?,"name":r.get::<_,String>(1)?,"role":r.get::<_,String>(2)?,"status":r.get::<_,String>(3)?}),
    )
}
fn member(db: &Connection, org: &str, user: &str) -> Result<Value, ApiError> {
    db.query_row(
        "SELECT id,name,role,status FROM users WHERE org_id=?1 AND id=?2",
        params![org, user],
        member_row,
    )
    .optional()?
    .ok_or(ApiError::NotFound)
}
fn device_row(r: &Row<'_>) -> rusqlite::Result<Value> {
    Ok(
        json!({"id":r.get::<_,String>(0)?,"name":r.get::<_,String>(1)?,"user_id":r.get::<_,String>(2)?,"revoked":r.get::<_,bool>(3)?}),
    )
}
fn board_row(r: &Row<'_>) -> rusqlite::Result<Value> {
    Ok(
        json!({"id":r.get::<_,String>(0)?,"workspace_id":r.get::<_,String>(1)?,"name":r.get::<_,String>(2)?,"revision":r.get::<_,i64>(3)?,"role":r.get::<_,String>(4)?}),
    )
}
fn board(db: &Connection, actor: &Actor, board_id: &str) -> Result<Value, ApiError> {
    let value = db.query_row(
        "SELECT b.id,b.workspace_id,b.name,b.revision,a.role FROM boards b JOIN board_access a ON a.org_id=b.org_id AND a.board_id=b.id
         WHERE b.org_id=?1 AND b.id=?2 AND a.user_id=?3",params![actor.org,board_id,actor.user],board_row).optional()?;
    value.ok_or(ApiError::NotFound)
}
fn board_write(
    db: &Connection,
    actor: &Actor,
    board_id: &str,
    manager: bool,
) -> Result<Value, ApiError> {
    let board = board(db, actor, board_id)?;
    let role = board["role"].as_str().unwrap_or_default();
    if role == "viewer" || (manager && role != "manager") {
        return Err(ApiError::PermissionDenied);
    }
    Ok(board)
}
fn workspace_exists(db: &Connection, org: &str, workspace: &str) -> Result<(), ApiError> {
    let exists: bool = db.query_row(
        "SELECT EXISTS(SELECT 1 FROM workspaces WHERE org_id=?1 AND id=?2)",
        params![org, workspace],
        |r| r.get(0),
    )?;
    if exists {
        Ok(())
    } else {
        Err(ApiError::NotFound)
    }
}

fn dispatch(db: &Connection, actor: &Actor, action: Action) -> Result<Value, ApiError> {
    match action {
        Action::Me => {
            let name: String = db.query_row(
                "SELECT name FROM organizations WHERE id=?1",
                [&actor.org],
                |r| r.get(0),
            )?;
            let device = db.query_row(
                "SELECT id,name,user_id,revoked FROM devices WHERE org_id=?1 AND id=?2",
                params![actor.org, actor.device],
                device_row,
            )?;
            Ok(
                json!({"organization":{"id":actor.org,"name":name},"user":member(db,&actor.org,&actor.user)?,"device":device}),
            )
        }
        Action::ListWorkspaces => {
            let mut query =
                db.prepare("SELECT id,name FROM workspaces WHERE org_id=?1 ORDER BY name,id")?;
            let values = query
                .query_map([&actor.org], |r| {
                    Ok(json!({"id":r.get::<_,String>(0)?,"name":r.get::<_,String>(1)?}))
                })?
                .collect::<Result<Vec<_>, _>>()?;
            Ok(json!({"workspaces":values}))
        }
        Action::CreateWorkspace(input) => {
            admin(actor)?;
            let name = text(&input.name, 200, true)?;
            let workspace = id();
            db.execute(
                "INSERT INTO workspaces(id,org_id,name) VALUES(?1,?2,?3)",
                params![workspace, actor.org, name],
            )?;
            Ok(json!({"id":workspace,"name":name}))
        }
        Action::ListBoards { workspace_id } => {
            if let Some(workspace) = &workspace_id {
                workspace_exists(db, &actor.org, workspace)?;
            }
            let mut query=db.prepare("SELECT b.id,b.workspace_id,b.name,b.revision,a.role FROM boards b JOIN board_access a ON a.org_id=b.org_id AND a.board_id=b.id WHERE b.org_id=?1 AND a.user_id=?2 AND (?3 IS NULL OR b.workspace_id=?3) ORDER BY b.name,b.id")?;
            let boards = query
                .query_map(params![actor.org, actor.user, workspace_id], board_row)?
                .collect::<Result<Vec<_>, _>>()?;
            Ok(json!({"boards":boards}))
        }
        Action::CreateBoard(input) => {
            admin(actor)?;
            workspace_exists(db, &actor.org, &input.workspace_id)?;
            let name = text(&input.name, 200, true)?;
            let board_id = id();
            db.execute(
                "INSERT INTO boards(id,org_id,workspace_id,name) VALUES(?1,?2,?3,?4)",
                params![board_id, actor.org, input.workspace_id, name],
            )?;
            db.execute(
                "INSERT INTO board_access(org_id,board_id,user_id,role) VALUES(?1,?2,?3,'manager')",
                params![actor.org, board_id, actor.user],
            )?;
            board(db, actor, &board_id)
        }
        Action::ListMembers => {
            admin(actor)?;
            let mut query = db.prepare(
                "SELECT id,name,role,status FROM users WHERE org_id=?1 ORDER BY name,id",
            )?;
            let members = query
                .query_map([&actor.org], member_row)?
                .collect::<Result<Vec<_>, _>>()?;
            Ok(json!({"members":members}))
        }
        Action::CreateMember(input) => {
            admin(actor)?;
            if !matches!(input.role.as_str(), "admin" | "member") {
                return Err(ApiError::InvalidArgument);
            }
            if actor.role != "owner" && input.role != "member" {
                return Err(ApiError::PermissionDenied);
            }
            let name = text(&input.name, 200, true)?;
            let device_name = text(&input.device_name, 200, true)?;
            let user = id();
            db.execute(
                "INSERT INTO users(id,org_id,name,role,status) VALUES(?1,?2,?3,?4,'active')",
                params![user, actor.org, name, input.role],
            )?;
            let issued = issue_device(db, &actor.org, &user, &device_name)?;
            Ok(
                json!({"member":member(db,&actor.org,&user)?,"device":issued["device"],"token":issued["token"],"expires_at":issued["expires_at"]}),
            )
        }
        Action::SetMemberStatus { member_id, input } => {
            set_member_status(db, actor, &member_id, &input.status)
        }
        Action::ListDevices => {
            admin(actor)?;
            let mut query = db.prepare(
                "SELECT id,name,user_id,revoked FROM devices WHERE org_id=?1 ORDER BY name,id",
            )?;
            let devices = query
                .query_map([&actor.org], device_row)?
                .collect::<Result<Vec<_>, _>>()?;
            Ok(json!({"devices":devices}))
        }
        Action::CreateDevice { member_id, input } => {
            admin(actor)?;
            let target = member(db, &actor.org, &member_id)?;
            protect_privileged_member(actor, &target)?;
            if target["status"] != "active" {
                return Err(ApiError::InvalidArgument);
            }
            issue_device(db, &actor.org, &member_id, &text(&input.name, 200, true)?)
        }
        Action::RevokeDevice { device_id } => {
            admin(actor)?;
            let user: String = db
                .query_row(
                    "SELECT user_id FROM devices WHERE org_id=?1 AND id=?2",
                    params![actor.org, device_id],
                    |r| r.get(0),
                )
                .optional()?
                .ok_or(ApiError::NotFound)?;
            protect_privileged_member(actor, &member(db, &actor.org, &user)?)?;
            // Keep at least one usable device for an active organization owner.
            // Expired/revoked devices are not recovery paths and do not count.
            let strands_owner: bool = db.query_row(
                "SELECT EXISTS(
                  SELECT 1 FROM devices d JOIN users u ON u.org_id=d.org_id AND u.id=d.user_id
                  WHERE d.org_id=?1 AND d.id=?2 AND d.revoked=0 AND d.expires_at>?3
                    AND u.role='owner' AND u.status='active'
                    AND NOT EXISTS(
                      SELECT 1 FROM devices other JOIN users owner ON owner.org_id=other.org_id AND owner.id=other.user_id
                      WHERE other.org_id=?1 AND other.id<>?2 AND other.revoked=0 AND other.expires_at>?3
                        AND owner.role='owner' AND owner.status='active'))",
                params![actor.org, device_id, Utc::now().timestamp()], |row| row.get(0),
            )?;
            if strands_owner {
                return Err(ApiError::LastManager);
            }
            db.execute(
                "UPDATE devices SET revoked=1 WHERE org_id=?1 AND id=?2",
                params![actor.org, device_id],
            )?;
            Ok(json!({"revoked":true}))
        }
        Action::SetAccess { board_id, input } => set_access(db, actor, &board_id, input),
        Action::Snapshot { board_id } => snapshot(db, actor, &board_id),
        Action::Command { board_id, input } => command(db, actor, &board_id, input),
        Action::Audit { after, limit } => {
            admin(actor)?;
            if after < 0 || !(1..=200).contains(&limit) {
                return Err(ApiError::InvalidArgument);
            }
            let mut query=db.prepare("SELECT id,actor_id,device_id,action,resource_id,outcome,created_at,operation_id FROM audit WHERE org_id=?1 AND id>?2 ORDER BY id LIMIT ?3")?;
            let events=query.query_map(params![actor.org,after,limit],|r|Ok(json!({"id":r.get::<_,i64>(0)?,"actor_id":r.get::<_,String>(1)?,"device_id":r.get::<_,String>(2)?,"action":r.get::<_,String>(3)?,"resource_id":r.get::<_,String>(4)?,"outcome":r.get::<_,String>(5)?,"created_at":r.get::<_,String>(6)?,"operation_id":r.get::<_,Option<String>>(7)?})))?.collect::<Result<Vec<_>,_>>()?;
            let next = events
                .last()
                .and_then(|e| e["id"].as_i64())
                .unwrap_or(after);
            Ok(json!({"events":events,"next_cursor":next}))
        }
    }
}

fn protect_privileged_member(actor: &Actor, target: &Value) -> Result<(), ApiError> {
    // Minting a device is impersonation; an admin cannot mint an owner's token.
    if actor.role != "owner"
        && (target["role"] == "owner" || (target["role"] == "admin" && target["id"] != actor.user))
    {
        Err(ApiError::PermissionDenied)
    } else {
        Ok(())
    }
}

fn active_manager_count(db: &Connection, org: &str, board_id: &str) -> Result<i64, ApiError> {
    Ok(db.query_row("SELECT count(*) FROM board_access a JOIN users u ON u.org_id=a.org_id AND u.id=a.user_id WHERE a.org_id=?1 AND a.board_id=?2 AND a.role='manager' AND u.status='active'",params![org,board_id],|r|r.get(0))?)
}
fn set_member_status(
    db: &Connection,
    actor: &Actor,
    user: &str,
    status: &str,
) -> Result<Value, ApiError> {
    admin(actor)?;
    if !matches!(status, "active" | "suspended") {
        return Err(ApiError::InvalidArgument);
    }
    let target = member(db, &actor.org, user)?;
    protect_privileged_member(actor, &target)?;
    if status == "suspended" && target["status"] == "active" {
        if target["role"] == "owner" {
            let owners: i64 = db.query_row(
                "SELECT count(*) FROM users WHERE org_id=?1 AND role='owner' AND status='active'",
                [&actor.org],
                |r| r.get(0),
            )?;
            if owners <= 1 {
                return Err(ApiError::LastManager);
            }
        }
        let mut query = db.prepare(
            "SELECT board_id FROM board_access WHERE org_id=?1 AND user_id=?2 AND role='manager'",
        )?;
        let boards = query
            .query_map(params![actor.org, user], |r| r.get::<_, String>(0))?
            .collect::<Result<Vec<_>, _>>()?;
        for board_id in boards {
            if active_manager_count(db, &actor.org, &board_id)? <= 1 {
                return Err(ApiError::LastManager);
            }
        }
        // Reactivation never revives previously issued credentials.
        db.execute(
            "UPDATE devices SET revoked=1 WHERE org_id=?1 AND user_id=?2",
            params![actor.org, user],
        )?;
    }
    db.execute(
        "UPDATE users SET status=?3 WHERE org_id=?1 AND id=?2",
        params![actor.org, user, status],
    )?;
    member(db, &actor.org, user)
}
fn set_access(
    db: &Connection,
    actor: &Actor,
    board_id: &str,
    input: AccessInput,
) -> Result<Value, ApiError> {
    board_write(db, actor, board_id, true)?;
    let target = member(db, &actor.org, &input.member_id)?;
    if let Some(role) = &input.role {
        if !matches!(role.as_str(), "manager" | "editor" | "viewer") || target["status"] != "active"
        {
            return Err(ApiError::InvalidArgument);
        }
    }
    let previous: Option<String> = db
        .query_row(
            "SELECT role FROM board_access WHERE org_id=?1 AND board_id=?2 AND user_id=?3",
            params![actor.org, board_id, input.member_id],
            |r| r.get(0),
        )
        .optional()?;
    if previous.as_deref() == Some("manager")
        && target["status"] == "active"
        && input.role.as_deref() != Some("manager")
        && active_manager_count(db, &actor.org, board_id)? <= 1
    {
        return Err(ApiError::LastManager);
    }
    if let Some(role) = input.role {
        db.execute("INSERT INTO board_access(org_id,board_id,user_id,role) VALUES(?1,?2,?3,?4) ON CONFLICT(org_id,board_id,user_id) DO UPDATE SET role=excluded.role",params![actor.org,board_id,input.member_id,role])?;
    } else {
        db.execute(
            "DELETE FROM board_access WHERE org_id=?1 AND board_id=?2 AND user_id=?3",
            params![actor.org, board_id, input.member_id],
        )?;
    }
    Ok(json!({"updated":true}))
}

fn task_row(r: &Row<'_>) -> rusqlite::Result<Value> {
    Ok(
        json!({"id":r.get::<_,String>(0)?,"title":r.get::<_,String>(1)?,"description":r.get::<_,String>(2)?,"status":r.get::<_,String>(3)?,"assignee_id":r.get::<_,Option<String>>(4)?,"version":r.get::<_,i64>(5)?}),
    )
}
fn task(db: &Connection, org: &str, board_id: &str, task_id: &str) -> Result<Value, ApiError> {
    db.query_row("SELECT id,title,description,status,assignee_id,version FROM tasks WHERE org_id=?1 AND board_id=?2 AND id=?3",params![org,board_id,task_id],task_row).optional()?.ok_or(ApiError::NotFound)
}
fn comment_row(r: &Row<'_>) -> rusqlite::Result<Value> {
    Ok(
        json!({"id":r.get::<_,String>(0)?,"task_id":r.get::<_,String>(1)?,"author_id":r.get::<_,String>(2)?,"body":r.get::<_,String>(3)?,"created_at":r.get::<_,String>(4)?}),
    )
}
fn snapshot(db: &Connection, actor: &Actor, board_id: &str) -> Result<Value, ApiError> {
    let board = board(db, actor, board_id)?;
    let mut tasks=db.prepare("SELECT id,title,description,status,assignee_id,version FROM tasks WHERE org_id=?1 AND board_id=?2 ORDER BY id")?;
    let tasks = tasks
        .query_map(params![actor.org, board_id], task_row)?
        .collect::<Result<Vec<_>, _>>()?;
    let mut comments=db.prepare("SELECT id,task_id,author_id,body,created_at FROM comments WHERE org_id=?1 AND board_id=?2 ORDER BY created_at,id")?;
    let comments = comments
        .query_map(params![actor.org, board_id], comment_row)?
        .collect::<Result<Vec<_>, _>>()?;
    Ok(json!({"board":board,"tasks":tasks,"comments":comments}))
}
fn validate_assignee(
    db: &Connection,
    org: &str,
    board_id: &str,
    user: Option<&str>,
) -> Result<(), ApiError> {
    if let Some(user) = user {
        let allowed:bool=db.query_row("SELECT EXISTS(SELECT 1 FROM users u JOIN board_access a ON a.org_id=u.org_id AND a.user_id=u.id WHERE u.org_id=?1 AND u.id=?2 AND u.status='active' AND a.board_id=?3)",params![org,user,board_id],|r|r.get(0))?;
        if !allowed {
            return Err(ApiError::NotFound);
        }
    }
    Ok(())
}
fn command(
    db: &Connection,
    actor: &Actor,
    board_id: &str,
    input: CommandInput,
) -> Result<Value, ApiError> {
    // Authorize before receipts, including retries accepted on another device.
    board_write(db, actor, board_id, false)?;
    let operation = uuid(&input.operation_id)?;
    let fingerprint = digest(&serde_json::to_vec(&input.command)?);
    let previous:Option<(String,Vec<u8>,String)>=db.query_row("SELECT board_id,fingerprint,response FROM operations WHERE org_id=?1 AND user_id=?2 AND operation_id=?3",params![actor.org,actor.user,operation],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?))).optional()?;
    if let Some((previous_board, previous_fingerprint, response)) = previous {
        if previous_board != board_id || previous_fingerprint != fingerprint {
            return Err(ApiError::IdempotencyConflict);
        }
        let mut response: Value = serde_json::from_str(&response)?;
        response["replayed"] = json!(true);
        return Ok(response);
    }
    let result = match input.command {
        Command::TaskCreate {
            title,
            description,
            assignee_id,
        } => {
            let title = text(&title, 500, true)?;
            let description = text(&description, 100_000, false)?;
            validate_assignee(db, &actor.org, board_id, assignee_id.as_deref())?;
            let task_id = id();
            db.execute("INSERT INTO tasks(id,org_id,board_id,title,description,status,assignee_id) VALUES(?1,?2,?3,?4,?5,'todo',?6)",params![task_id,actor.org,board_id,title,description,assignee_id])?;
            json!({"task":task(db,&actor.org,board_id,&task_id)?})
        }
        Command::TaskUpdate {
            task_id,
            base_version,
            title,
            description,
            status,
            assignee_id,
        } => {
            let previous = task(db, &actor.org, board_id, &task_id)?;
            if base_version < 1 {
                return Err(ApiError::InvalidArgument);
            }
            if previous["version"] != base_version {
                return Err(ApiError::Conflict);
            }
            if title.is_none() && description.is_none() && status.is_none() && assignee_id.is_none()
            {
                return Err(ApiError::InvalidArgument);
            }
            let title = title
                .map(|value| text(&value, 500, true))
                .transpose()?
                .unwrap_or_else(|| previous["title"].as_str().unwrap().into());
            let description = description
                .map(|value| text(&value, 100_000, false))
                .transpose()?
                .unwrap_or_else(|| previous["description"].as_str().unwrap().into());
            let status = status.unwrap_or_else(|| previous["status"].as_str().unwrap().into());
            if !matches!(status.as_str(), "todo" | "done") {
                return Err(ApiError::InvalidArgument);
            }
            if let Some(ref assigned) = assignee_id {
                validate_assignee(db, &actor.org, board_id, assigned.as_deref())?;
            }
            let assignee =
                assignee_id.unwrap_or_else(|| previous["assignee_id"].as_str().map(str::to_owned));
            db.execute("UPDATE tasks SET title=?4,description=?5,status=?6,assignee_id=?7,version=version+1 WHERE org_id=?1 AND board_id=?2 AND id=?3",params![actor.org,board_id,task_id,title,description,status,assignee])?;
            json!({"task":task(db,&actor.org,board_id,&task_id)?})
        }
        Command::CommentCreate { task_id, body } => {
            task(db, &actor.org, board_id, &task_id)?;
            let body = text(&body, 10_000, true)?;
            let comment = id();
            let created = timestamp();
            db.execute("INSERT INTO comments(id,org_id,board_id,task_id,author_id,body,created_at) VALUES(?1,?2,?3,?4,?5,?6,?7)",params![comment,actor.org,board_id,task_id,actor.user,body,created])?;
            json!({"comment":{"id":comment,"task_id":task_id,"author_id":actor.user,"body":body,"created_at":created}})
        }
    };
    db.execute(
        "UPDATE boards SET revision=revision+1 WHERE org_id=?1 AND id=?2",
        params![actor.org, board_id],
    )?;
    let revision: i64 = db.query_row(
        "SELECT revision FROM boards WHERE org_id=?1 AND id=?2",
        params![actor.org, board_id],
        |r| r.get(0),
    )?;
    let response =
        json!({"operation_id":operation,"revision":revision,"replayed":false,"result":result});
    db.execute("INSERT INTO operations(org_id,user_id,operation_id,board_id,fingerprint,response) VALUES(?1,?2,?3,?4,?5,?6)",params![actor.org,actor.user,operation,board_id,fingerprint,serde_json::to_string(&response)?])?;
    Ok(response)
}

fn audit(
    db: &Connection,
    actor: &Actor,
    action: &str,
    resource: &str,
    outcome: &str,
    operation: Option<&str>,
) -> Result<(), ApiError> {
    db.execute("INSERT INTO audit(org_id,id,actor_id,device_id,action,resource_id,outcome,created_at,operation_id) SELECT ?1,coalesce(max(id),0)+1,?2,?3,?4,?5,?6,?7,?8 FROM audit WHERE org_id=?1",params![actor.org,actor.user,actor.device,action,resource,outcome,timestamp(),operation])?;
    Ok(())
}

type ActionInfo = (&'static str, String, Option<String>, bool);
fn action_info(db: &Connection, actor: &Actor, action: &Action) -> Result<ActionInfo, ApiError> {
    let (name, table, resource, operation, mutates) = match action {
        Action::Me => ("session.read", None, None, None, false),
        Action::ListWorkspaces => ("workspace.list", None, None, None, false),
        Action::CreateWorkspace(_) => ("workspace.create", None, None, None, true),
        Action::ListBoards { workspace_id } => (
            "board.list",
            Some("workspaces"),
            workspace_id.as_deref(),
            None,
            false,
        ),
        Action::CreateBoard(input) => (
            "board.create",
            Some("workspaces"),
            Some(input.workspace_id.as_str()),
            None,
            true,
        ),
        Action::ListMembers => ("member.list", None, None, None, false),
        Action::CreateMember(_) => ("member.create", None, None, None, true),
        Action::SetMemberStatus { member_id, .. } => (
            "member.status",
            Some("users"),
            Some(member_id.as_str()),
            None,
            true,
        ),
        Action::ListDevices => ("device.list", None, None, None, false),
        Action::CreateDevice { member_id, .. } => (
            "device.create",
            Some("users"),
            Some(member_id.as_str()),
            None,
            true,
        ),
        Action::RevokeDevice { device_id } => (
            "device.revoke",
            Some("devices"),
            Some(device_id.as_str()),
            None,
            true,
        ),
        Action::SetAccess { board_id, .. } => (
            "board.access",
            Some("boards"),
            Some(board_id.as_str()),
            None,
            true,
        ),
        Action::Snapshot { board_id } => (
            "board.snapshot",
            Some("boards"),
            Some(board_id.as_str()),
            None,
            false,
        ),
        Action::Command { board_id, input } => {
            let name = match input.command {
                Command::TaskCreate { .. } => "task.create",
                Command::TaskUpdate { .. } => "task.update",
                Command::CommentCreate { .. } => "comment.create",
            };
            (
                name,
                Some("boards"),
                Some(board_id.as_str()),
                Uuid::parse_str(&input.operation_id)
                    .ok()
                    .map(|id| id.to_string()),
                true,
            )
        }
        Action::Audit { .. } => ("audit.list", None, None, None, false),
    };
    let resource = if let (Some(table), Some(resource)) = (table, resource) {
        // Table names above are compile-time constants; resource values are bound.
        let exists: bool = db.query_row(
            &format!("SELECT EXISTS(SELECT 1 FROM {table} WHERE org_id=?1 AND id=?2)"),
            params![actor.org, resource],
            |r| r.get(0),
        )?;
        if exists {
            resource.to_owned()
        } else {
            String::new()
        }
    } else {
        actor.org.clone()
    };
    Ok((name, resource, operation, mutates))
}
