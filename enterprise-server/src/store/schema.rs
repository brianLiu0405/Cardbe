pub(super) const SCHEMA: &str = r#"
CREATE TABLE IF NOT EXISTS enterprise_schema(version INTEGER NOT NULL);
INSERT INTO enterprise_schema(version) SELECT 1 WHERE NOT EXISTS(SELECT 1 FROM enterprise_schema);
CREATE TABLE IF NOT EXISTS organizations(id TEXT PRIMARY KEY, name TEXT NOT NULL);
CREATE TABLE IF NOT EXISTS users(
  id TEXT PRIMARY KEY, org_id TEXT NOT NULL REFERENCES organizations(id), name TEXT NOT NULL,
  role TEXT NOT NULL CHECK(role IN ('owner','admin','member')),
  status TEXT NOT NULL CHECK(status IN ('active','suspended')), UNIQUE(org_id,id));
CREATE TABLE IF NOT EXISTS devices(
  id TEXT PRIMARY KEY, org_id TEXT NOT NULL, user_id TEXT NOT NULL, name TEXT NOT NULL,
  token_hash BLOB NOT NULL UNIQUE, revoked INTEGER NOT NULL DEFAULT 0,
  expires_at INTEGER NOT NULL, UNIQUE(org_id,id),
  FOREIGN KEY(org_id,user_id) REFERENCES users(org_id,id));
CREATE TABLE IF NOT EXISTS workspaces(
  id TEXT PRIMARY KEY, org_id TEXT NOT NULL REFERENCES organizations(id), name TEXT NOT NULL,
  UNIQUE(org_id,id));
CREATE TABLE IF NOT EXISTS boards(
  id TEXT PRIMARY KEY, org_id TEXT NOT NULL, workspace_id TEXT NOT NULL, name TEXT NOT NULL,
  revision INTEGER NOT NULL DEFAULT 0, UNIQUE(org_id,id),
  FOREIGN KEY(org_id,workspace_id) REFERENCES workspaces(org_id,id));
CREATE TABLE IF NOT EXISTS board_access(
  org_id TEXT NOT NULL, board_id TEXT NOT NULL, user_id TEXT NOT NULL,
  role TEXT NOT NULL CHECK(role IN ('manager','editor','viewer')),
  PRIMARY KEY(org_id,board_id,user_id),
  FOREIGN KEY(org_id,board_id) REFERENCES boards(org_id,id),
  FOREIGN KEY(org_id,user_id) REFERENCES users(org_id,id));
CREATE TABLE IF NOT EXISTS tasks(
  id TEXT PRIMARY KEY, org_id TEXT NOT NULL, board_id TEXT NOT NULL, title TEXT NOT NULL,
  description TEXT NOT NULL, status TEXT NOT NULL CHECK(status IN ('todo','done')),
  assignee_id TEXT, version INTEGER NOT NULL DEFAULT 1, UNIQUE(org_id,board_id,id),
  FOREIGN KEY(org_id,board_id) REFERENCES boards(org_id,id),
  FOREIGN KEY(org_id,assignee_id) REFERENCES users(org_id,id));
CREATE TABLE IF NOT EXISTS comments(
  id TEXT PRIMARY KEY, org_id TEXT NOT NULL, board_id TEXT NOT NULL, task_id TEXT NOT NULL,
  author_id TEXT NOT NULL, body TEXT NOT NULL, created_at TEXT NOT NULL,
  FOREIGN KEY(org_id,board_id,task_id) REFERENCES tasks(org_id,board_id,id),
  FOREIGN KEY(org_id,author_id) REFERENCES users(org_id,id));
CREATE TABLE IF NOT EXISTS operations(
  org_id TEXT NOT NULL, user_id TEXT NOT NULL, operation_id TEXT NOT NULL,
  board_id TEXT NOT NULL, fingerprint BLOB NOT NULL, response TEXT NOT NULL,
  PRIMARY KEY(org_id,user_id,operation_id),
  FOREIGN KEY(org_id,user_id) REFERENCES users(org_id,id),
  FOREIGN KEY(org_id,board_id) REFERENCES boards(org_id,id));
CREATE TABLE IF NOT EXISTS audit(
  org_id TEXT NOT NULL, id INTEGER NOT NULL, actor_id TEXT NOT NULL, device_id TEXT NOT NULL,
  action TEXT NOT NULL, resource_id TEXT NOT NULL, outcome TEXT NOT NULL,
  created_at TEXT NOT NULL, operation_id TEXT, PRIMARY KEY(org_id,id),
  FOREIGN KEY(org_id,actor_id) REFERENCES users(org_id,id),
  FOREIGN KEY(org_id,device_id) REFERENCES devices(org_id,id));
CREATE INDEX IF NOT EXISTS devices_by_user ON devices(org_id,user_id);
CREATE INDEX IF NOT EXISTS boards_by_workspace ON boards(org_id,workspace_id);
CREATE INDEX IF NOT EXISTS access_by_user ON board_access(org_id,user_id);
CREATE INDEX IF NOT EXISTS tasks_by_board ON tasks(org_id,board_id);
CREATE INDEX IF NOT EXISTS comments_by_board ON comments(org_id,board_id);
"#;
