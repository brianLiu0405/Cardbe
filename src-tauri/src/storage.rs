use crate::errors::DomainError;
use crate::models::{
    Archive, Board, BoardRole, Column, ColumnOverview, ColumnSort, IrohDeviceStatus,
    IrohPermission, Note, SingleBoardExport, StoredData, SyncStatus, Task, TaskSummary,
    TaskTemplate, CURRENT_SCHEMA_VERSION,
};
use crate::task_events::{self, EventSource};
use base64::Engine;
use rusqlite::{
    params,
    types::{FromSql, FromSqlError, FromSqlResult, ToSql, ToSqlOutput, ValueRef},
    Connection, OptionalExtension, Transaction, TransactionBehavior,
};
use std::{
    collections::HashMap,
    fs::{self, File},
    io::{BufReader, Write},
    path::{Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};

pub(crate) type StorageResult<T> = Result<T, Box<dyn std::error::Error>>;

/// A task found for due notifications, with where it is.
pub struct DueTask {
    pub board_id: i64,
    pub board_name: String,
    pub column_id: i64,
    pub task: Task,
}
type IrohInviteRecord = (String, i64, String, IrohPermission, bool);
type IrohInviteSummaryRecord = (String, i64, IrohPermission, bool, String, String);

pub(crate) struct AllTaskRow {
    pub board_id: i64,
    pub board_name: String,
    pub column_id: Option<i64>,
    pub column_name: Option<String>,
    pub archived_at: Option<u128>,
    /// Whether the task is in its board's done column.
    pub done: bool,
    /// Open tasks of its board that the task still waits on.
    pub waiting_count: i64,
    pub task: TaskSummary,
    /// Where a search matched outside the title.
    pub excerpt: Option<String>,
}

/// Tasks matching a filter under each explorer status.
#[derive(Debug, Default, PartialEq, Eq, serde::Serialize)]
pub struct TaskStatusCounts {
    pub all: i64,
    pub active: i64,
    pub overdue: i64,
    pub recurring: i64,
    pub archived: i64,
}

#[derive(Clone, Default, serde::Deserialize)]
#[serde(default)]
pub struct TaskExplorerQuery {
    pub query: String,
    pub board_ids: Option<Vec<i64>>,
    pub column_id: Option<i64>,
    pub status: String,
    pub sort: String,
    pub due: String,
    pub due_start: Option<i64>,
    pub due_end: Option<i64>,
    pub archive_start: Option<i64>,
    pub archive_end: Option<i64>,
    pub now: i64,
    /// Priorities to keep, with `none` for tasks without one; `None` keeps all.
    pub priorities: Option<Vec<String>>,
    /// `open`, `done` (in the done column) and `archived` tasks to keep;
    /// `None` keeps all. Applies on top of `status`.
    pub completion: Option<Vec<String>>,
    /// Keeps tasks with any of these labels.
    pub labels: Option<Vec<String>>,
    /// `waiting` keeps active tasks that wait on an open task, `not_waiting`
    /// the rest, and `waited_on` tasks that an open task waits on.
    pub waiting: Option<String>,
    /// Keeps only recurring tasks, or only the others.
    pub recurring: Option<bool>,
    pub search_options: Option<crate::search::SearchOptions>,
}

impl TaskExplorerQuery {
    /// The filters added for Focuses, passed to SQL as one JSON object.
    fn extra_filters(&self) -> Result<String, serde_json::Error> {
        serde_json::to_string(&serde_json::json!({
            "completion": self.completion,
            "labels": self.labels,
            "waiting": self.waiting,
            "recurring": self.recurring,
        }))
    }
}

// Shared eligibility is applied before either engine selects its Top K.
const TASK_SEARCH_CTE: &str = "WITH tasks AS (
             SELECT s.rowid AS search_id, s.board_id, b.name AS board_name, c.name AS column_name,
                    t.column_id, s.archived, s.task_id, a.archived_at,
                    CASE WHEN s.archived=0 THEN t.payload ELSE a.payload END AS payload,
                    (SELECT d.id FROM cardbe_columns d WHERE d.board_id=s.board_id AND d.done=1
                     ORDER BY d.position, d.id LIMIT 1) AS done_column_id
             FROM cardbe_task_search s
             JOIN cardbe_boards b ON b.id=s.board_id
             LEFT JOIN cardbe_tasks t
               ON s.archived=0 AND t.board_id=s.board_id AND t.id=s.task_id
             LEFT JOIN cardbe_columns c
               ON s.archived=0 AND c.board_id=t.board_id AND c.id=t.column_id
             LEFT JOIN cardbe_archives a
               ON s.archived=1 AND a.board_id=s.board_id AND a.task_id=s.task_id
             WHERE (?2 IS NULL OR s.board_id IN (SELECT value FROM json_each(?2)))
             ), summaries AS (
               SELECT *, json_extract(payload, '$.due_time') AS due_time,
                      json_extract(payload, '$.title') AS title,
                      json_extract(payload, '$.recurrence') AS recurrence,
                      COALESCE(json_extract(payload, '$.due_at_midnight'), 0) AS due_at_midnight,
                      COALESCE(json_extract(payload, '$.priority'), 'none') AS priority
               FROM tasks
             )
             , classified AS (SELECT *,
               CASE WHEN archived=1 THEN 'archived' WHEN column_id=done_column_id THEN 'done'
                    ELSE 'open' END AS completion,
               CASE WHEN archived=1 THEN 0 ELSE (
                 SELECT COUNT(*) FROM json_each(e.payload, '$.blocked_by') w
                 JOIN cardbe_tasks bt ON bt.board_id=e.board_id AND bt.id=w.value
                 WHERE bt.column_id IS NOT e.done_column_id) END AS waiting_count
             FROM summaries e)
             , eligible AS (SELECT * FROM classified c
             WHERE (?3 IS NULL OR column_id=?3)
               AND (CASE ?4
                 WHEN 'active' THEN archived=0
                 WHEN 'archived' THEN archived=1
                 WHEN 'overdue' THEN archived=0 AND due_time<?5 AND (due_time<>?11 OR due_at_midnight)
                 WHEN 'recurring' THEN archived=0 AND recurrence IS NOT NULL
                 ELSE 1 END)
               AND (CASE ?6 WHEN 'none' THEN due_time IS NULL
                           WHEN 'due' THEN due_time IS NOT NULL ELSE 1 END)
               AND (?7 IS NULL OR due_time>=?7)
               AND (?8 IS NULL OR due_time<?8)
               AND (archived=0 OR ((?9 IS NULL OR CAST(archived_at AS INTEGER)>=?9)
                                 AND (?10 IS NULL OR CAST(archived_at AS INTEGER)<?10)))
               AND (?12 IS NULL OR priority IN (SELECT value FROM json_each(?12)))
               AND (json_extract(?13, '$.completion') IS NULL
                    OR completion IN (SELECT value FROM json_each(?13, '$.completion')))
               AND (json_extract(?13, '$.labels') IS NULL OR EXISTS (
                 SELECT 1 FROM json_each(c.payload, '$.labels') l
                 WHERE l.value IN (SELECT value FROM json_each(?13, '$.labels'))))
               AND (json_extract(?13, '$.recurring') IS NULL
                    OR (recurrence IS NOT NULL)=json_extract(?13, '$.recurring'))
               AND (CASE json_extract(?13, '$.waiting')
                 WHEN 'waiting' THEN waiting_count>0
                 WHEN 'not_waiting' THEN waiting_count=0
                 WHEN 'waited_on' THEN completion='open' AND EXISTS (
                   SELECT 1 FROM cardbe_tasks wt, json_each(wt.payload, '$.blocked_by') w
                   WHERE wt.board_id=c.board_id AND w.value=c.task_id
                     AND wt.column_id IS NOT c.done_column_id)
                 ELSE 1 END)
)";

fn search_params(filter: &TaskExplorerQuery) -> StorageResult<Vec<rusqlite::types::Value>> {
    use rusqlite::types::Value;
    let opt = |value: Option<i64>| value.map(Value::Integer).unwrap_or(Value::Null);
    Ok(vec![
        Value::Null,
        filter
            .board_ids
            .as_ref()
            .map(serde_json::to_string)
            .transpose()?
            .map(Value::Text)
            .unwrap_or(Value::Null),
        opt(filter.column_id),
        Value::Text(filter.status.clone()),
        Value::Integer(filter.now),
        Value::Text(filter.due.clone()),
        opt(filter.due_start),
        opt(filter.due_end),
        opt(filter.archive_start),
        opt(filter.archive_end),
        // A date-only due date today is stored at this midnight and is not
        // overdue until the day ends; earlier midnights already are. A chosen
        // 00:00 (`due_at_midnight`) is overdue as soon as it passes.
        Value::Integer(crate::task_dates::local_day_start(filter.now)),
        filter
            .priorities
            .as_ref()
            .map(serde_json::to_string)
            .transpose()?
            .map(Value::Text)
            .unwrap_or(Value::Null),
        Value::Text(filter.extra_filters()?),
    ])
}

fn initialize_note_search_index(connection: &Connection) -> StorageResult<()> {
    let index_exists = connection.query_row(
        "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name='cardbe_note_fts')",
        [],
        |row| row.get::<_, bool>(0),
    )?;
    connection.execute_batch(
        "CREATE VIRTUAL TABLE IF NOT EXISTS cardbe_note_fts USING fts5(
             title,content,content='notes',content_rowid='id',tokenize='trigram'
         );
         CREATE TRIGGER IF NOT EXISTS cardbe_notes_ai AFTER INSERT ON notes BEGIN
             INSERT INTO cardbe_note_fts(rowid,title,content) VALUES(new.id,new.title,new.content);
         END;
         CREATE TRIGGER IF NOT EXISTS cardbe_notes_ad AFTER DELETE ON notes BEGIN
             INSERT INTO cardbe_note_fts(cardbe_note_fts,rowid,title,content)
             VALUES('delete',old.id,old.title,old.content);
         END;
         CREATE TRIGGER IF NOT EXISTS cardbe_notes_au AFTER UPDATE OF id,title,content ON notes BEGIN
             INSERT INTO cardbe_note_fts(cardbe_note_fts,rowid,title,content)
             VALUES('delete',old.id,old.title,old.content);
             INSERT INTO cardbe_note_fts(rowid,title,content) VALUES(new.id,new.title,new.content);
         END;",
    )?;
    if !index_exists {
        connection
            .execute_batch("INSERT INTO cardbe_note_fts(cardbe_note_fts) VALUES('rebuild');")?;
    }
    Ok(())
}

fn hybrid_search(
    connection: &Connection,
    filter: &TaskExplorerQuery,
    options: crate::search::SearchOptions,
) -> StorageResult<Vec<i64>> {
    use crate::search::{fuse, title_priority, FuzzySearch};
    let query = filter.query.trim();
    if !options.valid() {
        return Err(DomainError::InvalidArgument.into());
    }
    if query.is_empty() {
        return Ok(Vec::new());
    }
    let started = std::time::Instant::now();
    let lowercase_query = query.to_lowercase();
    let mut values = search_params(filter)?;
    values.push(rusqlite::types::Value::Text(if query.chars().count() < 3 {
        query.to_owned()
    } else {
        format!("\"{}\"", query.replace('"', "\"\""))
    }));
    values.push(rusqlite::types::Value::Integer(i64::try_from(
        options.candidate_limit,
    )?));
    let sql = if query.chars().count() < 3 {
        format!(
            "{TASK_SEARCH_CTE} SELECT e.search_id,e.title FROM eligible e
            JOIN cardbe_task_search s ON s.rowid=e.search_id
            WHERE instr(lower(s.body),lower(?14))>0 ORDER BY e.search_id LIMIT ?15"
        )
    } else {
        format!(
            "{TASK_SEARCH_CTE} SELECT e.search_id,e.title FROM cardbe_task_fts
            JOIN eligible e ON e.search_id=cardbe_task_fts.rowid
            WHERE cardbe_task_fts MATCH ?14 ORDER BY bm25(cardbe_task_fts),e.search_id LIMIT ?15"
        )
    };
    let mut statement = connection.prepare(&sql)?;
    let fts = statement
        .query_map(rusqlite::params_from_iter(&values), |row| {
            Ok((
                row.get::<_, i64>(0)?,
                title_priority(&row.get::<_, String>(1)?, &lowercase_query),
            ))
        })?
        .collect::<Result<Vec<_>, _>>()?;
    let fts_elapsed = started.elapsed();
    let fuzzy_started = std::time::Instant::now();
    // ponytail: large scopes scan every title; add a cache only after release profiling justifies it.
    let sql = format!("{TASK_SEARCH_CTE} SELECT search_id,title FROM eligible");
    let mut statement = connection.prepare(&sql)?;
    let values = search_params(filter)?;
    let mut rows = statement.query(rusqlite::params_from_iter(&values))?;
    let mut fuzzy = FuzzySearch::new(query, options.candidate_limit);
    let mut scanned = 0;
    while let Some(row) = rows.next()? {
        fuzzy.push(row.get(0)?, &row.get::<_, String>(1)?);
        scanned += 1;
    }
    let fuzzy = fuzzy.finish();
    let fuzzy_elapsed = fuzzy_started.elapsed();
    let merge_started = std::time::Instant::now();
    let results = fuse(&fts, &fuzzy, options);
    log::debug!(target: "storage.search", "scanned={} fts_us={} fuzzy_us={} merge_us={} total_us={} results={}",
        scanned, fts_elapsed.as_micros(), fuzzy_elapsed.as_micros(), merge_started.elapsed().as_micros(), started.elapsed().as_micros(), results.len());
    Ok(results)
}

pub struct IrohConflict {
    pub revision: i64,
    pub name: String,
    pub permission: IrohPermission,
    pub data: StoredData,
    pub loro_update: Option<Vec<u8>>,
}

macro_rules! impl_text_enum {
    ($type:ty { $($value:literal => $variant:path),+ $(,)? }) => {
        impl FromSql for $type {
            fn column_result(value: ValueRef<'_>) -> FromSqlResult<Self> {
                match value.as_str()? {
                    $($value => Ok($variant),)+
                    _ => Err(FromSqlError::InvalidType),
                }
            }
        }

        impl ToSql for $type {
            fn to_sql(&self) -> rusqlite::Result<ToSqlOutput<'_>> {
                let value = match self {
                    $($variant => $value,)+
                };
                Ok(ToSqlOutput::from(value))
            }
        }
    };
}

impl_text_enum!(BoardRole {
    "owner" => BoardRole::Owner,
    "editor" => BoardRole::Editor,
    "viewer" => BoardRole::Viewer,
});
impl_text_enum!(SyncStatus {
    "local" => SyncStatus::Local,
    "pending" => SyncStatus::Pending,
    "synced" => SyncStatus::Synced,
    "conflict" => SyncStatus::Conflict,
});
impl_text_enum!(IrohPermission {
    "viewer" => IrohPermission::Viewer,
    "editor" => IrohPermission::Editor,
});

/// Reads `id, name, color, sort_order, sort_rules`.
fn column_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<Column> {
    let sort_rules: String = row.get(4)?;
    let mut column = Column {
        id: row.get(0)?,
        name: row.get(1)?,
        color: row.get(2)?,
        sort_order: row.get(3)?,
        // Unreadable rules fall back to the legacy sort order.
        sort_rules: serde_json::from_str(&sort_rules).unwrap_or_default(),
        done: row.get(5)?,
        tasks: Vec::new(),
    };
    column.reconcile_sort();
    Ok(column)
}

impl FromSql for ColumnSort {
    fn column_result(value: ValueRef<'_>) -> FromSqlResult<Self> {
        Ok(match value.as_str()? {
            "due_date_asc" => Self::DueDateAsc,
            "due_date_desc" => Self::DueDateDesc,
            _ => Self::Custom,
        })
    }
}

impl ToSql for ColumnSort {
    fn to_sql(&self) -> rusqlite::Result<ToSqlOutput<'_>> {
        let value = match self {
            Self::Custom => "custom",
            Self::DueDateAsc => "due_date_asc",
            Self::DueDateDesc => "due_date_desc",
        };
        Ok(ToSqlOutput::from(value))
    }
}

impl FromSql for IrohDeviceStatus {
    fn column_result(value: ValueRef<'_>) -> FromSqlResult<Self> {
        match value.as_i64()? {
            0 => Ok(Self::Pending),
            1 => Ok(Self::Approved),
            2 => Ok(Self::Revoked),
            _ => Err(FromSqlError::InvalidType),
        }
    }
}

impl ToSql for IrohDeviceStatus {
    fn to_sql(&self) -> rusqlite::Result<ToSqlOutput<'_>> {
        let value: i64 = match self {
            Self::Pending => 0,
            Self::Approved => 1,
            Self::Revoked => 2,
        };
        Ok(ToSqlOutput::from(value))
    }
}

pub struct LoadedData {
    pub stored: StoredData,
    pub database: Database,
    pub recovery_messages: Vec<String>,
    pub archives_loaded: bool,
}

pub struct Database {
    pub(crate) connection: Connection,
    path: PathBuf,
    pub(crate) positions: DatabasePositions,
    active_board_id: i64,
    // Writer identities live only for this database session, never in backups.
    pub(crate) loro_peers: HashMap<i64, crate::loro_board::LocalPeer>,
}

#[derive(Clone, Default)]
pub(crate) struct DatabasePositions {
    columns: HashMap<i64, i64>,
    tasks: HashMap<i64, i64>,
}

const SORT_POSITION_GAP: i64 = 1024;

#[derive(Debug, Default, PartialEq, Eq)]
pub struct PersistStats {
    pub columns_changed: usize,
    pub tasks_changed: usize,
    pub archives_changed: usize,
    pub templates_changed: usize,
    pub metadata_changed: usize,
    /// The task event operation recorded for this write, if any.
    pub event_op: Option<i64>,
}

/// How a board write appears in the task event log.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum EventWrite {
    /// Record task placement changes.
    Record(EventSource),
    /// The write replaces task ids, as an import does.
    Reset(EventSource),
}

impl Database {
    pub(crate) fn open_search_reader(path: PathBuf) -> StorageResult<Self> {
        let connection =
            Connection::open_with_flags(&path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)?;
        connection.busy_timeout(std::time::Duration::from_secs(5))?;
        // All search stages see one snapshot, without holding the application state lock.
        connection.execute_batch("BEGIN")?;
        Ok(Self {
            connection,
            path,
            positions: DatabasePositions::default(),
            active_board_id: 0,
            loro_peers: HashMap::new(),
        })
    }

    pub fn get_link_preview(
        &self,
        url: &str,
        now: i64,
    ) -> Result<Option<crate::link_preview::LinkPreview>, DomainError> {
        crate::link_preview::cache::read(&self.connection, url, now)
    }

    pub fn cache_link_preview(
        &self,
        preview: &crate::link_preview::LinkPreview,
    ) -> Result<(), DomainError> {
        crate::link_preview::cache::write(&self.connection, preview)
    }

    pub(crate) fn open(path: PathBuf) -> StorageResult<Self> {
        let connection = Connection::open(&path)?;
        crate::link_preview::cache::initialize(&connection)?;
        let sqlite_user_version: i64 =
            connection.query_row("PRAGMA user_version", [], |row| row.get(0))?;
        log::debug!(
            target: "storage.database",
            "Opened database path={} sqlite_user_version={} current_schema_version={}",
            path.display(),
            sqlite_user_version,
            CURRENT_SCHEMA_VERSION,
        );
        connection.execute_batch(
            "PRAGMA journal_mode = WAL;
             PRAGMA synchronous = NORMAL;
             PRAGMA foreign_keys = ON;
             PRAGMA busy_timeout = 5000;
             CREATE TABLE IF NOT EXISTS columns (
                 id INTEGER PRIMARY KEY,
                 name TEXT NOT NULL,
                 color TEXT NOT NULL,
                 position INTEGER NOT NULL
             );
             CREATE TABLE IF NOT EXISTS tasks (
                 id INTEGER PRIMARY KEY,
                 column_id INTEGER NOT NULL REFERENCES columns(id) ON DELETE CASCADE,
                 position INTEGER NOT NULL,
                 payload TEXT NOT NULL
             );
             CREATE INDEX IF NOT EXISTS tasks_by_column_position
                 ON tasks(column_id, position);
             CREATE TABLE IF NOT EXISTS archives (
                 task_id INTEGER PRIMARY KEY,
                 archived_at TEXT NOT NULL,
                 position INTEGER NOT NULL,
                 payload TEXT NOT NULL
             );
             CREATE TABLE IF NOT EXISTS templates (
                 id INTEGER PRIMARY KEY,
                 name TEXT NOT NULL,
                 position INTEGER NOT NULL,
                 payload TEXT NOT NULL
             );
             CREATE TABLE IF NOT EXISTS notes (
                 id INTEGER PRIMARY KEY AUTOINCREMENT,
                 title TEXT NOT NULL DEFAULT '',
                 content TEXT NOT NULL DEFAULT '',
                 pinned INTEGER NOT NULL DEFAULT 0,
                 created_at INTEGER NOT NULL,
                 updated_at INTEGER NOT NULL
             );
             CREATE INDEX IF NOT EXISTS notes_by_pinned_updated
                 ON notes(pinned DESC, updated_at DESC);
             CREATE TABLE IF NOT EXISTS metadata (
                 key TEXT PRIMARY KEY,
                 value TEXT NOT NULL
             );
             ",
        )?;
        initialize_note_search_index(&connection)?;
        let has_sort_order = connection
            .prepare("PRAGMA table_info(columns)")?
            .query_map([], |row| row.get::<_, String>(1))?
            .collect::<Result<Vec<_>, _>>()?
            .iter()
            .any(|name| name == "sort_order");
        if !has_sort_order {
            connection.execute(
                "ALTER TABLE columns ADD COLUMN sort_order TEXT NOT NULL DEFAULT 'custom'",
                [],
            )?;
        }
        let _ = connection.execute(
            "ALTER TABLE columns ADD COLUMN sort_rules TEXT NOT NULL DEFAULT '[]'",
            [],
        );
        let positions = DatabasePositions::default();
        Ok(Self {
            connection,
            path,
            positions,
            active_board_id: 0,
            loro_peers: HashMap::new(),
        })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn read_flow(
        &self,
        board_id: i64,
        tab_id: Option<&str>,
    ) -> StorageResult<crate::flow::FlowData> {
        crate::flow::read(&self.connection, board_id, tab_id)
    }

    pub fn read_flow_backup(&self, board_id: i64) -> StorageResult<crate::flow::FlowData> {
        crate::flow::read_backup(&self.connection, board_id)
    }

    pub fn read_single_board_backup(&self, board_id: i64) -> StorageResult<SingleBoardExport> {
        self.read_consistent(|db| -> StorageResult<_> {
            let data = db.read_board_complete(board_id)?;
            let flow = db.read_flow_backup(board_id)?;
            let note_ids = flow_note_ids(&flow);
            let notes = db
                .get_notes()?
                .into_iter()
                .filter(|note| note_ids.contains(&note.id))
                .collect();
            Ok(SingleBoardExport {
                data: crate::models::ExportData {
                    labels: data
                        .columns
                        .iter()
                        .flat_map(|column| &column.tasks)
                        .flat_map(|task| task.labels.iter().cloned())
                        .collect::<std::collections::BTreeSet<_>>()
                        .into_iter()
                        .collect(),
                    columns: data.columns,
                    archives: data.archives,
                    templates: data.templates,
                    label_recency: data.label_recency,
                },
                flow: Some(flow),
                notes,
            })
        })?
    }

    pub fn save_flow_layout(
        &mut self,
        board_id: i64,
        tab_id: &str,
        layout: &crate::flow::FlowLayout,
    ) -> StorageResult<()> {
        let tx = self.connection.transaction()?;
        if !crate::flow::tabs(&tx, board_id)?
            .iter()
            .any(|tab| tab.id == tab_id)
        {
            return Err(DomainError::FlowTabNotFound.into());
        }
        crate::flow::validate_layout(layout, &crate::flow::task_ids(&tx, board_id)?)?;
        crate::flow::save_layout(&tx, board_id, tab_id, layout)?;
        tx.commit()?;
        Ok(())
    }

    pub fn change_flow_tabs(
        &mut self,
        board_id: i64,
        change: impl FnOnce(&Transaction<'_>) -> StorageResult<()>,
    ) -> StorageResult<Vec<crate::flow::FlowTab>> {
        let tx = self.connection.transaction()?;
        change(&tx)?;
        let tabs = crate::flow::tabs(&tx, board_id)?;
        tx.commit()?;
        Ok(tabs)
    }

    pub fn change_relationship(
        &mut self,
        board_id: i64,
        relationship: &crate::flow::Relationship,
        remove: bool,
    ) -> StorageResult<()> {
        let relationship = &crate::flow::normalize(relationship.clone());
        let tx = self.connection.transaction()?;
        if remove {
            crate::flow::delete_relationship(&tx, board_id, relationship)?;
            crate::flow::remove_connection(&tx, board_id, relationship)?;
        } else {
            let data = crate::flow::read(&tx, board_id, None)?;
            crate::flow::validate_relationship(
                &data.relationships,
                relationship,
                &crate::flow::task_ids(&tx, board_id)?,
            )?;
            crate::flow::insert_relationship(&tx, board_id, relationship)?;
        }
        tx.commit()?;
        Ok(())
    }

    pub fn update_relationship(
        &mut self,
        board_id: i64,
        relationship: &crate::flow::Relationship,
    ) -> StorageResult<()> {
        let tx = self.connection.transaction()?;
        crate::flow::update_relationship(
            &tx,
            board_id,
            &crate::flow::normalize(relationship.clone()),
        )?;
        tx.commit()?;
        Ok(())
    }

    pub fn active_board_id(&self) -> i64 {
        self.active_board_id
    }

    pub fn search_tasks(&self, board_id: i64, query: &str) -> StorageResult<Vec<i64>> {
        self.search_tasks_with_options(board_id, query, crate::search::SearchOptions::default())
    }

    pub fn search_tasks_with_options(
        &self,
        board_id: i64,
        query: &str,
        options: crate::search::SearchOptions,
    ) -> StorageResult<Vec<i64>> {
        let filter = TaskExplorerQuery {
            query: query.to_owned(),
            board_ids: Some(vec![board_id]),
            ..Default::default()
        };
        let rowids = hybrid_search(&self.connection, &filter, options)?;
        let mut statement = self.connection.prepare(
            "SELECT s.task_id FROM json_each(?1) ranked
                 JOIN cardbe_task_search s ON s.rowid=ranked.value
                 ORDER BY CAST(ranked.key AS INTEGER)",
        )?;
        let rows = statement.query_map([serde_json::to_string(&rowids)?], |row| {
            row.get::<_, i64>(0)
        })?;
        let mut ids = Vec::with_capacity(rowids.len());
        let mut seen = std::collections::HashSet::new();
        for id in rows {
            let id = id?;
            if seen.insert(id) {
                ids.push(id);
            }
        }
        Ok(ids)
    }

    pub fn get_notes(&self) -> StorageResult<Vec<Note>> {
        let mut statement = self.connection.prepare(
            "SELECT id, title, content, pinned, created_at, updated_at
             FROM notes ORDER BY pinned DESC, updated_at DESC, id DESC",
        )?;
        let notes = statement
            .query_map([], |row| {
                Ok(Note {
                    id: row.get(0)?,
                    title: row.get(1)?,
                    content: row.get(2)?,
                    pinned: row.get(3)?,
                    created_at: row.get(4)?,
                    updated_at: row.get(5)?,
                })
            })?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(notes)
    }

    /// Note IDs ranked by fused trigram FTS (title and content) and fuzzy title matching.
    pub fn search_notes(
        &self,
        query: &str,
        options: crate::search::SearchOptions,
    ) -> StorageResult<Vec<i64>> {
        use crate::search::{fuse, title_priority, FuzzySearch};
        let query = query.trim();
        if !options.valid() {
            return Err(DomainError::InvalidArgument.into());
        }
        if query.is_empty() {
            return Ok(Vec::new());
        }
        let lowercase_query = query.to_lowercase();
        let limit = i64::try_from(options.candidate_limit)?;
        let (sql, pattern) = if query.chars().count() < 3 {
            (
                "SELECT id,title FROM notes
                 WHERE instr(lower(title || ' ' || content),lower(?1))>0 ORDER BY id LIMIT ?2",
                query.to_owned(),
            )
        } else {
            (
                "SELECT n.id,n.title FROM cardbe_note_fts JOIN notes n ON n.id=cardbe_note_fts.rowid
                 WHERE cardbe_note_fts MATCH ?1 ORDER BY bm25(cardbe_note_fts),n.id LIMIT ?2",
                format!("\"{}\"", query.replace('"', "\"\"")),
            )
        };
        let fts = self
            .connection
            .prepare(sql)?
            .query_map(params![pattern, limit], |row| {
                Ok((
                    row.get::<_, i64>(0)?,
                    title_priority(&row.get::<_, String>(1)?, &lowercase_query),
                ))
            })?
            .collect::<Result<Vec<_>, _>>()?;
        let mut statement = self.connection.prepare("SELECT id,title FROM notes")?;
        let mut rows = statement.query([])?;
        let mut fuzzy = FuzzySearch::new(query, options.candidate_limit);
        while let Some(row) = rows.next()? {
            fuzzy.push(row.get(0)?, &row.get::<_, String>(1)?);
        }
        Ok(fuse(&fts, &fuzzy.finish(), options))
    }

    pub fn create_note(&mut self, now: i64) -> StorageResult<Note> {
        self.create_note_with_content(now, String::new(), String::new())
    }

    pub fn boards(&self) -> StorageResult<Vec<Board>> {
        Ok(self
            .connection
            .prepare(
                "SELECT b.id, b.name, COUNT(t.id), b.shared_role, b.sync_status, b.sync_revision,
                        b.shared_role <> 'owner' OR EXISTS(SELECT 1 FROM cardbe_iroh_invites i WHERE i.board_id=b.id)
                 FROM cardbe_boards b
                 LEFT JOIN cardbe_tasks t ON t.board_id = b.id
                 GROUP BY b.id, b.name, b.shared_role, b.sync_status, b.sync_revision
                 ORDER BY b.id",
            )?
            .query_map([], |row| {
                Ok(Board {
                    id: row.get(0)?,
                    name: row.get(1)?,
                    task_count: row.get(2)?,
                    shared_role: row.get(3)?,
                    sync_status: row.get(4)?,
                    sync_revision: row.get(5)?,
                    is_shared: row.get(6)?,
                })
            })?
            .collect::<Result<Vec<_>, _>>()?)
    }

    /// Every board's columns in board order, with their task counts.
    pub fn column_overviews(&self) -> StorageResult<Vec<ColumnOverview>> {
        Ok(self
            .connection
            .prepare(
                "SELECT c.board_id, c.id, c.name, c.done, COUNT(t.id)
                 FROM cardbe_columns c
                 LEFT JOIN cardbe_tasks t ON t.board_id = c.board_id AND t.column_id = c.id
                 GROUP BY c.board_id, c.id
                 ORDER BY c.board_id, c.position",
            )?
            .query_map([], |row| {
                Ok(ColumnOverview {
                    board_id: row.get(0)?,
                    id: row.get(1)?,
                    name: row.get(2)?,
                    done: row.get(3)?,
                    task_count: row.get(4)?,
                })
            })?
            .collect::<Result<Vec<_>, _>>()?)
    }

    pub fn initialize_boards(
        &mut self,
        legacy: &StoredData,
    ) -> StorageResult<(Vec<Board>, StoredData)> {
        self.migrate_multiboard_schema(legacy)?;
        let boards = self.boards()?;
        if let Some(board) = boards.first() {
            let requested_active_board_id =
                namespaced_metadata_parse::<i64>(&self.connection, "active_board_id")?;
            let id = requested_active_board_id
                .filter(|id| boards.iter().any(|board| board.id == *id))
                .unwrap_or(board.id);
            let stored = self.load_board(id)?;
            if requested_active_board_id != Some(id) {
                log::warn!(
                    target: "storage.database",
                    "Stored active board {:?} was not selected; using active_board_id={} board_count={}",
                    requested_active_board_id,
                    id,
                    boards.len(),
                );
            }
            log::debug!(
                target: "storage.database",
                "Boards initialized board_count={} active_board_id={} schema_version={} current_schema_version={}",
                boards.len(),
                id,
                stored.schema_version,
                CURRENT_SCHEMA_VERSION,
            );
            return Ok((boards, stored));
        }
        self.connection
            .execute("INSERT INTO cardbe_boards(name) VALUES ('My board')", [])?;
        let id = self.connection.last_insert_rowid();
        self.write_board(id, legacy)?;
        self.set_active_board_id(id)?;
        Ok((
            vec![Board {
                id,
                name: "My board".into(),
                task_count: legacy
                    .columns
                    .iter()
                    .map(|column| column.tasks.len() as i64)
                    .sum(),
                shared_role: BoardRole::Owner,
                is_shared: false,
                sync_status: SyncStatus::Local,
                sync_revision: 0,
            }],
            legacy.clone(),
        ))
    }

    pub fn load_board(&mut self, id: i64) -> StorageResult<StoredData> {
        let stored = self.load_board_data(id, true)?;
        self.positions = self.load_board_positions(id)?;
        self.set_active_board_id(id)?;
        Ok(stored)
    }

    /// Read a board without changing the active-board metadata or cached positions.
    pub fn read_board(&self, id: i64) -> StorageResult<StoredData> {
        self.load_board_data(id, false)
    }

    /// Read every board-owned collection without changing active state.
    pub fn read_board_complete(&self, id: i64) -> StorageResult<StoredData> {
        self.load_board_data(id, true)
    }

    /// Run `read` inside one read transaction so its queries observe a single
    /// committed state. Under WAL this does not block writers.
    pub fn read_consistent<R>(&self, read: impl FnOnce(&Self) -> R) -> StorageResult<R> {
        let _transaction = self.connection.unchecked_transaction()?;
        Ok(read(self))
    }

    /// Read what an Iroh host response needs for one board. Authorization,
    /// revision, Loro document, and board data come from one read transaction,
    /// so a concurrent revocation, local edit, or editor push cannot mix
    /// states, and content committed after a revocation is never returned.
    /// `plan` chooses what to read once permission and revision are known.
    pub fn read_iroh_share(
        &self,
        board_id: i64,
        invite_id: &str,
        secret: &str,
        node_id: &str,
        plan: impl FnOnce(IrohPermission, &Board) -> IrohShareRead,
    ) -> StorageResult<Result<IrohShareContents, IrohShareDenied>> {
        self.read_consistent(|db| -> StorageResult<Result<_, IrohShareDenied>> {
            let approved = db
                .connection
                .query_row(
                    "SELECT d.status FROM cardbe_iroh_devices d JOIN cardbe_iroh_invites i ON i.invite_id=d.invite_id WHERE d.invite_id=?1 AND d.node_id=?2 AND i.enabled=1 AND i.secret=?3",
                    params![invite_id, node_id, secret],
                    |row| row.get::<_, IrohDeviceStatus>(0),
                )
                .optional()?
                == Some(IrohDeviceStatus::Approved);
            if !approved {
                return Ok(Err(IrohShareDenied::DeviceNotApproved));
            }
            let Some(permission) = db
                .connection
                .query_row(
                    "SELECT permission FROM cardbe_iroh_invites WHERE invite_id=?1 AND board_id=?2 AND secret=?3 AND enabled=1",
                    params![invite_id, board_id, secret],
                    |row| row.get::<_, IrohPermission>(0),
                )
                .optional()?
            else {
                return Ok(Err(IrohShareDenied::InvitationUnavailable));
            };
            let Some(board) = db
                .boards()?
                .into_iter()
                .find(|board| board.id == board_id && board.shared_role == BoardRole::Owner)
            else {
                return Ok(Err(IrohShareDenied::BoardDeleted));
            };
            let read = plan(permission, &board);
            let loro_document = if read.loro_document {
                Some(
                    db.iroh_loro_update(board_id)?
                        .ok_or("Editable board is missing its Loro document")?,
                )
            } else {
                None
            };
            let data = if read.data {
                Some(db.load_board_data(board_id, true)?)
            } else {
                None
            };
            Ok(Ok(IrohShareContents {
                board,
                permission,
                loro_document,
                data,
            }))
        })?
    }

    pub fn load_board_archives_for_export(&self, id: i64) -> StorageResult<Vec<Archive>> {
        self.load_board_archives(id)
    }

    pub fn replace_all_boards(
        &mut self,
        boards: &[(String, StoredData, crate::flow::FlowData)],
        active_index: usize,
        notes: &[Note],
        settings: &crate::models::Settings,
        focuses: Option<&[crate::focuses::Focus]>,
    ) -> StorageResult<(Vec<Board>, StoredData)> {
        if boards.is_empty() || active_index >= boards.len() {
            return Err("Backup must contain an active board".into());
        }
        let tx = self.connection.transaction()?;
        tx.execute("DELETE FROM cardbe_boards", [])?;
        tx.execute("DELETE FROM notes", [])?;
        let mut created = Vec::with_capacity(boards.len());
        for (name, data, flow) in boards {
            tx.execute("INSERT INTO cardbe_boards(name) VALUES(?1)", [name])?;
            let id = tx.last_insert_rowid();
            write_board_transaction(&tx, id, data)?;
            crate::flow::restore(&tx, id, flow)?;
            created.push(Board {
                id,
                name: name.clone(),
                task_count: data
                    .columns
                    .iter()
                    .map(|column| column.tasks.len() as i64)
                    .sum(),
                shared_role: BoardRole::Owner,
                is_shared: false,
                sync_status: SyncStatus::Local,
                sync_revision: 0,
            });
        }
        for note in notes {
            tx.execute("INSERT INTO notes(id,title,content,pinned,created_at,updated_at) VALUES(?1,?2,?3,?4,?5,?6)", params![note.id, note.title, note.content, note.pinned, note.created_at, note.updated_at])?;
        }
        let active_id = created[active_index].id;
        tx.execute("INSERT INTO cardbe_global_metadata(key,value) VALUES('active_board_id',?1) ON CONFLICT(key) DO UPDATE SET value=excluded.value", [active_id.to_string()])?;
        tx.execute("INSERT INTO cardbe_global_metadata(key,value) VALUES('settings',?1) ON CONFLICT(key) DO UPDATE SET value=excluded.value", [serde_json::to_string(settings)?])?;
        if let Some(views) = focuses {
            // Backups refer to boards by position; restored boards get new IDs.
            let mut views = views.to_vec();
            crate::focuses::map_boards(&mut views, |index| {
                usize::try_from(index)
                    .ok()
                    .and_then(|index| created.get(index))
                    .map(|board| board.id)
            });
            crate::focuses::save(&tx, &views)?;
        }
        tx.commit()?;
        self.active_board_id = active_id;
        self.loro_peers.clear();
        self.positions = self.load_board_positions(active_id)?;
        let active = self.load_board_data(active_id, true)?;
        Ok((created, active))
    }

    pub fn create_board(&mut self, name: &str) -> StorageResult<Board> {
        // Board creation writes both the row and its scoped metadata. Keep
        // those writes in the same transaction so a storage error cannot
        // leave a selectable, half-initialized board behind.
        self.create_board_with_data(name, &StoredData::default())
    }

    pub fn board_exists(&self, id: i64) -> StorageResult<bool> {
        Ok(self.connection.query_row(
            "SELECT EXISTS(SELECT 1 FROM cardbe_boards WHERE id=?1)",
            [id],
            |row| row.get(0),
        )?)
    }

    pub fn board_role(&self, id: i64) -> StorageResult<BoardRole> {
        self.connection
            .query_row(
                "SELECT shared_role FROM cardbe_boards WHERE id=?1",
                [id],
                |row| row.get(0),
            )
            .optional()?
            .ok_or_else(|| DomainError::BoardNotFound.into())
    }

    #[cfg(test)]
    pub fn set_shared_board(
        &mut self,
        id: i64,
        role: BoardRole,
        status: SyncStatus,
        revision: i64,
    ) -> StorageResult<()> {
        if self.connection.execute(
            "UPDATE cardbe_boards SET shared_role=?2,sync_status=?3,sync_revision=?4 WHERE id=?1",
            params![id, role, status, revision],
        )? == 0
        {
            return Err(DomainError::BoardNotFound.into());
        }
        Ok(())
    }

    /// Iroh capabilities are deliberately database-only metadata.  The JSON
    /// backup format remains portable and must never contain bearer secrets.
    pub fn save_iroh_invite(
        &mut self,
        invite_id: &str,
        board_id: i64,
        secret: &str,
        permission: IrohPermission,
    ) -> StorageResult<()> {
        self.connection.execute("INSERT INTO cardbe_iroh_invites(invite_id,board_id,secret,permission,enabled,created_at) VALUES(?1,?2,?3,?4,1,datetime('now')) ON CONFLICT(invite_id) DO UPDATE SET board_id=excluded.board_id,secret=excluded.secret,permission=excluded.permission", params![invite_id, board_id, secret, permission])?; // gitleaks:allow
        Ok(())
    }
    pub fn iroh_device_access(
        &mut self,
        invite_id: &str,
        secret: &str,
        node_id: &str,
        request_approval: bool,
    ) -> StorageResult<Option<IrohDeviceStatus>> {
        self.connection.execute(
            "INSERT INTO cardbe_iroh_devices(invite_id,node_id,status) SELECT invite_id,?3,0 FROM cardbe_iroh_invites WHERE invite_id=?1 AND secret=?2 AND enabled=1 ON CONFLICT(invite_id,node_id) DO NOTHING",
            params![invite_id, secret, node_id],
        )?;
        if request_approval {
            self.connection.execute(
                "UPDATE cardbe_iroh_devices SET status=?3 WHERE invite_id=?1 AND node_id=?2 AND status=?4 AND EXISTS(SELECT 1 FROM cardbe_iroh_invites WHERE invite_id=?1 AND secret=?5 AND enabled=1)",
                params![invite_id, node_id, IrohDeviceStatus::Pending, IrohDeviceStatus::Revoked, secret],
            )?;
        }
        Ok(self.connection.query_row(
            "SELECT d.status FROM cardbe_iroh_devices d JOIN cardbe_iroh_invites i ON i.invite_id=d.invite_id WHERE d.invite_id=?1 AND d.node_id=?2 AND i.enabled=1 AND i.secret=?3",
            params![invite_id, node_id, secret], |r| r.get::<_, IrohDeviceStatus>(0),
        ).optional()?)
    }
    pub fn iroh_devices(&self) -> StorageResult<Vec<(String, String, IrohDeviceStatus)>> {
        Ok(self.connection.prepare("SELECT invite_id,node_id,status FROM cardbe_iroh_devices WHERE status!=?1 ORDER BY status,invite_id,node_id")?
            .query_map([IrohDeviceStatus::Revoked], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?
            .collect::<Result<Vec<_>, _>>()?)
    }
    pub fn set_iroh_device_approved(
        &mut self,
        invite_id: &str,
        node_id: &str,
        approved: bool,
    ) -> StorageResult<()> {
        let status = if approved {
            IrohDeviceStatus::Approved
        } else {
            IrohDeviceStatus::Revoked
        };
        if self.connection.execute(
            "UPDATE cardbe_iroh_devices SET status=?3 WHERE invite_id=?1 AND node_id=?2",
            params![invite_id, node_id, status],
        )? == 0
        {
            return Err(crate::errors::DomainError::DeviceRequestNotFound.into());
        }
        Ok(())
    }
    pub fn iroh_remote(&self, board_id: i64) -> StorageResult<Option<String>> {
        Ok(self
            .connection
            .query_row(
                "SELECT invitation FROM cardbe_iroh_remotes WHERE board_id=?1",
                [board_id],
                |r| r.get(0),
            )
            .optional()?)
    }
    pub fn update_iroh_remote(&self, board_id: i64, invitation: &str) -> StorageResult<()> {
        if self.connection.execute(
            "UPDATE cardbe_iroh_remotes SET invitation=?2 WHERE board_id=?1",
            params![board_id, invitation],
        )? == 0
        {
            return Err(DomainError::InviteNotFound.into());
        }
        Ok(())
    }
    pub fn iroh_loro_update(&self, board_id: i64) -> StorageResult<Option<Vec<u8>>> {
        Ok(self
            .connection
            .query_row(
                "SELECT payload FROM cardbe_loro_docs WHERE board_id=?1",
                [board_id],
                |r| r.get(0),
            )
            .optional()?)
    }
    pub fn ensure_iroh_loro_doc(&mut self, board_id: i64) -> StorageResult<Vec<u8>> {
        if let Some(update) = self.iroh_loro_update(board_id)? {
            return Ok(update);
        }
        let data = self.read_board_complete(board_id)?;
        let update = crate::loro_board::apply_local_delta_with_peer(
            None,
            &StoredData::default(),
            &data,
            self.loro_peers.entry(board_id).or_default(),
        )
        .map_err(|e| format!("Could not seed shared-board CRDT: {e}"))?;
        self.connection.execute(
            "INSERT INTO cardbe_loro_docs(board_id,payload) VALUES(?1,?2)",
            params![board_id, update],
        )?;
        self.iroh_loro_update(board_id)?
            .ok_or_else(|| "Could not load seeded shared-board CRDT".into())
    }
    /// Applies an editor's Loro update atomically with the SQL projection.
    #[cfg(test)]
    pub fn apply_iroh_loro_update(
        &mut self,
        board_id: i64,
        invite_id: &str,
        secret: &str,
        node_id: &str,
        update: &[u8],
    ) -> StorageResult<(bool, i64, StoredData)> {
        loop {
            let prepared =
                self.prepare_iroh_loro_update(board_id, invite_id, secret, node_id, update)?;
            if let Some(result) =
                self.commit_iroh_loro_update(prepared, invite_id, secret, node_id)?
            {
                return Ok(result);
            }
        }
    }

    /// Performs the CPU-bound half of an editor push: merge, projection, and
    /// size validation. It writes nothing, so callers can run it without
    /// holding application state and commit it afterwards.
    pub fn prepare_iroh_loro_update(
        &self,
        board_id: i64,
        invite_id: &str,
        secret: &str,
        node_id: &str,
        update: &[u8],
    ) -> StorageResult<PreparedLoroUpdate> {
        ensure_iroh_editor(&self.connection, board_id, invite_id, secret, node_id)?;
        let base = editable_loro_doc(&self.connection, board_id)?;
        let merged = crate::loro_board::merge(Some(&base), update)
            .map_err(|e| format!("Invalid Loro update: {e}"))?;
        // Snapshot bytes can differ after re-export even when the Loro
        // operation history is unchanged. Use its version vector for retries.
        if crate::loro_board::same_state_vector(&base, &merged)
            .map_err(|e| format!("Invalid Loro version vector: {e}"))?
        {
            return Ok(PreparedLoroUpdate {
                board_id,
                base,
                merged: None,
            });
        }
        let projected = crate::loro_board::project(&merged)
            .map_err(|e| format!("Invalid Loro projection: {e}"))?;
        validate_remote_board_size(Some(&self.read_board_complete(board_id)?), &projected)?;
        Ok(PreparedLoroUpdate {
            board_id,
            base,
            merged: Some((merged, projected)),
        })
    }

    /// Commits a prepared editor push. Returns `None` without writing when
    /// the board's Loro document changed after preparation, so the caller can
    /// prepare again from the current document. The capability is checked
    /// inside this transaction so revocation wins even when it races an
    /// already accepted Iroh connection.
    pub fn commit_iroh_loro_update(
        &mut self,
        prepared: PreparedLoroUpdate,
        invite_id: &str,
        secret: &str,
        node_id: &str,
    ) -> StorageResult<Option<(bool, i64, StoredData)>> {
        let board_id = prepared.board_id;
        let previous = self.read_board_complete(board_id)?;
        let tx = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        ensure_iroh_editor(&tx, board_id, invite_id, secret, node_id)?;
        if editable_loro_doc(&tx, board_id)? != prepared.base {
            return Ok(None);
        }
        let revision: i64 = tx.query_row(
            "SELECT sync_revision FROM cardbe_boards WHERE id=?1 AND shared_role=?2",
            params![board_id, BoardRole::Owner],
            |row| row.get(0),
        )?;
        let Some((merged, projected)) = prepared.merged else {
            tx.commit()?;
            return Ok(Some((false, revision, previous)));
        };
        let mut data = previous.clone();
        data.columns = projected.columns;
        data.archives = projected.archives;
        data.templates = projected.templates;
        write_board_transaction(&tx, board_id, &data)?;
        task_events::record(&tx, board_id, &previous, &data, &EventSource::remote())?;
        tx.execute("INSERT INTO cardbe_loro_docs(board_id,payload) VALUES(?1,?2) ON CONFLICT(board_id) DO UPDATE SET payload=excluded.payload", params![board_id, merged])?;
        tx.execute(
            "UPDATE cardbe_boards SET sync_revision=sync_revision+1,sync_status=?2 WHERE id=?1",
            params![board_id, SyncStatus::Synced],
        )?;
        tx.commit()?;
        Ok(Some((true, revision + 1, data)))
    }
    /// Client-side counterpart: merge an owner-approved diff into the local
    /// document and atomically refresh only the CRDT-owned projection.
    pub fn merge_iroh_loro_diff(
        &mut self,
        board_id: i64,
        update: &[u8],
        name: &str,
        role: IrohPermission,
        revision: i64,
        sent: &[u8],
    ) -> StorageResult<bool> {
        let previous = self.read_board_complete(board_id)?;
        let current = self
            .iroh_loro_update(board_id)?
            .ok_or("Editable board is missing its Loro document")?;
        let changed_during_sync = current != sent;
        let merged = crate::loro_board::merge(Some(&current), update)
            .map_err(|e| format!("Invalid Loro update: {e}"))?;
        let projected = crate::loro_board::project(&merged)
            .map_err(|e| format!("Invalid Loro projection: {e}"))?;
        validate_remote_board_size(Some(&previous), &projected)?;
        let content_changed = previous.columns != projected.columns
            || previous.archives != projected.archives
            || previous.templates != projected.templates
            || self.board_role(board_id)? != BoardRole::from(role);
        let mut data = previous.clone();
        data.columns = projected.columns;
        data.archives = projected.archives;
        data.templates = projected.templates;
        let tx = self.connection.transaction()?;
        if content_changed {
            write_board_transaction(&tx, board_id, &data)?;
            task_events::record(&tx, board_id, &previous, &data, &EventSource::remote())?;
        }
        if !crate::loro_board::same_state_vector(&current, &merged)? {
            tx.execute("INSERT INTO cardbe_loro_docs(board_id,payload) VALUES(?1,?2) ON CONFLICT(board_id) DO UPDATE SET payload=excluded.payload", params![board_id, merged])?;
        }
        tx.execute("UPDATE cardbe_boards SET name=?2,shared_role=?3,sync_revision=?4,sync_status=?5 WHERE id=?1", params![board_id, name, role, revision, if changed_during_sync { SyncStatus::Pending } else { SyncStatus::Synced }])?;
        tx.commit()?;
        if self.active_board_id == board_id {
            self.positions = self.load_board_positions(board_id)?;
        }
        Ok(content_changed)
    }

    pub fn save_iroh_conflict(
        &mut self,
        board_id: i64,
        revision: i64,
        name: &str,
        permission: IrohPermission,
        data: &StoredData,
        loro_update: Option<&[u8]>,
    ) -> StorageResult<()> {
        validate_iroh_snapshot(data, permission, loro_update)?;
        let tx = self.connection.transaction()?;
        tx.execute("INSERT INTO cardbe_iroh_conflicts(board_id,revision,name,permission,payload,loro_update) VALUES(?1,?2,?3,?4,?5,?6) ON CONFLICT(board_id) DO UPDATE SET revision=excluded.revision,name=excluded.name,permission=excluded.permission,payload=excluded.payload,loro_update=excluded.loro_update", params![board_id, revision, name, permission, serde_json::to_string(data)?, loro_update])?;
        tx.execute(
            "UPDATE cardbe_boards SET sync_status=?2,shared_role=?3 WHERE id=?1",
            params![board_id, SyncStatus::Conflict, BoardRole::from(permission)],
        )?;
        tx.commit()?;
        Ok(())
    }

    fn initialize_task_search_index(&self) -> StorageResult<()> {
        let index_exists = self.connection.query_row(
            "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name='cardbe_task_search')",
            [],
            |row| row.get::<_, bool>(0),
        )?;
        self.connection.execute_batch(
            "CREATE TABLE IF NOT EXISTS cardbe_task_search(
                 rowid INTEGER PRIMARY KEY,
                 board_id INTEGER NOT NULL,
                 task_id INTEGER NOT NULL,
                 archived INTEGER NOT NULL,
                 body TEXT NOT NULL,
                 UNIQUE(board_id,task_id,archived)
             );
             CREATE VIRTUAL TABLE IF NOT EXISTS cardbe_task_fts USING fts5(
                 body,content='cardbe_task_search',content_rowid='rowid',tokenize='trigram'
             );
             CREATE TRIGGER IF NOT EXISTS cardbe_task_search_ai AFTER INSERT ON cardbe_task_search BEGIN
                 INSERT INTO cardbe_task_fts(rowid,body) VALUES(new.rowid,new.body);
             END;
             CREATE TRIGGER IF NOT EXISTS cardbe_task_search_ad AFTER DELETE ON cardbe_task_search BEGIN
                 INSERT INTO cardbe_task_fts(cardbe_task_fts,rowid,body) VALUES('delete',old.rowid,old.body);
             END;
             CREATE TRIGGER IF NOT EXISTS cardbe_task_search_au AFTER UPDATE ON cardbe_task_search BEGIN
                 INSERT INTO cardbe_task_fts(cardbe_task_fts,rowid,body) VALUES('delete',old.rowid,old.body);
                 INSERT INTO cardbe_task_fts(rowid,body) VALUES(new.rowid,new.body);
             END;
             CREATE VIEW IF NOT EXISTS cardbe_task_search_payloads AS
                 SELECT board_id,id AS task_id,0 AS archived,payload FROM cardbe_tasks
                 UNION ALL
                 SELECT board_id,task_id,1 AS archived,payload FROM cardbe_archives;
             CREATE VIEW IF NOT EXISTS cardbe_task_search_content AS
                 SELECT board_id,task_id,archived,
                     COALESCE(json_extract(payload,'$.title'),'') || ' ' ||
                     COALESCE(json_extract(payload,'$.description'),'') || ' ' ||
                     COALESCE((SELECT group_concat(value,' ') FROM json_each(payload,'$.labels')),'') || ' ' ||
                     COALESCE((SELECT group_concat(json_extract(value,'$.text'),' ') FROM json_each(payload,'$.items')),'') AS body
                 FROM cardbe_task_search_payloads;
             CREATE TRIGGER IF NOT EXISTS cardbe_tasks_ai AFTER INSERT ON cardbe_tasks BEGIN
                 INSERT INTO cardbe_task_search(board_id,task_id,archived,body)
                 SELECT board_id,task_id,archived,body FROM cardbe_task_search_content
                 WHERE board_id=new.board_id AND task_id=new.id AND archived=0
                 ON CONFLICT(board_id,task_id,archived) DO UPDATE SET body=excluded.body;
             END;
             CREATE TRIGGER IF NOT EXISTS cardbe_tasks_ad AFTER DELETE ON cardbe_tasks BEGIN
                 DELETE FROM cardbe_task_search WHERE board_id=old.board_id AND task_id=old.id AND archived=0;
             END;
             CREATE TRIGGER IF NOT EXISTS cardbe_tasks_au AFTER UPDATE OF board_id,id,payload ON cardbe_tasks BEGIN
                 DELETE FROM cardbe_task_search WHERE board_id=old.board_id AND task_id=old.id AND archived=0;
                 INSERT INTO cardbe_task_search(board_id,task_id,archived,body)
                 SELECT board_id,task_id,archived,body FROM cardbe_task_search_content
                 WHERE board_id=new.board_id AND task_id=new.id AND archived=0
                 ON CONFLICT(board_id,task_id,archived) DO UPDATE SET body=excluded.body;
             END;
             CREATE TRIGGER IF NOT EXISTS cardbe_archives_ai AFTER INSERT ON cardbe_archives BEGIN
                 INSERT INTO cardbe_task_search(board_id,task_id,archived,body)
                 SELECT board_id,task_id,archived,body FROM cardbe_task_search_content
                 WHERE board_id=new.board_id AND task_id=new.task_id AND archived=1
                 ON CONFLICT(board_id,task_id,archived) DO UPDATE SET body=excluded.body;
             END;
             CREATE TRIGGER IF NOT EXISTS cardbe_archives_ad AFTER DELETE ON cardbe_archives BEGIN
                 DELETE FROM cardbe_task_search WHERE board_id=old.board_id AND task_id=old.task_id AND archived=1;
             END;
             CREATE TRIGGER IF NOT EXISTS cardbe_archives_au AFTER UPDATE OF board_id,task_id,payload ON cardbe_archives BEGIN
                 DELETE FROM cardbe_task_search WHERE board_id=old.board_id AND task_id=old.task_id AND archived=1;
                 INSERT INTO cardbe_task_search(board_id,task_id,archived,body)
                 SELECT board_id,task_id,archived,body FROM cardbe_task_search_content
                 WHERE board_id=new.board_id AND task_id=new.task_id AND archived=1
                 ON CONFLICT(board_id,task_id,archived) DO UPDATE SET body=excluded.body;
             END;",
        )?;
        if !index_exists {
            self.connection.execute_batch(
                "INSERT INTO cardbe_task_search(board_id,task_id,archived,body)
                 SELECT board_id,task_id,archived,body FROM cardbe_task_search_content WHERE 1;",
            )?;
        }
        Ok(())
    }

    pub fn iroh_conflict(&self, board_id: i64) -> StorageResult<Option<IrohConflict>> {
        let raw = self.connection.query_row(
            "SELECT revision,name,permission,payload,loro_update FROM cardbe_iroh_conflicts WHERE board_id=?1",
            [board_id], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?, r.get::<_, IrohPermission>(2)?, r.get::<_, String>(3)?, r.get::<_, Option<Vec<u8>>>(4)?)),
        ).optional()?;
        raw.map(|(revision, name, permission, payload, loro_update)| {
            Ok(IrohConflict {
                revision,
                name,
                permission,
                data: serde_json::from_str(&payload)?,
                loro_update,
            })
        })
        .transpose()
    }

    pub fn keep_iroh_conflict_local(&mut self, board_id: i64) -> StorageResult<()> {
        let conflict = self
            .iroh_conflict(board_id)?
            .ok_or("No saved sync conflict")?;
        if conflict.permission != IrohPermission::Editor {
            return Err("The invitation is read-only; save your changes as a copy instead".into());
        }
        validate_iroh_snapshot(
            &conflict.data,
            conflict.permission,
            conflict.loro_update.as_deref(),
        )?;
        let local = self.read_board_complete(board_id)?;
        let mut peer = crate::loro_board::LocalPeer::default();
        let rebased = crate::loro_board::apply_local_delta_with_peer(
            conflict.loro_update.as_deref(),
            &conflict.data,
            &local,
            &mut peer,
        )
        .map_err(|error| format!("Could not rebase local changes: {error}"))?;
        let tx = self.connection.transaction()?;
        tx.execute("INSERT INTO cardbe_loro_docs(board_id,payload) VALUES(?1,?2) ON CONFLICT(board_id) DO UPDATE SET payload=excluded.payload", params![board_id, rebased])?;
        tx.execute(
            "UPDATE cardbe_boards SET shared_role=?2,sync_status=?3,sync_revision=?4 WHERE id=?1",
            params![
                board_id,
                BoardRole::Editor,
                SyncStatus::Pending,
                conflict.revision
            ],
        )?;
        tx.execute(
            "DELETE FROM cardbe_iroh_conflicts WHERE board_id=?1",
            [board_id],
        )?;
        tx.commit()?;
        self.loro_peers.insert(board_id, peer);
        Ok(())
    }

    pub fn board_sync_state(&self, id: i64) -> StorageResult<(i64, SyncStatus)> {
        Ok(self.connection.query_row(
            "SELECT sync_revision,sync_status FROM cardbe_boards WHERE id=?1",
            [id],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )?)
    }
    pub fn iroh_remote_tickets(&self) -> StorageResult<Vec<String>> {
        Ok(self
            .connection
            .prepare("SELECT invitation FROM cardbe_iroh_remotes")?
            .query_map([], |row| row.get(0))?
            .collect::<Result<Vec<_>, _>>()?)
    }
    pub fn iroh_invites(&self) -> StorageResult<Vec<IrohInviteRecord>> {
        Ok(self
            .connection
            .prepare(
                "SELECT invite_id,board_id,secret,permission,enabled FROM cardbe_iroh_invites",
            )?
            .query_map([], |r| {
                Ok((
                    r.get(0)?,
                    r.get(1)?,
                    r.get(2)?,
                    r.get(3)?,
                    r.get::<_, i64>(4)? != 0,
                ))
            })?
            .collect::<Result<Vec<_>, _>>()?)
    }
    pub fn iroh_invite(&self, invite_id: &str) -> StorageResult<Option<IrohInviteRecord>> {
        Ok(self
            .connection
            .query_row(
                "SELECT invite_id,board_id,secret,permission,enabled FROM cardbe_iroh_invites WHERE invite_id=?1",
                [invite_id],
                |r| {
                    Ok((
                        r.get(0)?,
                        r.get(1)?,
                        r.get(2)?,
                        r.get(3)?,
                        r.get::<_, i64>(4)? != 0,
                    ))
                },
            )
            .optional()?)
    }
    pub fn iroh_invite_summaries(&self) -> StorageResult<Vec<IrohInviteSummaryRecord>> {
        Ok(self.connection.prepare("SELECT invite_id,board_id,permission,enabled,created_at,secret FROM cardbe_iroh_invites ORDER BY created_at DESC")?
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get::<_, i64>(3)? != 0, r.get(4)?, r.get(5)?)))?
            .collect::<Result<Vec<_>, _>>()?)
    }
    pub fn update_iroh_invite(
        &mut self,
        invite_id: &str,
        permission: Option<IrohPermission>,
        enabled: Option<bool>,
    ) -> StorageResult<()> {
        if self.connection.execute(
            "UPDATE cardbe_iroh_invites SET permission=COALESCE(?2,permission),enabled=COALESCE(?3,enabled) WHERE invite_id=?1",
            params![invite_id, permission, enabled.map(i64::from)],
        )? == 0 {
            return Err(crate::errors::DomainError::InviteNotFound.into());
        }
        Ok(())
    }
    pub fn delete_iroh_invite(&mut self, invite_id: &str) -> StorageResult<()> {
        if self.connection.execute(
            "DELETE FROM cardbe_iroh_invites WHERE invite_id=?1",
            [invite_id],
        )? == 0
        {
            return Err(crate::errors::DomainError::InviteNotFound.into());
        }
        Ok(())
    }
    /// Deleted invitations still need an endpoint to report revocation to offline peers.
    // ponytail: retain the endpoint indefinitely; retire it only with peer acknowledgements.
    pub fn iroh_host_required(&self) -> StorageResult<bool> {
        Ok(self.connection.query_row(
            "SELECT EXISTS(SELECT 1 FROM cardbe_iroh_invites WHERE enabled=1) OR EXISTS(SELECT 1 FROM cardbe_global_metadata WHERE key='iroh_has_revocations')",
            [],
            |row| row.get(0),
        )?)
    }

    pub fn iroh_endpoint_seed(&mut self) -> StorageResult<String> {
        if let Some(seed) = self
            .connection
            .query_row(
                "SELECT value FROM cardbe_global_metadata WHERE key='iroh_endpoint_seed'",
                [],
                |r| r.get(0),
            )
            .optional()?
        {
            return Ok(seed);
        }
        let seed = base64::engine::general_purpose::URL_SAFE_NO_PAD
            .encode(iroh::SecretKey::generate().to_bytes());
        self.connection.execute(
            "INSERT INTO cardbe_global_metadata(key,value) VALUES('iroh_endpoint_seed',?1)",
            [&seed],
        )?;
        Ok(seed)
    }

    /// The last automatically selected UDP port for this device's sharing
    /// endpoint, so peers' saved invitation addresses stay valid after restart.
    pub fn iroh_automatic_port(&self) -> StorageResult<Option<u16>> {
        Ok(
            namespaced_metadata_value(&self.connection, "iroh_automatic_port")?
                .and_then(|value| value.parse().ok())
                .filter(|port| *port != 0),
        )
    }

    pub fn set_iroh_automatic_port(&mut self, port: u16) -> StorageResult<()> {
        self.connection.execute(
            "INSERT INTO cardbe_global_metadata(key,value) VALUES('iroh_automatic_port',?1)
             ON CONFLICT(key) DO UPDATE SET value=excluded.value",
            [port.to_string()],
        )?;
        Ok(())
    }
    /// Every existing column as `(board ID, column ID)`.
    pub fn column_ids(&self) -> StorageResult<std::collections::HashSet<(i64, i64)>> {
        let mut stmt = self
            .connection
            .prepare("SELECT board_id,id FROM cardbe_columns")?;
        let ids = stmt
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?
            .collect::<Result<_, _>>()?;
        Ok(ids)
    }

    /// Unarchived tasks with a due time in `[from, to)`, for due notifications.
    pub fn tasks_due_between(&self, from: u128, to: u128) -> StorageResult<Vec<DueTask>> {
        let bound = |time: u128| i64::try_from(time).unwrap_or(i64::MAX);
        let mut stmt = self.connection.prepare(
            // Tasks in a done column are completed and need no reminder.
            "SELECT t.board_id,b.name,t.column_id,t.payload
             FROM cardbe_tasks t JOIN cardbe_boards b ON b.id=t.board_id
             JOIN cardbe_columns c ON c.board_id=t.board_id AND c.id=t.column_id
             WHERE c.done=0 AND json_extract(t.payload,'$.due_time')>=?1
               AND json_extract(t.payload,'$.due_time')<?2
             ORDER BY t.board_id,t.column_id,t.position,t.id",
        )?;
        let rows = stmt.query_map([bound(from), bound(to)], |r| {
            Ok((
                r.get::<_, i64>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, i64>(2)?,
                r.get::<_, String>(3)?,
            ))
        })?;
        let mut tasks = Vec::new();
        for row in rows {
            let (board_id, board_name, column_id, payload) = row?;
            tasks.push(DueTask {
                board_id,
                board_name,
                column_id,
                task: serde_json::from_str(&payload)?,
            });
        }
        Ok(tasks)
    }

    /// Due times before this instant (milliseconds since the Unix epoch) were
    /// already checked for notifications on this device.
    pub fn notifications_checked_until(&self) -> StorageResult<Option<u128>> {
        Ok(
            namespaced_metadata_value(&self.connection, "notifications_checked_until")?
                .and_then(|value| value.parse().ok()),
        )
    }

    pub fn set_notifications_checked_until(&mut self, time: u128) -> StorageResult<()> {
        self.connection.execute(
            "INSERT INTO cardbe_global_metadata(key,value) VALUES('notifications_checked_until',?1)
             ON CONFLICT(key) DO UPDATE SET value=excluded.value",
            [time.to_string()],
        )?;
        Ok(())
    }
    pub fn apply_iroh_snapshot(
        &mut self,
        board_id: i64,
        name: &str,
        role: IrohPermission,
        revision: i64,
        data: &StoredData,
        loro_update: Option<&[u8]>,
    ) -> StorageResult<()> {
        validate_iroh_snapshot(data, role, loro_update)?;
        let previous = self.read_board_complete(board_id)?;
        let tx = self.connection.transaction()?;
        if tx
            .query_row("SELECT 1 FROM cardbe_boards WHERE id=?1", [board_id], |r| {
                r.get::<_, i64>(0)
            })
            .optional()?
            .is_none()
        {
            return Err(DomainError::BoardNotFound.into());
        }
        write_board_transaction(&tx, board_id, data)?;
        task_events::record(&tx, board_id, &previous, data, &EventSource::remote())?;
        if let Some(update) = loro_update.filter(|bytes| !bytes.is_empty()) {
            tx.execute("INSERT INTO cardbe_loro_docs(board_id,payload) VALUES(?1,?2) ON CONFLICT(board_id) DO UPDATE SET payload=excluded.payload", params![board_id, update])?;
        } else {
            tx.execute("DELETE FROM cardbe_loro_docs WHERE board_id=?1", [board_id])?;
        }
        tx.execute("UPDATE cardbe_boards SET name=?2,shared_role=?3,sync_status=?4,sync_revision=?5 WHERE id=?1", params![board_id, name, role, SyncStatus::Synced, revision])?;
        tx.execute(
            "DELETE FROM cardbe_iroh_conflicts WHERE board_id=?1",
            [board_id],
        )?;
        tx.commit()?;
        self.loro_peers.remove(&board_id);
        if self.active_board_id == board_id {
            self.positions = self.load_board_positions(board_id)?;
        }
        Ok(())
    }

    pub fn use_iroh_conflict_remote(
        &mut self,
        board_id: i64,
        local_name: &str,
        local: &StoredData,
    ) -> StorageResult<Board> {
        let conflict = self
            .iroh_conflict(board_id)?
            .ok_or("No saved sync conflict")?;
        validate_iroh_snapshot(
            &conflict.data,
            conflict.permission,
            conflict.loro_update.as_deref(),
        )?;
        let previous = self.read_board_complete(board_id)?;
        let tx = self.connection.transaction()?;
        let copy_name = format!("{local_name} (conflict copy)");
        tx.execute("INSERT INTO cardbe_boards(name) VALUES(?1)", [&copy_name])?;
        let copy_id = tx.last_insert_rowid();
        write_board_transaction(&tx, copy_id, local)?;
        write_board_transaction(&tx, board_id, &conflict.data)?;
        task_events::record(
            &tx,
            board_id,
            &previous,
            &conflict.data,
            &EventSource::remote(),
        )?;
        if let Some(update) = conflict
            .loro_update
            .as_deref()
            .filter(|bytes| !bytes.is_empty())
        {
            tx.execute("INSERT INTO cardbe_loro_docs(board_id,payload) VALUES(?1,?2) ON CONFLICT(board_id) DO UPDATE SET payload=excluded.payload", params![board_id, update])?;
        } else {
            tx.execute("DELETE FROM cardbe_loro_docs WHERE board_id=?1", [board_id])?;
        }
        tx.execute("UPDATE cardbe_boards SET name=?2,shared_role=?3,sync_status=?4,sync_revision=?5 WHERE id=?1", params![board_id, conflict.name, conflict.permission, SyncStatus::Synced, conflict.revision])?;
        tx.execute(
            "DELETE FROM cardbe_iroh_conflicts WHERE board_id=?1",
            [board_id],
        )?;
        tx.commit()?;
        self.loro_peers.remove(&board_id);
        if self.active_board_id == board_id {
            self.positions = self.load_board_positions(board_id)?;
        }
        Ok(Board {
            id: copy_id,
            name: copy_name,
            task_count: local
                .columns
                .iter()
                .map(|column| column.tasks.len() as i64)
                .sum(),
            shared_role: BoardRole::Owner,
            is_shared: false,
            sync_status: SyncStatus::Local,
            sync_revision: 0,
        })
    }

    pub fn copy_task_template_to_board(
        &mut self,
        source_id: i64,
        template_id: i64,
        target_id: i64,
    ) -> StorageResult<()> {
        if !self.board_exists(source_id)? || !self.board_exists(target_id)? {
            return Err(DomainError::BoardNotFound.into());
        }
        if source_id == target_id {
            return Err(DomainError::InvalidArgument.into());
        }
        let source = self.read_board_complete(source_id)?;
        let mut template = source
            .templates
            .into_iter()
            .find(|template| template.id == template_id)
            .ok_or(DomainError::TemplateNotFound)?;
        let mut target = self.read_board_complete(target_id)?;
        template.id = target.allocate_template_id()?;
        target.templates.push(template);
        self.replace_board_as_local_edit(target_id, &target)?;
        Ok(())
    }

    /// Moves the tasks in source board order; any missing task aborts the move.
    pub fn move_tasks_to_board(
        &mut self,
        source_id: i64,
        task_ids: &[i64],
        target_id: i64,
        column_id: i64,
    ) -> StorageResult<StoredData> {
        if !self.board_exists(source_id)? || !self.board_exists(target_id)? {
            return Err(DomainError::BoardNotFound.into());
        }
        if source_id == target_id {
            return Err("Choose a different board".into());
        }
        let before_source = self.read_board_complete(source_id)?;
        let before_target = self.read_board_complete(target_id)?;
        let mut source = before_source.clone();
        let mut target = before_target.clone();
        let column = target
            .columns
            .iter()
            .position(|c| c.id == column_id)
            .ok_or(DomainError::ColumnNotFound)?;
        let selected: std::collections::HashSet<i64> = task_ids.iter().copied().collect();
        let mut moved = Vec::with_capacity(selected.len());
        for source_column in &mut source.columns {
            let (tasks, kept): (Vec<Task>, Vec<Task>) = std::mem::take(&mut source_column.tasks)
                .into_iter()
                .partition(|task| selected.contains(&task.id));
            source_column.tasks = kept;
            moved.extend(tasks);
        }
        if moved.len() != selected.len() {
            return Err(DomainError::TaskNotFound.into());
        }
        for mut task in moved {
            task.id = target.allocate_task_id()?;
            // Tasks only wait on tasks of their own board.
            task.blocked_by.clear();
            for label in &task.labels {
                target.touch_label(label.clone());
            }
            target.columns[column].tasks.push(task);
        }
        let now = crate::commands::archive::current_time_millis()?;
        crate::completion::reconcile(&before_target, &mut target, now)?;
        target.keep_pinned_tasks_first();
        let tx = self.connection.transaction()?;
        for (id, before, after) in [
            (source_id, &before_source, &source),
            (target_id, &before_target, &target),
        ] {
            let role: BoardRole = tx.query_row(
                "SELECT shared_role FROM cardbe_boards WHERE id=?1",
                [id],
                |r| r.get(0),
            )?;
            if role == BoardRole::Viewer {
                return Err(DomainError::PermissionDenied.into());
            }
            write_board_transaction(&tx, id, after)?;
            mark_local_board_change(&tx, id, role)?;
            persist_loro_local_delta(
                &tx,
                id,
                before,
                after,
                self.loro_peers.entry(id).or_default(),
            )?;
            task_events::record_transfer(&tx, id, before, after)?;
        }
        tx.commit()?;
        self.positions = self.load_board_positions(self.active_board_id)?;
        Ok(source)
    }

    pub fn replace_board_as_local_edit(
        &mut self,
        board_id: i64,
        data: &StoredData,
    ) -> StorageResult<(i64, SyncStatus)> {
        Ok(self.replace_board_as_local_edit_recorded(board_id, data)?.0)
    }

    /// Like [`Self::replace_board_as_local_edit`], also returning the task
    /// event operation it recorded.
    pub fn replace_board_as_local_edit_recorded(
        &mut self,
        board_id: i64,
        data: &StoredData,
    ) -> StorageResult<((i64, SyncStatus), Option<i64>)> {
        // The Loro delta must compare with the persisted board, not an empty
        // value: otherwise removals are absent from the CRDT and reappear on
        // the next editor sync.
        let before = self.read_board_complete(board_id)?;
        let tx = self.connection.transaction()?;
        let role: BoardRole = tx.query_row(
            "SELECT shared_role FROM cardbe_boards WHERE id=?1",
            [board_id],
            |r| r.get(0),
        )?;
        if role == BoardRole::Viewer {
            return Err(DomainError::PermissionDenied.into());
        }
        write_board_transaction(&tx, board_id, data)?;
        mark_local_board_change(&tx, board_id, role)?;
        persist_loro_local_delta(
            &tx,
            board_id,
            &before,
            data,
            self.loro_peers.entry(board_id).or_default(),
        )?;
        let event_op = task_events::record(&tx, board_id, &before, data, &EventSource::local())?;
        tx.commit()?;
        if self.active_board_id == board_id {
            self.positions = self.load_board_positions(board_id)?;
        }
        Ok((self.board_sync_state(board_id)?, event_op))
    }

    /// Create and populate a board atomically.
    pub fn create_board_with_data(
        &mut self,
        name: &str,
        data: &StoredData,
    ) -> StorageResult<Board> {
        self.create_board_with_backup(name, data, None, &[])
    }

    /// Board data, all Flow tabs and referenced notes commit together.
    pub fn create_board_with_backup(
        &mut self,
        name: &str,
        data: &StoredData,
        flow: Option<&crate::flow::FlowData>,
        notes: &[Note],
    ) -> StorageResult<Board> {
        if flow.is_none() && !notes.is_empty() {
            return Err(DomainError::InvalidArgument.into());
        }
        let tx = self.connection.transaction()?;
        tx.execute("INSERT INTO cardbe_boards(name) VALUES (?1)", [name])?;
        let id = tx.last_insert_rowid();
        write_board_transaction(&tx, id, data)?;
        if let Some(flow) = flow {
            restore_single_board_flow(&tx, id, flow, notes)?;
        }
        persist_loro_local_delta(
            &tx,
            id,
            &StoredData::default(),
            data,
            self.loro_peers.entry(id).or_default(),
        )?;
        tx.commit()?;
        Ok(Board {
            id,
            name: name.into(),
            task_count: data
                .columns
                .iter()
                .map(|column| column.tasks.len() as i64)
                .sum(),
            shared_role: BoardRole::Owner,
            is_shared: false,
            sync_status: SyncStatus::Local,
            sync_revision: 0,
        })
    }

    pub fn rename_board(&mut self, id: i64, name: &str) -> StorageResult<()> {
        let tx = self.connection.transaction()?;
        let role: BoardRole = tx.query_row(
            "SELECT shared_role FROM cardbe_boards WHERE id=?1",
            [id],
            |r| r.get(0),
        )?;
        if role != BoardRole::Owner {
            return Err(DomainError::PermissionDenied.into());
        }
        tx.execute(
            "UPDATE cardbe_boards SET name=?2 WHERE id=?1",
            params![id, name],
        )?;
        mark_local_board_change(&tx, id, role)?;
        tx.commit()?;
        Ok(())
    }

    pub fn create_received_board(
        &mut self,
        name: &str,
        data: &StoredData,
        role: IrohPermission,
        revision: i64,
        invitation: &str,
        loro_update: Option<&[u8]>,
    ) -> StorageResult<Board> {
        validate_iroh_snapshot(data, role, loro_update)?;
        let tx = self.connection.transaction()?;
        tx.execute("INSERT INTO cardbe_boards(name,shared_role,sync_status,sync_revision) VALUES(?1,?2,?3,?4)", params![name, role, SyncStatus::Synced, revision])?;
        let id = tx.last_insert_rowid();
        write_board_transaction(&tx, id, data)?;
        if let Some(update) = loro_update.filter(|update| !update.is_empty()) {
            tx.execute(
                "INSERT INTO cardbe_loro_docs(board_id,payload) VALUES(?1,?2)",
                params![id, update],
            )?;
        }
        tx.execute(
            "INSERT INTO cardbe_iroh_remotes(board_id,invitation) VALUES(?1,?2)",
            params![id, invitation],
        )?;
        tx.commit()?;
        Ok(Board {
            id,
            name: name.into(),
            task_count: data
                .columns
                .iter()
                .map(|column| column.tasks.len() as i64)
                .sum(),
            shared_role: role.into(),
            is_shared: true,
            sync_status: SyncStatus::Synced,
            sync_revision: revision,
        })
    }

    pub fn delete_board(&mut self, id: i64) -> StorageResult<()> {
        let tx = self.connection.transaction()?;
        if tx.execute("DELETE FROM cardbe_boards WHERE id = ?1", [id])? == 0 {
            return Err(DomainError::BoardNotFound.into());
        }
        // Plugin source mappings have no foreign key and the table is created lazily.
        if table_exists(&tx, "cardbe_plugin_sources")? {
            tx.execute("DELETE FROM cardbe_plugin_sources WHERE board_id=?1", [id])?;
        }
        tx.commit()?;
        self.loro_peers.remove(&id);
        Ok(())
    }

    pub fn archived_task_count(&self, board_id: i64) -> StorageResult<i64> {
        Ok(self.connection.query_row(
            "SELECT COUNT(*) FROM cardbe_archives WHERE board_id=?1",
            [board_id],
            |row| row.get(0),
        )?)
    }

    fn set_active_board_id(&mut self, id: i64) -> StorageResult<()> {
        self.connection.execute(
            "INSERT INTO cardbe_global_metadata(key, value) VALUES ('active_board_id', ?1)
             ON CONFLICT(key) DO UPDATE SET value = excluded.value",
            [id.to_string()],
        )?;
        self.active_board_id = id;
        Ok(())
    }

    pub fn create_note_with_content(
        &mut self,
        now: i64,
        title: String,
        content: String,
    ) -> StorageResult<Note> {
        self.connection.execute(
            "INSERT INTO notes(title, content, pinned, created_at, updated_at)
             VALUES (?1, ?2, 0, ?3, ?3)",
            params![title, content, now],
        )?;
        Ok(Note {
            id: self.connection.last_insert_rowid(),
            title,
            content,
            pinned: false,
            created_at: now,
            updated_at: now,
        })
    }

    pub fn update_note(&mut self, note: &Note) -> StorageResult<()> {
        let changed = self.connection.execute(
            "UPDATE notes SET title = ?2, content = ?3, pinned = ?4,
                 updated_at = ?5 WHERE id = ?1",
            params![
                note.id,
                note.title,
                note.content,
                note.pinned,
                note.updated_at
            ],
        )?;
        if changed == 0 {
            return Err(crate::errors::DomainError::NoteNotFound.into());
        }
        Ok(())
    }

    pub fn delete_note(&mut self, note_id: i64) -> StorageResult<()> {
        let changed = self
            .connection
            .execute("DELETE FROM notes WHERE id = ?1", [note_id])?;
        if changed == 0 {
            return Err(crate::errors::DomainError::NoteNotFound.into());
        }
        Ok(())
    }

    pub fn persist_diff(
        &mut self,
        before: &StoredData,
        after: &StoredData,
    ) -> StorageResult<PersistStats> {
        self.persist_diff_with_events(
            before,
            after,
            true,
            EventWrite::Record(EventSource::local()),
        )
    }

    #[cfg(test)]
    pub fn persist_diff_without_archives(
        &mut self,
        before: &StoredData,
        after: &StoredData,
    ) -> StorageResult<PersistStats> {
        self.persist_diff_with_events(
            before,
            after,
            false,
            EventWrite::Record(EventSource::local()),
        )
    }

    /// Persists the active board. Without loaded archives, `before` and
    /// `after` hold none, so archiving and restoring cannot occur.
    pub fn persist_diff_with_events(
        &mut self,
        before: &StoredData,
        after: &StoredData,
        include_archives: bool,
        events: EventWrite,
    ) -> StorageResult<PersistStats> {
        self.persist_diff_with_flow(before, after, include_archives, events, None)
    }

    pub fn import_board_data(
        &mut self,
        before: &StoredData,
        after: &StoredData,
        flow: Option<&crate::flow::FlowData>,
        notes: &[Note],
    ) -> StorageResult<PersistStats> {
        if flow.is_none() && !notes.is_empty() {
            return Err(DomainError::InvalidArgument.into());
        }
        self.persist_diff_with_flow(
            before,
            after,
            true,
            EventWrite::Reset(EventSource::local()),
            flow.map(|flow| (flow, notes)),
        )
    }

    fn persist_diff_with_flow(
        &mut self,
        before: &StoredData,
        after: &StoredData,
        include_archives: bool,
        events: EventWrite,
        flow: Option<(&crate::flow::FlowData, &[Note])>,
    ) -> StorageResult<PersistStats> {
        let mut positions = self.positions.clone();
        let board_changed = before.columns != after.columns
            || before.archives != after.archives
            || before.templates != after.templates
            || before.label_recency != after.label_recency
            || before.next_column_id != after.next_column_id
            || before.next_task_id != after.next_task_id
            || before.next_template_id != after.next_template_id
            || before.schema_version != after.schema_version;
        // Only a missing document needs the complete archive projection.
        let needs_archive_seed = !include_archives
            && board_changed
            && !self.connection.query_row(
                "SELECT EXISTS(SELECT 1 FROM cardbe_loro_docs WHERE board_id=?1)",
                [self.active_board_id],
                |row| row.get::<_, bool>(0),
            )?;
        let archives = if needs_archive_seed {
            Some(self.load_board_archives(self.active_board_id)?)
        } else {
            None
        };
        let transaction = self.connection.transaction()?;
        if self.active_board_id == 0 {
            return Err("No active board is selected".into());
        }
        let role: BoardRole = transaction.query_row(
            "SELECT shared_role FROM cardbe_boards WHERE id=?1",
            [self.active_board_id],
            |r| r.get(0),
        )?;
        if (board_changed || flow.is_some()) && role == BoardRole::Viewer {
            return Err(DomainError::PermissionDenied.into());
        }
        let mut stats = persist_board_diff_transaction(
            &transaction,
            self.active_board_id,
            before,
            after,
            &mut positions,
            include_archives,
        )?;
        if let Some((flow, notes)) = flow {
            transaction.execute(
                "DELETE FROM cardbe_flow_layouts WHERE board_id=?1",
                [self.active_board_id],
            )?;
            transaction.execute(
                "DELETE FROM cardbe_task_links WHERE board_id=?1",
                [self.active_board_id],
            )?;
            restore_single_board_flow(&transaction, self.active_board_id, flow, notes)?;
        }
        if board_changed {
            stats.event_op = match &events {
                EventWrite::Record(source) => {
                    task_events::record(&transaction, self.active_board_id, before, after, source)?
                }
                EventWrite::Reset(source) => {
                    task_events::record_reset(&transaction, self.active_board_id, source)?
                }
            };
        }
        // Settings are global application preferences, not board content.
        transaction.execute(
            "INSERT INTO cardbe_global_metadata(key,value) VALUES ('settings',?1)
             ON CONFLICT(key) DO UPDATE SET value=excluded.value",
            [serde_json::to_string(&after.settings)?],
        )?;
        if board_changed {
            mark_local_board_change(&transaction, self.active_board_id, role)?;
            if matches!(role, BoardRole::Owner | BoardRole::Editor) {
                if let Some(archives) = archives {
                    let mut complete_before = before.clone();
                    let mut complete_after = after.clone();
                    complete_before.archives = archives.clone();
                    complete_after.archives = archives;
                    persist_loro_local_delta(
                        &transaction,
                        self.active_board_id,
                        &complete_before,
                        &complete_after,
                        self.loro_peers.entry(self.active_board_id).or_default(),
                    )?;
                } else {
                    persist_loro_local_delta(
                        &transaction,
                        self.active_board_id,
                        before,
                        after,
                        self.loro_peers.entry(self.active_board_id).or_default(),
                    )?;
                }
            }
        }
        transaction.commit()?;
        self.positions = positions;
        Ok(stats)
    }

    fn replace_all(&mut self, data: &StoredData) -> StorageResult<()> {
        let mut positions = DatabasePositions::default();
        let transaction = self.connection.transaction()?;
        transaction.execute_batch(
            "DELETE FROM tasks;
             DELETE FROM columns;
             DELETE FROM archives;
             DELETE FROM templates;
             DELETE FROM metadata;",
        )?;
        persist_diff_transaction(
            &transaction,
            &StoredData::default(),
            data,
            &mut positions,
            true,
        )?;
        transaction.execute(
            "INSERT INTO metadata(key, value) VALUES
                ('storage_initialized', '1'),
                ('lazy_archives_ready', '1')",
            [],
        )?;
        transaction.commit()?;
        self.positions = positions;
        if self.active_board_id != 0 {
            let board_id = self.active_board_id;
            let tx = self.connection.transaction()?;
            tx.execute("DELETE FROM cardbe_columns WHERE board_id=?1", [board_id])?;
            tx.execute("DELETE FROM cardbe_archives WHERE board_id=?1", [board_id])?;
            tx.execute("DELETE FROM cardbe_templates WHERE board_id=?1", [board_id])?;
            tx.execute(
                "DELETE FROM cardbe_board_metadata WHERE board_id=?1",
                [board_id],
            )?;
            write_board_transaction(&tx, board_id, data)?;
            tx.commit()?;
            self.positions = self.load_board_positions(board_id)?;
        }
        Ok(())
    }

    fn is_initialized(&self) -> StorageResult<bool> {
        Ok(metadata_value(&self.connection, "storage_initialized")?.as_deref() == Some("1"))
    }

    fn load(&self) -> StorageResult<StoredData> {
        if self.active_board_id != 0 {
            return self.load_board_data(self.active_board_id, true);
        }
        self.load_internal(true)
    }

    fn load_internal(&self, include_archives: bool) -> StorageResult<StoredData> {
        let mut stored = StoredData::default();

        let mut columns = self.connection.prepare(
            // Legacy single-board columns predate the done flag.
            "SELECT id, name, color, sort_order, sort_rules, 0 FROM columns ORDER BY position, id",
        )?;
        stored.columns = columns
            .query_map([], column_from_row)?
            .collect::<Result<Vec<_>, _>>()?;

        let column_indices = stored
            .columns
            .iter()
            .enumerate()
            .map(|(index, column)| (column.id, index))
            .collect::<HashMap<_, _>>();
        let mut tasks = self
            .connection
            .prepare("SELECT column_id, payload FROM tasks ORDER BY column_id, position, id")?;
        let task_rows = tasks
            .query_map([], |row| {
                Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?))
            })?
            .collect::<Result<Vec<_>, _>>()?;
        for (column_id, payload) in task_rows {
            let task: Task = serde_json::from_str(&payload)?;
            let index = column_indices
                .get(&column_id)
                .ok_or_else(|| format!("Task {} refers to missing column {column_id}", task.id))?;
            stored.columns[*index].tasks.push(task);
        }

        if include_archives {
            stored.archives = self.load_archives()?;
        }

        let mut templates = self
            .connection
            .prepare("SELECT id, name, payload FROM templates ORDER BY position, id")?;
        let template_rows = templates
            .query_map([], |row| {
                Ok((
                    row.get::<_, i64>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                ))
            })?
            .collect::<Result<Vec<_>, _>>()?;
        stored.templates = template_rows
            .into_iter()
            .map(|(id, name, payload)| {
                Ok(TaskTemplate {
                    id,
                    name,
                    task: serde_json::from_str(&payload)?,
                })
            })
            .collect::<StorageResult<Vec<_>>>()?;

        stored.schema_version =
            metadata_parse(&self.connection, "schema_version")?.unwrap_or(stored.schema_version);
        stored.next_column_id =
            metadata_parse(&self.connection, "next_column_id")?.unwrap_or_default();
        stored.next_task_id = metadata_parse(&self.connection, "next_task_id")?.unwrap_or_default();
        stored.next_template_id =
            metadata_parse(&self.connection, "next_template_id")?.unwrap_or_default();
        stored.settings = metadata_json(&self.connection, "settings")?.unwrap_or_default();
        stored.label_recency =
            metadata_json(&self.connection, "label_recency")?.unwrap_or_default();
        Ok(stored)
    }

    pub fn load_archives(&self) -> StorageResult<Vec<Archive>> {
        if self.active_board_id != 0 {
            return self.load_board_archives(self.active_board_id);
        }
        let mut archives = self
            .connection
            .prepare("SELECT archived_at, payload FROM archives ORDER BY position, task_id")?;
        let archive_rows = archives
            .query_map([], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
            })?
            .collect::<Result<Vec<_>, _>>()?;
        archive_rows
            .into_iter()
            .map(|(time, payload)| {
                Ok(Archive {
                    time: time.parse()?,
                    task: serde_json::from_str(&payload)?,
                })
            })
            .collect()
    }

    fn lazy_archives_ready(&self) -> StorageResult<bool> {
        Ok(metadata_value(&self.connection, "lazy_archives_ready")?.as_deref() == Some("1"))
    }

    fn mark_lazy_archives_ready(&self) -> StorageResult<()> {
        self.connection.execute(
            "INSERT INTO metadata(key, value) VALUES ('lazy_archives_ready', '1')
             ON CONFLICT(key) DO UPDATE SET value = excluded.value",
            [],
        )?;
        Ok(())
    }

    fn migrate_multiboard_schema(&mut self, legacy: &StoredData) -> StorageResult<()> {
        let tx = self.connection.transaction()?;
        tx.execute_batch(
            "CREATE TABLE IF NOT EXISTS cardbe_global_metadata(key TEXT PRIMARY KEY,value TEXT NOT NULL);
             CREATE TABLE IF NOT EXISTS cardbe_boards(id INTEGER PRIMARY KEY AUTOINCREMENT,name TEXT NOT NULL,shared_role TEXT NOT NULL DEFAULT 'owner',sync_status TEXT NOT NULL DEFAULT 'local',sync_revision INTEGER NOT NULL DEFAULT 0);
             CREATE TABLE IF NOT EXISTS cardbe_columns(board_id INTEGER NOT NULL REFERENCES cardbe_boards(id) ON DELETE CASCADE,id INTEGER NOT NULL,name TEXT NOT NULL,color TEXT NOT NULL,sort_order TEXT NOT NULL,position INTEGER NOT NULL,sort_rules TEXT NOT NULL DEFAULT '[]',done INTEGER NOT NULL DEFAULT 0,PRIMARY KEY(board_id,id));
             CREATE TABLE IF NOT EXISTS cardbe_tasks(board_id INTEGER NOT NULL,id INTEGER NOT NULL,column_id INTEGER NOT NULL,position INTEGER NOT NULL,payload TEXT NOT NULL,PRIMARY KEY(board_id,id),FOREIGN KEY(board_id,column_id) REFERENCES cardbe_columns(board_id,id) ON DELETE CASCADE);
             CREATE INDEX IF NOT EXISTS cardbe_tasks_by_board_column_position ON cardbe_tasks(board_id,column_id,position);
             CREATE TABLE IF NOT EXISTS cardbe_archives(board_id INTEGER NOT NULL REFERENCES cardbe_boards(id) ON DELETE CASCADE,task_id INTEGER NOT NULL,archived_at TEXT NOT NULL,position INTEGER NOT NULL,payload TEXT NOT NULL,PRIMARY KEY(board_id,task_id));
             CREATE INDEX IF NOT EXISTS cardbe_archives_by_board_time ON cardbe_archives(board_id,CAST(archived_at AS INTEGER) DESC,task_id DESC);
             CREATE TABLE IF NOT EXISTS cardbe_templates(board_id INTEGER NOT NULL REFERENCES cardbe_boards(id) ON DELETE CASCADE,id INTEGER NOT NULL,name TEXT NOT NULL,position INTEGER NOT NULL,payload TEXT NOT NULL,PRIMARY KEY(board_id,id));
             CREATE TABLE IF NOT EXISTS cardbe_board_metadata(board_id INTEGER NOT NULL REFERENCES cardbe_boards(id) ON DELETE CASCADE,key TEXT NOT NULL,value TEXT NOT NULL,PRIMARY KEY(board_id,key));"
        )?;
        tx.execute_batch("CREATE TABLE IF NOT EXISTS cardbe_iroh_invites(invite_id TEXT PRIMARY KEY,board_id INTEGER NOT NULL REFERENCES cardbe_boards(id) ON DELETE CASCADE,secret TEXT NOT NULL,permission TEXT NOT NULL,enabled INTEGER NOT NULL DEFAULT 1,created_at TEXT NOT NULL DEFAULT (datetime('now')));
            CREATE TABLE IF NOT EXISTS cardbe_iroh_devices(invite_id TEXT NOT NULL REFERENCES cardbe_iroh_invites(invite_id) ON DELETE CASCADE,node_id TEXT NOT NULL,status INTEGER NOT NULL DEFAULT 0 CHECK(status IN (0,1,2)),PRIMARY KEY(invite_id,node_id));
            CREATE TABLE IF NOT EXISTS cardbe_iroh_remotes(board_id INTEGER PRIMARY KEY REFERENCES cardbe_boards(id) ON DELETE CASCADE,invitation TEXT NOT NULL);
            CREATE TABLE IF NOT EXISTS cardbe_iroh_conflicts(board_id INTEGER PRIMARY KEY REFERENCES cardbe_boards(id) ON DELETE CASCADE,revision INTEGER NOT NULL,name TEXT NOT NULL,permission TEXT NOT NULL,payload TEXT NOT NULL);
            CREATE TABLE IF NOT EXISTS cardbe_loro_docs(board_id INTEGER PRIMARY KEY REFERENCES cardbe_boards(id) ON DELETE CASCADE,payload BLOB NOT NULL);")?;
        // Persist only an activation marker, never deleted invitation secrets.
        // The trigger also covers board cascades and backup restoration.
        tx.execute_batch("CREATE TRIGGER IF NOT EXISTS cardbe_iroh_record_revocation AFTER DELETE ON cardbe_iroh_invites BEGIN
            INSERT INTO cardbe_global_metadata(key,value) VALUES('iroh_has_revocations','1') ON CONFLICT(key) DO NOTHING;
            END;")?;
        for sql in [
            "ALTER TABLE cardbe_boards ADD COLUMN shared_role TEXT NOT NULL DEFAULT 'owner'",
            "ALTER TABLE cardbe_boards ADD COLUMN sync_status TEXT NOT NULL DEFAULT 'local'",
            "ALTER TABLE cardbe_boards ADD COLUMN sync_revision INTEGER NOT NULL DEFAULT 0",
            "ALTER TABLE cardbe_iroh_invites ADD COLUMN enabled INTEGER NOT NULL DEFAULT 1",
            "ALTER TABLE cardbe_iroh_invites ADD COLUMN created_at TEXT NOT NULL DEFAULT ''",
            "ALTER TABLE cardbe_iroh_conflicts ADD COLUMN loro_update BLOB",
            "ALTER TABLE cardbe_columns ADD COLUMN sort_rules TEXT NOT NULL DEFAULT '[]'",
            "ALTER TABLE cardbe_columns ADD COLUMN done INTEGER NOT NULL DEFAULT 0",
        ] {
            let _ = tx.execute(sql, []);
        }
        crate::flow::initialize(&tx)?;
        task_events::initialize(&tx)?;
        validate_cardbe_schema(&tx)?;
        let migrated: Option<String> = tx
            .query_row(
                "SELECT value FROM cardbe_global_metadata WHERE key='multiboard_schema_version'",
                [],
                |r| r.get(0),
            )
            .optional()?;
        if migrated.as_deref() != Some("1") {
            let experimental = experimental_snapshots(&tx)?;
            if experimental.is_empty() {
                tx.execute("INSERT INTO cardbe_boards(name) VALUES ('My board')", [])?;
                let id = tx.last_insert_rowid();
                write_board_transaction(&tx, id, legacy)?;
                tx.execute(
                    "INSERT INTO cardbe_global_metadata(key,value) VALUES ('active_board_id',?1)",
                    [id.to_string()],
                )?;
            } else {
                let mut first = None;
                for (old_id, name, data) in experimental {
                    tx.execute(
                        "INSERT INTO cardbe_boards(id,name) VALUES (?1,?2)",
                        params![old_id, name],
                    )?;
                    write_board_transaction(&tx, old_id, &data)?;
                    first.get_or_insert(old_id);
                }
                let preferred = legacy_metadata_parse::<i64>(&tx, "active_board_id")?
                    .filter(|id| {
                        tx.query_row(
                            "SELECT EXISTS(SELECT 1 FROM cardbe_boards WHERE id=?1)",
                            [id],
                            |r| r.get::<_, bool>(0),
                        )
                        .unwrap_or(false)
                    })
                    .or(first)
                    .ok_or("Experimental board migration contained no boards")?;
                tx.execute(
                    "INSERT INTO cardbe_global_metadata(key,value) VALUES ('active_board_id',?1)",
                    [preferred.to_string()],
                )?;
            }
            tx.execute("INSERT INTO cardbe_global_metadata(key,value) VALUES ('settings',?1) ON CONFLICT(key) DO NOTHING", [serde_json::to_string(&legacy.settings)?])?;
            tx.execute("INSERT INTO cardbe_global_metadata(key,value) VALUES ('multiboard_schema_version','1')", [])?;
        }
        if let Some(value) = namespaced_metadata_json::<serde_json::Value>(&tx, "settings")? {
            if value.pointer("/iroh_network/enabled").is_none() {
                let mut settings: crate::models::Settings = serde_json::from_value(value)?;
                settings.iroh_network.enabled = tx.query_row(
                    "SELECT EXISTS(SELECT 1 FROM cardbe_iroh_invites)
                     OR EXISTS(SELECT 1 FROM cardbe_iroh_remotes)
                     OR EXISTS(SELECT 1 FROM cardbe_global_metadata WHERE key='iroh_has_revocations')",
                    [],
                    |row| row.get(0),
                )?;
                tx.execute(
                    "UPDATE cardbe_global_metadata SET value=?1 WHERE key='settings'",
                    [serde_json::to_string(&settings)?],
                )?;
            }
        }
        tx.commit()?;
        self.initialize_task_search_index()?;
        Ok(())
    }

    fn write_board(&mut self, board_id: i64, data: &StoredData) -> StorageResult<()> {
        let tx = self.connection.transaction()?;
        write_board_transaction(&tx, board_id, data)?;
        tx.commit()?;
        Ok(())
    }

    pub(crate) fn load_board_positions(&self, board_id: i64) -> StorageResult<DatabasePositions> {
        Ok(DatabasePositions {
            columns: load_board_positions(&self.connection, "cardbe_columns", board_id)?,
            tasks: load_board_positions(&self.connection, "cardbe_tasks", board_id)?,
        })
    }

    fn load_board_data(&self, board_id: i64, include_archives: bool) -> StorageResult<StoredData> {
        let mut stored = StoredData::default();
        let mut stmt=self.connection.prepare("SELECT id,name,color,sort_order,sort_rules,done FROM cardbe_columns WHERE board_id=?1 ORDER BY position,id")?;
        stored.columns = stmt
            .query_map([board_id], column_from_row)?
            .collect::<Result<Vec<_>, _>>()?;
        let indexes = stored
            .columns
            .iter()
            .enumerate()
            .map(|(i, c)| (c.id, i))
            .collect::<HashMap<_, _>>();
        let mut stmt=self.connection.prepare("SELECT column_id,payload FROM cardbe_tasks WHERE board_id=?1 ORDER BY column_id,position,id")?;
        for row in stmt.query_map([board_id], |r| {
            Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?))
        })? {
            let (column_id, payload) = row?;
            let task: Task = serde_json::from_str(&payload)?;
            let index = indexes.get(&column_id).ok_or_else(|| {
                format!(
                    "Board {board_id} task {} refers to missing column {column_id}",
                    task.id
                )
            })?;
            stored.columns[*index].tasks.push(task);
        }
        if include_archives {
            stored.archives = self.load_board_archives(board_id)?;
        }
        let mut stmt = self.connection.prepare(
            "SELECT id,name,payload FROM cardbe_templates WHERE board_id=?1 ORDER BY position,id",
        )?;
        stored.templates = stmt
            .query_map([board_id], |r| {
                Ok((
                    r.get::<_, i64>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, String>(2)?,
                ))
            })?
            .map(|row| {
                let (id, name, payload) = row?;
                Ok(TaskTemplate {
                    id,
                    name,
                    task: serde_json::from_str(&payload).map_err(|e| {
                        rusqlite::Error::FromSqlConversionFailure(
                            2,
                            rusqlite::types::Type::Text,
                            Box::new(e),
                        )
                    })?,
                })
            })
            .collect::<Result<Vec<_>, rusqlite::Error>>()?;
        stored.schema_version = board_metadata_parse(&self.connection, board_id, "schema_version")?
            .unwrap_or(CURRENT_SCHEMA_VERSION);
        stored.next_column_id =
            board_metadata_parse(&self.connection, board_id, "next_column_id")?.unwrap_or_default();
        stored.next_task_id =
            board_metadata_parse(&self.connection, board_id, "next_task_id")?.unwrap_or_default();
        stored.next_template_id =
            board_metadata_parse(&self.connection, board_id, "next_template_id")?
                .unwrap_or_default();
        stored.label_recency =
            board_metadata_json(&self.connection, board_id, "label_recency")?.unwrap_or_default();
        stored.settings =
            namespaced_metadata_json(&self.connection, "settings")?.unwrap_or_default();
        Ok(stored)
    }

    fn load_board_archives(&self, board_id: i64) -> StorageResult<Vec<Archive>> {
        let mut stmt=self.connection.prepare("SELECT archived_at,payload FROM cardbe_archives WHERE board_id=?1 ORDER BY position,task_id")?;
        let result = stmt
            .query_map([board_id], |r| {
                Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))
            })?
            .map(|row| {
                let (time, payload) = row?;
                Ok(Archive {
                    time: time.parse()?,
                    task: serde_json::from_str(&payload)?,
                })
            })
            .collect();
        result
    }

    pub fn load_archive_task(&self, board_id: i64, task_id: i64) -> StorageResult<Option<Task>> {
        self.connection
            .query_row(
                "SELECT payload FROM cardbe_archives WHERE board_id=?1 AND task_id=?2",
                params![board_id, task_id],
                |row| row.get::<_, String>(0),
            )
            .optional()?
            .map(|payload| serde_json::from_str(&payload).map_err(Into::into))
            .transpose()
    }

    /// Loads archived tasks in request order, skipping duplicates and missing IDs.
    pub fn load_archive_tasks(&self, board_id: i64, task_ids: &[i64]) -> StorageResult<Vec<Task>> {
        let mut statement = self
            .connection
            .prepare("SELECT payload FROM cardbe_archives WHERE board_id=?1 AND task_id=?2")?;
        let mut seen = std::collections::HashSet::new();
        let mut tasks = Vec::new();
        for task_id in task_ids {
            if !seen.insert(*task_id) {
                continue;
            }
            if let Some(payload) = statement
                .query_row(params![board_id, task_id], |row| row.get::<_, String>(0))
                .optional()?
            {
                tasks.push(serde_json::from_str(&payload)?);
            }
        }
        Ok(tasks)
    }

    pub fn list_all_task_page(
        &self,
        filter: &TaskExplorerQuery,
        offset: i64,
        limit: usize,
    ) -> StorageResult<Vec<AllTaskRow>> {
        if filter
            .search_options
            .is_some_and(|options| !options.valid())
        {
            return Err(DomainError::InvalidArgument.into());
        }
        // shortcut: OFFSET slows deep pages; use keyset cursors if profiling justifies it.
        let matches = if filter.query.trim().is_empty() {
            None
        } else {
            Some(serde_json::to_string(&hybrid_search(
                &self.connection,
                filter,
                filter.search_options.unwrap_or_default(),
            )?)?)
        };
        let sql = format!(
            "{TASK_SEARCH_CTE}
             , hits AS MATERIALIZED (
               SELECT CAST(value AS INTEGER) AS search_id, CAST(key AS INTEGER) AS rank
               FROM json_each(?1))
             SELECT e.board_id, e.board_name, e.column_name, e.archived_at, e.payload,
                    e.column_id, e.completion='done', e.waiting_count
             FROM eligible e
             LEFT JOIN hits h ON h.search_id=e.search_id
             WHERE (?1 IS NULL OR h.search_id IS NOT NULL)
             ORDER BY
               CASE WHEN ?14='relevance' THEN h.rank END,
               CASE WHEN ?14='priority' THEN CASE priority
                 WHEN 'urgent' THEN 4 WHEN 'high' THEN 3 WHEN 'medium' THEN 2
                 WHEN 'low' THEN 1 ELSE 0 END END DESC,
               CASE WHEN ?14 IN ('due','priority') THEN due_time IS NULL END,
               CASE WHEN ?14 IN ('due','priority') THEN due_time END,
               CASE WHEN ?14='archived' THEN CAST(archived_at AS INTEGER) END DESC,
               CASE WHEN ?14='column' THEN column_name END COLLATE NOCASE,
               CASE WHEN ?14='created' THEN
                 CAST(json_extract(payload, '$.created_time') AS INTEGER) END DESC,
               title COLLATE NOCASE, board_id, archived, task_id
             LIMIT ?15 OFFSET ?16",
        );
        let mut statement = self.connection.prepare(&sql)?;
        let rows = statement
            .query_map(
                params![
                    matches,
                    filter
                        .board_ids
                        .as_ref()
                        .map(serde_json::to_string)
                        .transpose()?,
                    filter.column_id,
                    filter.status,
                    filter.now,
                    filter.due,
                    filter.due_start,
                    filter.due_end,
                    filter.archive_start,
                    filter.archive_end,
                    crate::task_dates::local_day_start(filter.now),
                    filter
                        .priorities
                        .as_ref()
                        .map(serde_json::to_string)
                        .transpose()?,
                    filter.extra_filters()?,
                    filter.sort,
                    i64::try_from(limit)?,
                    offset,
                ],
                |row| {
                    let board_id = row.get(0)?;
                    let board_name = row.get(1)?;
                    let column_name = row.get(2)?;
                    let archived_at = row
                        .get::<_, Option<String>>(3)?
                        .map(|value| value.parse::<u128>())
                        .transpose()
                        .map_err(|error| {
                            rusqlite::Error::FromSqlConversionFailure(
                                3,
                                rusqlite::types::Type::Text,
                                Box::new(error),
                            )
                        })?;
                    let payload = row.get::<_, String>(4)?;
                    let task: Task = serde_json::from_str(&payload).map_err(|error| {
                        rusqlite::Error::FromSqlConversionFailure(
                            4,
                            rusqlite::types::Type::Text,
                            Box::new(error),
                        )
                    })?;
                    Ok(AllTaskRow {
                        board_id,
                        board_name,
                        column_id: row.get(5)?,
                        column_name,
                        archived_at,
                        done: row.get(6)?,
                        waiting_count: row.get(7)?,
                        excerpt: crate::search::task_excerpt(&task, &filter.query),
                        task: TaskSummary::from(&task),
                    })
                },
            )?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    /// Counts `filter`'s tasks under every status at once, ignoring its own
    /// status and sort. The column filter does not apply to archived tasks,
    /// as the explorer drops it there. Searches count one pool fused across
    /// all statuses, so each count is capped by the same search limits.
    pub fn count_all_tasks(&self, filter: &TaskExplorerQuery) -> StorageResult<TaskStatusCounts> {
        let every_status = TaskExplorerQuery {
            status: "all".into(),
            column_id: None,
            ..filter.clone()
        };
        let options = filter.search_options.unwrap_or_default();
        if !options.valid() {
            return Err(DomainError::InvalidArgument.into());
        }
        let mut values = search_params(&every_status)?;
        if !filter.query.trim().is_empty() {
            let matches = hybrid_search(&self.connection, &every_status, options)?;
            values[0] = rusqlite::types::Value::Text(serde_json::to_string(&matches)?);
        }
        values.push(
            filter
                .column_id
                .map(rusqlite::types::Value::Integer)
                .unwrap_or(rusqlite::types::Value::Null),
        );
        let sql = format!(
            "{TASK_SEARCH_CTE}
             , counted AS (SELECT *, archived=0 AND (?14 IS NULL OR column_id=?14) AS open_match
               FROM eligible
               WHERE (?1 IS NULL OR search_id IN (SELECT value FROM json_each(?1))))
             SELECT COALESCE(SUM(?14 IS NULL OR column_id=?14), 0),
                    COALESCE(SUM(open_match), 0),
                    COALESCE(SUM(open_match AND due_time<?5
                                 AND (due_time<>?11 OR due_at_midnight)), 0),
                    COALESCE(SUM(open_match AND recurrence IS NOT NULL), 0),
                    COALESCE(SUM(archived=1), 0)
             FROM counted",
        );
        Ok(self
            .connection
            .query_row(&sql, rusqlite::params_from_iter(&values), |row| {
                Ok(TaskStatusCounts {
                    all: row.get(0)?,
                    active: row.get(1)?,
                    overdue: row.get(2)?,
                    recurring: row.get(3)?,
                    archived: row.get(4)?,
                })
            })?)
    }

    pub fn task_events(
        &self,
        board_id: i64,
        task_id: Option<i64>,
    ) -> StorageResult<Vec<task_events::TaskEvent>> {
        task_events::list(&self.connection, board_id, task_id, None)
    }

    /// Board events recorded after `after_id`, for refreshing a loaded log.
    pub fn task_events_after(
        &self,
        board_id: i64,
        after_id: Option<i64>,
    ) -> StorageResult<Vec<task_events::TaskEvent>> {
        task_events::list(&self.connection, board_id, None, after_id)
    }

    pub fn load_task(&self, board_id: i64, task_id: i64) -> StorageResult<Option<Task>> {
        self.connection
            .query_row(
                "SELECT payload FROM cardbe_tasks WHERE board_id=?1 AND id=?2",
                params![board_id, task_id],
                |row| row.get::<_, String>(0),
            )
            .optional()?
            .map(|payload| serde_json::from_str(&payload).map_err(Into::into))
            .transpose()
    }
}

pub fn load(app_data_dir: &Path) -> StorageResult<LoadedData> {
    fs::create_dir_all(app_data_dir)?;
    let database_path = app_data_dir.join("data.sqlite3");
    let database_existed = database_path.try_exists()?;
    let mut messages = Vec::new();
    log::debug!(
        target: "storage.database",
        "Loading database path={} file_exists={} current_schema_version={}",
        database_path.display(),
        database_existed,
        CURRENT_SCHEMA_VERSION,
    );
    let mut database = Database::open(database_path)?;
    let storage_initialized = database.is_initialized()?;
    let legacy_schema_version = metadata_parse::<u32>(&database.connection, "schema_version")?;
    log::debug!(
        target: "storage.database",
        "Database state storage_initialized={} legacy_schema_version={:?}",
        storage_initialized,
        legacy_schema_version,
    );

    let (legacy, _archives_loaded) = if database_existed && storage_initialized {
        let archives_loaded = !database.lazy_archives_ready()?;
        // Snapshots must include archives even when the legacy store is still
        // using lazy archive hydration, otherwise the first board migration
        // would omit them.
        let stored = database.load()?;
        let (migrated, _) = stored.migrate().map_err(std::io::Error::other)?;
        if archives_loaded {
            database.mark_lazy_archives_ready()?;
        }
        (migrated, archives_loaded)
    } else {
        let (stored, imported_current_json) = load_json_or_legacy(app_data_dir, &mut messages)?;
        let (stored, _) = stored.migrate().map_err(std::io::Error::other)?;
        database.replace_all(&stored).map_err(|error| {
            std::io::Error::other(format!(
                "Could not migrate application data to SQLite; the source files were left unchanged: {error}"
            ))
        })?;
        if imported_current_json {
            preserve_migrated_json(app_data_dir, &mut messages);
        }
        (stored, true)
    };

    // Board snapshots were introduced after the original single-board SQLite
    // layout.  The first run copies that complete document into the default
    // board before any future writes, preserving every old field verbatim.
    let (boards, stored) = database.initialize_boards(&legacy)?;
    let before = stored.clone();
    let (mut stored, changed) = stored.migrate().map_err(std::io::Error::other)?;
    if changed {
        if database.board_role(database.active_board_id)? == BoardRole::Viewer {
            // Startup normalization is a local schema repair, not a user edit.
            // Keep the owner's permission, revision, and synchronization state.
            database.write_board(database.active_board_id, &stored)?;
            database.positions = database.load_board_positions(database.active_board_id)?;
        } else {
            database.persist_diff(&before, &stored)?;
        }
    }
    if !_archives_loaded {
        stored.archives.clear();
    }
    log::debug!(
        target: "storage.database",
        "Database ready board_count={} active_board_id={} schema_version={} schema_migrated={} archives_loaded={} recovery_messages={}",
        boards.len(),
        database.active_board_id(),
        stored.schema_version,
        changed,
        _archives_loaded,
        messages.len(),
    );
    Ok(LoadedData {
        stored,
        database,
        recovery_messages: messages,
        archives_loaded: _archives_loaded,
    })
}

fn persist_diff_transaction(
    transaction: &Transaction<'_>,
    before: &StoredData,
    after: &StoredData,
    positions: &mut DatabasePositions,
    include_archives: bool,
) -> StorageResult<PersistStats> {
    let mut stats = PersistStats::default();
    let before_tasks = task_locations(before);
    let after_tasks = task_locations(after);

    let column_ids = after
        .columns
        .iter()
        .map(|column| column.id)
        .collect::<Vec<_>>();
    let column_positions = assign_sparse_positions(&column_ids, &positions.columns)?;

    let mut task_positions = HashMap::new();
    for column in &after.columns {
        let ids = column.tasks.iter().map(|task| task.id).collect::<Vec<_>>();
        let old_positions = before
            .columns
            .iter()
            .find(|old_column| old_column.id == column.id)
            .into_iter()
            .flat_map(|old_column| old_column.tasks.iter())
            .filter_map(|task| {
                positions
                    .tasks
                    .get(&task.id)
                    .copied()
                    .map(|position| (task.id, position))
            })
            .collect::<HashMap<_, _>>();
        task_positions.extend(assign_sparse_positions(&ids, &old_positions)?);
    }

    for id in before_tasks
        .keys()
        .filter(|id| !after_tasks.contains_key(id))
    {
        transaction.execute("DELETE FROM tasks WHERE id = ?1", [id])?;
        stats.tasks_changed += 1;
    }

    let before_columns = before
        .columns
        .iter()
        .map(|column| (column.id, column))
        .collect::<HashMap<_, _>>();
    let after_columns = after
        .columns
        .iter()
        .map(|column| (column.id, column))
        .collect::<HashMap<_, _>>();
    for id in before_columns
        .keys()
        .filter(|id| !after_columns.contains_key(id))
    {
        transaction.execute("DELETE FROM columns WHERE id = ?1", [id])?;
        stats.columns_changed += 1;
    }
    for (id, column) in &after_columns {
        let position = column_positions[id];
        let changed = before_columns.get(id).is_none_or(|old| {
            positions.columns.get(id) != Some(&position)
                || old.name != column.name
                || old.color != column.color
                || old.sort_order != column.sort_order
                || old.sort_rules != column.sort_rules
        });
        if changed {
            transaction.execute(
                "INSERT INTO columns(id, name, color, sort_order, sort_rules, position) VALUES (?1, ?2, ?3, ?4, ?5, ?6)
                 ON CONFLICT(id) DO UPDATE SET
                   name = excluded.name, color = excluded.color, sort_order = excluded.sort_order,
                   sort_rules = excluded.sort_rules, position = excluded.position",
                params![
                    id,
                    column.name,
                    column.color,
                    column.sort_order,
                    serde_json::to_string(&column.sort_rules)?,
                    position
                ],
            )?;
            stats.columns_changed += 1;
        }
    }

    for (id, (column_id, task)) in &after_tasks {
        let position = task_positions[id];
        let changed = before_tasks.get(id).is_none_or(|(old_column, old)| {
            old_column != column_id || positions.tasks.get(id) != Some(&position) || old != task
        });
        if changed {
            transaction.execute(
                "INSERT INTO tasks(id, column_id, position, payload) VALUES (?1, ?2, ?3, ?4)
                 ON CONFLICT(id) DO UPDATE SET
                   column_id = excluded.column_id,
                   position = excluded.position,
                   payload = excluded.payload",
                params![id, column_id, position, serde_json::to_string(task)?],
            )?;
            stats.tasks_changed += 1;
        }
    }

    if include_archives {
        stats.archives_changed = sync_archives(transaction, &before.archives, &after.archives)?;
    }
    stats.templates_changed = sync_templates(transaction, &before.templates, &after.templates)?;

    let metadata = [
        (
            "schema_version",
            before.schema_version.to_string(),
            after.schema_version.to_string(),
        ),
        (
            "next_column_id",
            before.next_column_id.to_string(),
            after.next_column_id.to_string(),
        ),
        (
            "next_task_id",
            before.next_task_id.to_string(),
            after.next_task_id.to_string(),
        ),
        (
            "next_template_id",
            before.next_template_id.to_string(),
            after.next_template_id.to_string(),
        ),
        (
            "settings",
            serde_json::to_string(&before.settings)?,
            serde_json::to_string(&after.settings)?,
        ),
        (
            "label_recency",
            serde_json::to_string(&before.label_recency)?,
            serde_json::to_string(&after.label_recency)?,
        ),
    ];
    for (key, old, new) in metadata {
        if old != new || !metadata_exists(transaction, key)? {
            transaction.execute(
                "INSERT INTO metadata(key, value) VALUES (?1, ?2)
                 ON CONFLICT(key) DO UPDATE SET value = excluded.value",
                params![key, new],
            )?;
            stats.metadata_changed += 1;
        }
    }
    positions.columns = column_positions;
    positions.tasks = task_positions;
    Ok(stats)
}

pub(crate) fn persist_board_diff_transaction(
    tx: &Transaction<'_>,
    board_id: i64,
    before: &StoredData,
    after: &StoredData,
    positions: &mut DatabasePositions,
    include_archives: bool,
) -> StorageResult<PersistStats> {
    let mut stats = PersistStats::default();
    let before_tasks = task_locations(before);
    let after_tasks = task_locations(after);
    let column_ids = after.columns.iter().map(|c| c.id).collect::<Vec<_>>();
    let column_positions = assign_sparse_positions(&column_ids, &positions.columns)?;
    let mut task_positions = HashMap::new();
    for column in &after.columns {
        let ids = column.tasks.iter().map(|t| t.id).collect::<Vec<_>>();
        let old = column
            .tasks
            .iter()
            .filter_map(|t| positions.tasks.get(&t.id).map(|p| (t.id, *p)))
            .collect();
        task_positions.extend(assign_sparse_positions(&ids, &old)?);
    }
    for id in before_tasks
        .keys()
        .filter(|id| !after_tasks.contains_key(id))
    {
        tx.execute(
            "DELETE FROM cardbe_tasks WHERE board_id=?1 AND id=?2",
            params![board_id, id],
        )?;
        stats.tasks_changed += 1;
    }
    let bc = before
        .columns
        .iter()
        .map(|c| (c.id, c))
        .collect::<HashMap<_, _>>();
    let ac = after
        .columns
        .iter()
        .map(|c| (c.id, c))
        .collect::<HashMap<_, _>>();
    for id in bc.keys().filter(|id| !ac.contains_key(id)) {
        tx.execute(
            "DELETE FROM cardbe_columns WHERE board_id=?1 AND id=?2",
            params![board_id, id],
        )?;
        stats.columns_changed += 1;
    }
    for (id, c) in &ac {
        let pos = column_positions[id];
        if bc.get(id).is_none_or(|o| {
            positions.columns.get(id) != Some(&pos)
                || o.name != c.name
                || o.color != c.color
                || o.sort_order != c.sort_order
                || o.sort_rules != c.sort_rules
                || o.done != c.done
        }) {
            tx.execute("INSERT INTO cardbe_columns(board_id,id,name,color,sort_order,sort_rules,position,done) VALUES(?1,?2,?3,?4,?5,?6,?7,?8) ON CONFLICT(board_id,id) DO UPDATE SET name=excluded.name,color=excluded.color,sort_order=excluded.sort_order,sort_rules=excluded.sort_rules,position=excluded.position,done=excluded.done",params![board_id,id,c.name,c.color,c.sort_order,serde_json::to_string(&c.sort_rules)?,pos,c.done])?;
            stats.columns_changed += 1;
        }
    }
    for (id, (column_id, task)) in &after_tasks {
        let pos = task_positions[id];
        if before_tasks.get(id).is_none_or(|(oc, ot)| {
            oc != column_id || positions.tasks.get(id) != Some(&pos) || ot != task
        }) {
            tx.execute("INSERT INTO cardbe_tasks(board_id,id,column_id,position,payload) VALUES(?1,?2,?3,?4,?5) ON CONFLICT(board_id,id) DO UPDATE SET column_id=excluded.column_id,position=excluded.position,payload=excluded.payload",params![board_id,id,column_id,pos,serde_json::to_string(task)?])?;
            stats.tasks_changed += 1;
        }
    }
    if include_archives && before.archives != after.archives {
        tx.execute("DELETE FROM cardbe_archives WHERE board_id=?1", [board_id])?;
        for (i, a) in after.archives.iter().enumerate() {
            tx.execute("INSERT INTO cardbe_archives(board_id,task_id,archived_at,position,payload) VALUES(?1,?2,?3,?4,?5)",params![board_id,a.task.id,a.time.to_string(),i as i64,serde_json::to_string(&a.task)?])?;
        }
        stats.archives_changed = after.archives.len().max(before.archives.len());
    }
    if before.templates != after.templates {
        tx.execute("DELETE FROM cardbe_templates WHERE board_id=?1", [board_id])?;
        for (i, t) in after.templates.iter().enumerate() {
            tx.execute("INSERT INTO cardbe_templates(board_id,id,name,position,payload) VALUES(?1,?2,?3,?4,?5)",params![board_id,t.id,t.name,i as i64,serde_json::to_string(&t.task)?])?;
        }
        stats.templates_changed = after.templates.len().max(before.templates.len());
    }
    let metadata = [
        ("schema_version", after.schema_version.to_string()),
        ("next_column_id", after.next_column_id.to_string()),
        ("next_task_id", after.next_task_id.to_string()),
        ("next_template_id", after.next_template_id.to_string()),
        (
            "label_recency",
            serde_json::to_string(&after.label_recency)?,
        ),
    ];
    for (key, value) in metadata {
        let current: Option<String> = tx
            .query_row(
                "SELECT value FROM cardbe_board_metadata WHERE board_id=?1 AND key=?2",
                params![board_id, key],
                |r| r.get(0),
            )
            .optional()?;
        if current.as_deref() != Some(value.as_str()) {
            tx.execute("INSERT INTO cardbe_board_metadata(board_id,key,value) VALUES(?1,?2,?3) ON CONFLICT(board_id,key) DO UPDATE SET value=excluded.value",params![board_id,key,value])?;
            stats.metadata_changed += 1;
        }
    }
    crate::flow::prune(tx, board_id)?;
    positions.columns = column_positions;
    positions.tasks = task_positions;
    Ok(stats)
}

/// What `Database::read_iroh_share` reads beyond authorization and the board.
pub struct IrohShareRead {
    pub loro_document: bool,
    pub data: bool,
}

pub struct IrohShareContents {
    pub board: Board,
    pub permission: IrohPermission,
    pub loro_document: Option<Vec<u8>>,
    pub data: Option<StoredData>,
}

#[derive(Debug, PartialEq, Eq)]
pub enum IrohShareDenied {
    DeviceNotApproved,
    InvitationUnavailable,
    BoardDeleted,
}

/// An editor push merged against a specific Loro document (`base`).
/// `merged` is `None` when the push adds no operations.
pub struct PreparedLoroUpdate {
    board_id: i64,
    base: Vec<u8>,
    merged: Option<(Vec<u8>, StoredData)>,
}

fn ensure_iroh_editor(
    connection: &Connection,
    board_id: i64,
    invite_id: &str,
    secret: &str,
    node_id: &str,
) -> StorageResult<()> {
    let authorized: bool = connection.query_row(
        "SELECT EXISTS(SELECT 1 FROM cardbe_iroh_invites i JOIN cardbe_iroh_devices d ON d.invite_id=i.invite_id WHERE i.invite_id=?1 AND i.board_id=?2 AND i.secret=?3 AND d.node_id=?4 AND d.status=?5 AND i.permission=?6 AND i.enabled=1)",
        params![invite_id, board_id, secret, node_id, IrohDeviceStatus::Approved, IrohPermission::Editor], |row| row.get(0),
    )?;
    if authorized {
        Ok(())
    } else {
        Err(DomainError::PermissionDenied.into())
    }
}

fn editable_loro_doc(connection: &Connection, board_id: i64) -> StorageResult<Vec<u8>> {
    connection
        .query_row(
            "SELECT payload FROM cardbe_loro_docs WHERE board_id=?1",
            [board_id],
            |row| row.get(0),
        )
        .optional()?
        .ok_or_else(|| "Editable board is missing its Loro document".into())
}

// Bounds for board state received from a peer. Local edits are unbounded, so
// a board that already exceeds a bound keeps syncing as long as a received
// change does not enlarge that measure further.
const REMOTE_MAX_COLUMNS: usize = 1_000;
const REMOTE_MAX_TASKS: usize = 100_000;
const REMOTE_MAX_TEMPLATES: usize = 1_000;
// Serialized size of the CRDT-owned content, excluding Loro edit history,
// which grows with normal use.
const REMOTE_MAX_CONTENT_BYTES: usize = 64 * 1024 * 1024;

fn task_count(data: &StoredData) -> usize {
    data.columns
        .iter()
        .map(|column| column.tasks.len())
        .sum::<usize>()
        + data.archives.len()
}

fn content_bytes(data: &StoredData) -> StorageResult<usize> {
    struct Counter(usize);
    impl Write for Counter {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            self.0 += bytes.len();
            Ok(bytes.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    let mut counter = Counter(0);
    serde_json::to_writer(
        &mut counter,
        &(&data.columns, &data.archives, &data.templates),
    )?;
    Ok(counter.0)
}

fn within_remote_bound(received: usize, previous: usize, bound: usize) -> bool {
    received <= bound || received <= previous
}

/// Reject received board state that grows beyond the remote bounds. Without a
/// previous local state, the bounds apply absolutely.
fn validate_remote_board_size(
    previous: Option<&StoredData>,
    received: &StoredData,
) -> StorageResult<()> {
    let (previous_columns, previous_tasks, previous_templates, previous_bytes) = match previous {
        Some(data) => (
            data.columns.len(),
            task_count(data),
            data.templates.len(),
            content_bytes(data)?,
        ),
        None => (0, 0, 0, 0),
    };
    if within_remote_bound(received.columns.len(), previous_columns, REMOTE_MAX_COLUMNS)
        && within_remote_bound(task_count(received), previous_tasks, REMOTE_MAX_TASKS)
        && within_remote_bound(
            received.templates.len(),
            previous_templates,
            REMOTE_MAX_TEMPLATES,
        )
        && within_remote_bound(
            content_bytes(received)?,
            previous_bytes,
            REMOTE_MAX_CONTENT_BYTES,
        )
    {
        Ok(())
    } else {
        Err(DomainError::ShareLimitExceeded.into())
    }
}

fn validate_iroh_snapshot(
    data: &StoredData,
    role: IrohPermission,
    loro_update: Option<&[u8]>,
) -> StorageResult<()> {
    validate_remote_board_size(None, data)?;
    if role == IrohPermission::Editor {
        let update = loro_update
            .filter(|bytes| !bytes.is_empty())
            .ok_or("Editable snapshot is missing its Loro document")?;
        let projected = crate::loro_board::project(update)
            .map_err(|error| format!("Invalid Loro document: {error}"))?;
        if projected.columns != data.columns
            || projected.archives != data.archives
            || projected.templates != data.templates
        {
            return Err("Shared snapshot and Loro document disagree".into());
        }
    }
    Ok(())
}

fn write_board_transaction(
    tx: &Transaction<'_>,
    board_id: i64,
    data: &StoredData,
) -> StorageResult<()> {
    // A snapshot is authoritative. Diffing against an empty value alone cannot
    // remove rows that are absent from the incoming snapshot.
    tx.execute("DELETE FROM cardbe_tasks WHERE board_id=?1", [board_id])?;
    tx.execute("DELETE FROM cardbe_columns WHERE board_id=?1", [board_id])?;
    tx.execute("DELETE FROM cardbe_archives WHERE board_id=?1", [board_id])?;
    tx.execute("DELETE FROM cardbe_templates WHERE board_id=?1", [board_id])?;
    tx.execute(
        "DELETE FROM cardbe_board_metadata WHERE board_id=?1 AND key <> 'link_reference'",
        [board_id],
    )?;
    let mut p = DatabasePositions::default();
    persist_board_diff_transaction(tx, board_id, &StoredData::default(), data, &mut p, true)?;
    Ok(())
}

/// Persists a local edit committed outside the open board's Undo history
/// (plugin changesets and shared operations) and records it for synchronization.
#[allow(clippy::too_many_arguments)]
pub(crate) fn persist_local_board_change(
    tx: &Transaction<'_>,
    board_id: i64,
    role: BoardRole,
    before: &StoredData,
    after: &StoredData,
    positions: &mut DatabasePositions,
    peer: &mut crate::loro_board::LocalPeer,
    include_archives: bool,
    source: &EventSource,
) -> StorageResult<()> {
    persist_board_diff_transaction(tx, board_id, before, after, positions, include_archives)?;
    if before != after {
        mark_local_board_change(tx, board_id, role)?;
        persist_loro_local_delta(tx, board_id, before, after, peer)?;
        task_events::record(tx, board_id, before, after, source)?;
    }
    Ok(())
}

pub(crate) fn mark_local_board_change(
    tx: &Transaction<'_>,
    board_id: i64,
    role: BoardRole,
) -> StorageResult<()> {
    match role {
        BoardRole::Owner => {
            tx.execute(
                "UPDATE cardbe_boards SET sync_revision=sync_revision+1 WHERE id=?1",
                [board_id],
            )?;
        }
        BoardRole::Editor => {
            tx.execute(
                "UPDATE cardbe_boards SET sync_status=CASE WHEN sync_status=?2 THEN ?2 ELSE ?3 END WHERE id=?1",
                params![board_id, SyncStatus::Conflict, SyncStatus::Pending],
            )?;
        }
        _ => return Err(DomainError::PermissionDenied.into()),
    }
    Ok(())
}

pub(crate) fn persist_loro_local_delta(
    tx: &Transaction<'_>,
    board_id: i64,
    before: &StoredData,
    after: &StoredData,
    peer: &mut crate::loro_board::LocalPeer,
) -> StorageResult<()> {
    let existing = tx
        .query_row(
            "SELECT payload FROM cardbe_loro_docs WHERE board_id=?1",
            [board_id],
            |row| row.get::<_, Vec<u8>>(0),
        )
        .optional()?;
    // Legacy boards have no document yet. Seed it from the complete current
    // projection, then subsequent writes are deltas.
    let update = if existing.is_some() {
        crate::loro_board::apply_local_delta_with_peer(existing.as_deref(), before, after, peer)
    } else {
        crate::loro_board::apply_local_delta_with_peer(None, &StoredData::default(), after, peer)
    }
    .map_err(|error| format!("Could not update shared-board CRDT: {error}"))?;
    tx.execute(
        "INSERT INTO cardbe_loro_docs(board_id,payload) VALUES(?1,?2)
         ON CONFLICT(board_id) DO UPDATE SET payload=excluded.payload",
        params![board_id, update],
    )?;
    Ok(())
}

fn load_board_positions(
    connection: &Connection,
    table: &str,
    board_id: i64,
) -> StorageResult<HashMap<i64, i64>> {
    let sql = match table {
        "cardbe_columns" => "SELECT id,position FROM cardbe_columns WHERE board_id=?1",
        "cardbe_tasks" => "SELECT id,position FROM cardbe_tasks WHERE board_id=?1",
        _ => return Err(format!("Unsupported position table: {table}").into()),
    };
    let mut s = connection.prepare(sql)?;
    let result = s
        .query_map([board_id], |r| Ok((r.get(0)?, r.get(1)?)))?
        .collect::<Result<_, _>>()?;
    Ok(result)
}

fn table_columns(tx: &Transaction<'_>, table: &str) -> StorageResult<Vec<(String, String)>> {
    let sql = match table {
        "cardbe_boards" => "PRAGMA table_info(cardbe_boards)",
        "cardbe_columns" => "PRAGMA table_info(cardbe_columns)",
        "cardbe_tasks" => "PRAGMA table_info(cardbe_tasks)",
        "boards" => "PRAGMA table_info(boards)",
        "board_snapshots" => "PRAGMA table_info(board_snapshots)",
        _ => return Err(format!("Unsupported schema table: {table}").into()),
    };
    let mut s = tx.prepare(sql)?;
    let result = s
        .query_map([], |r| Ok((r.get(1)?, r.get(2)?)))?
        .collect::<Result<_, _>>()?;
    Ok(result)
}
fn table_exists(tx: &Transaction<'_>, table: &str) -> StorageResult<bool> {
    Ok(tx.query_row(
        "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name=?1)",
        [table],
        |r| r.get(0),
    )?)
}
fn validate_cardbe_schema(tx: &Transaction<'_>) -> StorageResult<()> {
    for (t, required) in [
        ("cardbe_boards", vec!["id", "name"]),
        (
            "cardbe_columns",
            vec!["board_id", "id", "name", "color", "sort_order", "position"],
        ),
        (
            "cardbe_tasks",
            vec!["board_id", "id", "column_id", "position", "payload"],
        ),
    ] {
        let columns = table_columns(tx, t)?
            .into_iter()
            .map(|v| v.0)
            .collect::<Vec<_>>();
        if !required.iter().all(|c| columns.iter().any(|v| v == c)) {
            return Err(format!(
                "Cardbe database schema conflict in {t}; expected columns: {}",
                required.join(", ")
            )
            .into());
        }
    }
    Ok(())
}

fn experimental_snapshots(tx: &Transaction<'_>) -> StorageResult<Vec<(i64, String, StoredData)>> {
    let has_boards = table_exists(tx, "boards")?;
    let has_snapshots = table_exists(tx, "board_snapshots")?;
    if !has_boards && !has_snapshots {
        return Ok(vec![]);
    }
    if has_boards && !has_snapshots {
        let bc = table_columns(tx, "boards")?;
        let looks_experimental = bc
            .iter()
            .any(|(n, t)| n == "id" && t.eq_ignore_ascii_case("INTEGER"))
            && bc.iter().any(|(n, _)| n == "name");
        if !looks_experimental {
            return Ok(vec![]);
        }
    }
    if !(has_boards && has_snapshots) {
        return Err("Incomplete experimental multi-board schema: both boards and board_snapshots are required. No data was changed.".into());
    }
    let bc = table_columns(tx, "boards")?;
    let sc = table_columns(tx, "board_snapshots")?;
    let valid = bc
        .iter()
        .any(|(n, t)| n == "id" && t.eq_ignore_ascii_case("INTEGER"))
        && bc.iter().any(|(n, _)| n == "name")
        && sc.iter().any(|(n, _)| n == "board_id")
        && sc.iter().any(|(n, _)| n == "payload");
    if !valid {
        return Ok(vec![]);
    } // Unknown/conflicting tables belong to another application.
    let mut s=tx.prepare("SELECT b.id,b.name,s.payload FROM boards b JOIN board_snapshots s ON s.board_id=b.id ORDER BY b.id")?;
    let rows = s
        .query_map([], |r| {
            Ok((
                r.get::<_, i64>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, String>(2)?,
            ))
        })?
        .collect::<Result<Vec<_>, _>>()?;
    let board_count: i64 = tx.query_row("SELECT count(*) FROM boards", [], |r| r.get(0))?;
    if rows.len() as i64 != board_count {
        return Err("Experimental board snapshots are incomplete; migration was rolled back to avoid data loss.".into());
    }
    rows.into_iter()
        .map(|(id, name, payload)| {
            let data = serde_json::from_str(&payload).map_err(|e| {
                format!("Invalid snapshot for experimental board {id} ({name}): {e}")
            })?;
            Ok((id, name, data))
        })
        .collect()
}

fn legacy_metadata_parse<T: std::str::FromStr>(
    tx: &Transaction<'_>,
    key: &str,
) -> StorageResult<Option<T>>
where
    T::Err: std::error::Error + 'static,
{
    let v: Option<String> = tx
        .query_row("SELECT value FROM metadata WHERE key=?1", [key], |r| {
            r.get(0)
        })
        .optional()?;
    Ok(v.map(|v| v.parse()).transpose()?)
}
fn namespaced_metadata_value(c: &Connection, key: &str) -> rusqlite::Result<Option<String>> {
    c.query_row(
        "SELECT value FROM cardbe_global_metadata WHERE key=?1",
        [key],
        |r| r.get(0),
    )
    .optional()
}
fn namespaced_metadata_parse<T: std::str::FromStr>(
    c: &Connection,
    key: &str,
) -> StorageResult<Option<T>>
where
    T::Err: std::error::Error + 'static,
{
    Ok(namespaced_metadata_value(c, key)?
        .map(|v| v.parse())
        .transpose()?)
}
fn namespaced_metadata_json<T: serde::de::DeserializeOwned>(
    c: &Connection,
    key: &str,
) -> StorageResult<Option<T>> {
    Ok(namespaced_metadata_value(c, key)?
        .map(|v| serde_json::from_str(&v))
        .transpose()?)
}
fn board_metadata_value(c: &Connection, b: i64, key: &str) -> rusqlite::Result<Option<String>> {
    c.query_row(
        "SELECT value FROM cardbe_board_metadata WHERE board_id=?1 AND key=?2",
        params![b, key],
        |r| r.get(0),
    )
    .optional()
}
fn board_metadata_parse<T: std::str::FromStr>(
    c: &Connection,
    b: i64,
    key: &str,
) -> StorageResult<Option<T>>
where
    T::Err: std::error::Error + 'static,
{
    Ok(board_metadata_value(c, b, key)?
        .map(|v| v.parse())
        .transpose()?)
}
fn board_metadata_json<T: serde::de::DeserializeOwned>(
    c: &Connection,
    b: i64,
    key: &str,
) -> StorageResult<Option<T>> {
    Ok(board_metadata_value(c, b, key)?
        .map(|v| serde_json::from_str(&v))
        .transpose()?)
}

fn task_locations(data: &StoredData) -> HashMap<i64, (i64, &Task)> {
    data.columns
        .iter()
        .flat_map(|column| {
            column
                .tasks
                .iter()
                .map(move |task| (task.id, (column.id, task)))
        })
        .collect()
}

fn assign_sparse_positions(
    ids: &[i64],
    old_positions: &HashMap<i64, i64>,
) -> StorageResult<HashMap<i64, i64>> {
    if ids.is_empty() {
        return Ok(HashMap::new());
    }

    let existing = ids
        .iter()
        .enumerate()
        .filter_map(|(index, id)| old_positions.get(id).map(|position| (index, *position)))
        .collect::<Vec<_>>();
    let mut tails: Vec<usize> = Vec::new();
    let mut previous = vec![None; existing.len()];
    for sequence_index in 0..existing.len() {
        let position = existing[sequence_index].1;
        let insertion = tails.partition_point(|tail| existing[*tail].1 < position);
        if insertion > 0 {
            previous[sequence_index] = Some(tails[insertion - 1]);
        }
        if insertion == tails.len() {
            tails.push(sequence_index);
        } else {
            tails[insertion] = sequence_index;
        }
    }

    let mut anchors = vec![false; ids.len()];
    let mut cursor = tails.last().copied();
    while let Some(sequence_index) = cursor {
        anchors[existing[sequence_index].0] = true;
        cursor = previous[sequence_index];
    }

    let mut assigned = HashMap::new();
    for (index, id) in ids.iter().enumerate() {
        if anchors[index] {
            assigned.insert(*id, old_positions[id]);
        }
    }

    let mut start = 0;
    while start < ids.len() {
        if anchors[start] {
            start += 1;
            continue;
        }
        let end = (start..ids.len())
            .find(|index| anchors[*index])
            .unwrap_or(ids.len());
        let count = end - start;
        let left = start
            .checked_sub(1)
            .filter(|index| anchors[*index])
            .map(|index| assigned[&ids[index]]);
        let right = (end < ids.len()).then(|| assigned[&ids[end]]);

        let values = sparse_values_between(left, right, count);
        let Some(values) = values else {
            return evenly_spaced_positions(ids);
        };
        for (id, position) in ids[start..end].iter().zip(values) {
            assigned.insert(*id, position);
        }
        start = end;
    }
    Ok(assigned)
}

fn sparse_values_between(left: Option<i64>, right: Option<i64>, count: usize) -> Option<Vec<i64>> {
    let count = i128::try_from(count).ok()?;
    match (left, right) {
        (Some(left), Some(right)) => {
            let available = i128::from(right) - i128::from(left);
            if available <= count {
                return None;
            }
            let step = available / (count + 1);
            (1..=count)
                .map(|offset| i64::try_from(i128::from(left) + step * offset).ok())
                .collect()
        }
        (Some(left), None) => (1..=count)
            .map(|offset| {
                i64::try_from(i128::from(left) + i128::from(SORT_POSITION_GAP) * offset).ok()
            })
            .collect(),
        (None, Some(right)) => (1..=count)
            .map(|offset| {
                let reverse_offset = count - offset + 1;
                i64::try_from(i128::from(right) - i128::from(SORT_POSITION_GAP) * reverse_offset)
                    .ok()
            })
            .collect(),
        (None, None) => evenly_spaced_values(count),
    }
}

fn evenly_spaced_values(count: i128) -> Option<Vec<i64>> {
    (1..=count)
        .map(|offset| i64::try_from(i128::from(SORT_POSITION_GAP) * offset).ok())
        .collect()
}

fn evenly_spaced_positions(ids: &[i64]) -> StorageResult<HashMap<i64, i64>> {
    let values = evenly_spaced_values(i128::try_from(ids.len())?)
        .ok_or("Too many items to assign sort positions")?;
    Ok(ids.iter().copied().zip(values).collect())
}

fn sync_archives(
    transaction: &Transaction<'_>,
    before: &[Archive],
    after: &[Archive],
) -> StorageResult<usize> {
    let old = before
        .iter()
        .enumerate()
        .map(|(position, archive)| (archive.task.id, (position, archive)))
        .collect::<HashMap<_, _>>();
    let new = after
        .iter()
        .enumerate()
        .map(|(position, archive)| (archive.task.id, (position, archive)))
        .collect::<HashMap<_, _>>();
    let mut changed = 0;
    for id in old.keys().filter(|id| !new.contains_key(id)) {
        transaction.execute("DELETE FROM archives WHERE task_id = ?1", [id])?;
        changed += 1;
    }
    for (id, (position, archive)) in new {
        if old
            .get(&id)
            .is_none_or(|(old_position, old)| old_position != &position || old != &archive)
        {
            transaction.execute(
                "INSERT INTO archives(task_id, archived_at, position, payload)
                 VALUES (?1, ?2, ?3, ?4)
                 ON CONFLICT(task_id) DO UPDATE SET
                   archived_at = excluded.archived_at,
                   position = excluded.position,
                   payload = excluded.payload",
                params![
                    id,
                    archive.time.to_string(),
                    index_i64(position)?,
                    serde_json::to_string(&archive.task)?
                ],
            )?;
            changed += 1;
        }
    }
    Ok(changed)
}

fn sync_templates(
    transaction: &Transaction<'_>,
    before: &[TaskTemplate],
    after: &[TaskTemplate],
) -> StorageResult<usize> {
    let old = before
        .iter()
        .enumerate()
        .map(|(position, template)| (template.id, (position, template)))
        .collect::<HashMap<_, _>>();
    let new = after
        .iter()
        .enumerate()
        .map(|(position, template)| (template.id, (position, template)))
        .collect::<HashMap<_, _>>();
    let mut changed = 0;
    for id in old.keys().filter(|id| !new.contains_key(id)) {
        transaction.execute("DELETE FROM templates WHERE id = ?1", [id])?;
        changed += 1;
    }
    for (id, (position, template)) in new {
        if old
            .get(&id)
            .is_none_or(|(old_position, old)| old_position != &position || old != &template)
        {
            transaction.execute(
                "INSERT INTO templates(id, name, position, payload) VALUES (?1, ?2, ?3, ?4)
                 ON CONFLICT(id) DO UPDATE SET
                   name = excluded.name,
                   position = excluded.position,
                   payload = excluded.payload",
                params![
                    id,
                    template.name,
                    index_i64(position)?,
                    serde_json::to_string(&template.task)?
                ],
            )?;
            changed += 1;
        }
    }
    Ok(changed)
}

fn metadata_exists(transaction: &Transaction<'_>, key: &str) -> rusqlite::Result<bool> {
    transaction
        .query_row("SELECT 1 FROM metadata WHERE key = ?1", [key], |_| Ok(()))
        .optional()
        .map(|value| value.is_some())
}

fn metadata_value(connection: &Connection, key: &str) -> rusqlite::Result<Option<String>> {
    connection
        .query_row("SELECT value FROM metadata WHERE key = ?1", [key], |row| {
            row.get(0)
        })
        .optional()
}

fn metadata_parse<T: std::str::FromStr>(
    connection: &Connection,
    key: &str,
) -> StorageResult<Option<T>>
where
    T::Err: std::error::Error + 'static,
{
    metadata_value(connection, key)?
        .map(|value| value.parse().map_err(Into::into))
        .transpose()
}

fn metadata_json<T: serde::de::DeserializeOwned>(
    connection: &Connection,
    key: &str,
) -> StorageResult<Option<T>> {
    metadata_value(connection, key)?
        .map(|value| serde_json::from_str(&value).map_err(Into::into))
        .transpose()
}

fn index_i64(index: usize) -> StorageResult<i64> {
    Ok(i64::try_from(index)?)
}

fn load_json_or_legacy(
    app_data_dir: &Path,
    messages: &mut Vec<String>,
) -> StorageResult<(StoredData, bool)> {
    let data_path = app_data_dir.join("data.json");
    let backup = backup_path(&data_path);
    let had_current_files = data_path.try_exists()? || backup.try_exists()?;
    match read_document(&data_path) {
        Ok(Some(data)) => return Ok((data, true)),
        Ok(None) => {}
        Err(error) => messages.push(quarantine_file(&data_path, "application", &*error)),
    }
    match read_document(&backup) {
        Ok(Some(data)) => {
            messages.push(format!(
                "The SQLite database was migrated from {} because the primary JSON file was unavailable.",
                backup.display()
            ));
            return Ok((data, false));
        }
        Ok(None) => {}
        Err(error) => messages.push(quarantine_file(&backup, "backup", &*error)),
    }

    if had_current_files {
        messages.push(
            "No valid current application data could be recovered; a new empty database was created."
                .to_string(),
        );
        return Ok((StoredData::default(), false));
    }

    let stored = StoredData {
        columns: read_legacy(app_data_dir.join("tasks.json"), "legacy task", messages)
            .unwrap_or_default(),
        archives: read_legacy(
            app_data_dir.join("archives.json"),
            "legacy archive",
            messages,
        )
        .unwrap_or_default(),
        settings: read_legacy(
            app_data_dir.join("settings.json"),
            "legacy settings",
            messages,
        )
        .unwrap_or_default(),
        ..StoredData::default()
    };
    Ok((stored, false))
}

fn preserve_migrated_json(app_data_dir: &Path, messages: &mut Vec<String>) {
    let source = app_data_dir.join("data.json");
    if !source.exists() {
        return;
    }
    let destination = app_data_dir.join("data.migrated.json");
    if destination.exists() {
        return;
    }
    match fs::rename(&source, &destination) {
        Ok(()) => messages.push(format!(
            "The previous JSON data was migrated to SQLite and preserved at {}.",
            destination.display()
        )),
        Err(error) => messages.push(format!(
            "The data was migrated to SQLite, but the previous JSON file could not be renamed: {error}."
        )),
    }
}

fn read_document(path: &Path) -> StorageResult<Option<StoredData>> {
    if !path.try_exists()? {
        return Ok(None);
    }
    let file = File::open(path)?;
    Ok(Some(serde_json::from_reader(BufReader::new(file))?))
}

fn read_legacy<T: serde::de::DeserializeOwned>(
    path: PathBuf,
    name: &str,
    messages: &mut Vec<String>,
) -> Option<T> {
    if !path.exists() {
        return None;
    }
    let file = match File::open(&path) {
        Ok(file) => file,
        Err(error) => {
            messages.push(quarantine_file(&path, name, &error));
            return None;
        }
    };
    match serde_json::from_reader(BufReader::new(file)) {
        Ok(data) => Some(data),
        Err(error) => {
            messages.push(quarantine_file(&path, name, &error));
            None
        }
    }
}

pub(crate) fn flow_note_ids(flow: &crate::flow::FlowData) -> std::collections::BTreeSet<i64> {
    std::iter::once(&flow.layout)
        .chain(flow.tabs.iter().filter_map(|tab| tab.layout.as_ref()))
        .flat_map(|layout| &layout.annotations)
        .filter_map(|annotation| match annotation.content {
            crate::flow::AnnotationContent::Note { note_id } => Some(note_id),
            _ => None,
        })
        .collect()
}

pub(crate) fn validate_single_board_notes(
    flow: &crate::flow::FlowData,
    notes: &[Note],
) -> Result<(), DomainError> {
    let referenced = flow_note_ids(flow);
    let mut seen = std::collections::HashSet::new();
    if referenced.iter().any(|id| *id < 0)
        || notes
            .iter()
            .any(|note| !referenced.contains(&note.id) || !seen.insert(note.id))
    {
        return Err(DomainError::InvalidArgument);
    }
    Ok(())
}

fn restore_single_board_flow(
    tx: &Transaction<'_>,
    board_id: i64,
    flow: &crate::flow::FlowData,
    notes: &[Note],
) -> StorageResult<()> {
    validate_single_board_notes(flow, notes)?;
    let notes: HashMap<_, _> = notes.iter().map(|note| (note.id, note)).collect();
    let mut ids = HashMap::new();
    for old_id in flow_note_ids(flow) {
        if let Some(note) = notes.get(&old_id) {
            tx.execute(
                "INSERT INTO notes(title,content,pinned,created_at,updated_at) VALUES(?1,?2,?3,?4,?5)",
                params![note.title, note.content, note.pinned, note.created_at, note.updated_at],
            )?;
            ids.insert(old_id, tx.last_insert_rowid());
        } else {
            // Deleted notes can leave annotations behind. Reserve an absent ID
            // so that the reference stays absent, even if the target database
            // has a different note at the original ID or creates notes later.
            tx.execute("INSERT INTO notes(created_at,updated_at) VALUES(0,0)", [])?;
            let id = tx.last_insert_rowid();
            tx.execute("DELETE FROM notes WHERE id=?1", [id])?;
            ids.insert(old_id, id);
        }
    }
    let mut restored = flow.clone();
    for layout in std::iter::once(&mut restored.layout).chain(
        restored
            .tabs
            .iter_mut()
            .filter_map(|tab| tab.layout.as_mut()),
    ) {
        for annotation in &mut layout.annotations {
            if let crate::flow::AnnotationContent::Note { note_id } = &mut annotation.content {
                *note_id = ids[note_id];
            }
        }
    }
    crate::flow::restore(tx, board_id, &restored)
}

pub fn write_document(path: &Path, data: &impl serde::Serialize) -> StorageResult<()> {
    let temp_path = path.with_extension("json.tmp");
    let backup = backup_path(path);
    let result = (|| {
        let mut file = File::create(&temp_path)?;
        serde_json::to_writer_pretty(&mut file, data)?;
        file.write_all(b"\n")?;
        file.sync_all()?;
        if path.try_exists()? {
            if backup.try_exists()? {
                fs::remove_file(&backup)?;
            }
            fs::rename(path, &backup)?;
        }
        if let Err(error) = fs::rename(&temp_path, path) {
            if !path.exists() && backup.exists() {
                let _ = fs::rename(&backup, path);
            }
            return Err(error.into());
        }
        StorageResult::Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(temp_path);
    }
    result
}

fn backup_path(path: &Path) -> PathBuf {
    PathBuf::from(format!("{}.bak", path.display()))
}

fn quarantine_file(path: &Path, name: &str, error: &dyn std::fmt::Display) -> String {
    let timestamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis())
        .unwrap_or_default();
    let quarantine = PathBuf::from(format!("{}.corrupt-{timestamp}", path.display()));
    match fs::rename(path, &quarantine) {
        Ok(()) => format!(
            "The {name} data file was unreadable and was moved to {} ({error}).",
            quarantine.display()
        ),
        Err(backup_error) => format!(
            "The {name} data file was unreadable ({error}), and could not be preserved: {backup_error}."
        ),
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn legacy_iroh_enablement_preserves_usage_and_explicit_preferences() {
        for usage in ["none", "invite", "disabled_invite", "remote", "revocation"] {
            for preference in [None, Some(false), Some(true)] {
                let dir = test_dir("iroh-enablement-migration");
                let mut loaded = load(&dir).unwrap();
                assert!(!loaded.stored.settings.iroh_network.enabled);
                let db = &mut loaded.database;
                let board_id = db.active_board_id();
                match usage {
                    "invite" | "disabled_invite" | "revocation" => {
                        db.save_iroh_invite("invite", board_id, "secret", IrohPermission::Viewer)
                            .unwrap();
                        if usage == "disabled_invite" {
                            db.update_iroh_invite("invite", None, Some(false)).unwrap();
                        } else if usage == "revocation" {
                            db.delete_iroh_invite("invite").unwrap();
                        }
                    }
                    "remote" => {
                        db.create_received_board(
                            "Received",
                            &StoredData::default(),
                            IrohPermission::Viewer,
                            1,
                            "fixture-ticket",
                            None,
                        )
                        .unwrap();
                    }
                    _ => {}
                }
                let mut expected = loaded.stored.settings.clone();
                expected.notify_enabled = true;
                expected.iroh_network.listen_port = 12345;
                expected.iroh_network.relay_urls = vec!["https://relay.example.com".into()];
                let mut value = serde_json::to_value(&expected).unwrap();
                if let Some(enabled) = preference {
                    value["iroh_network"]["enabled"] = enabled.into();
                } else {
                    value["iroh_network"]
                        .as_object_mut()
                        .unwrap()
                        .remove("enabled");
                }
                db.connection
                    .execute(
                        "UPDATE cardbe_global_metadata SET value=?1 WHERE key='settings'",
                        [value.to_string()],
                    )
                    .unwrap();
                let boards = db.boards().unwrap();
                drop(loaded);

                expected.iroh_network.enabled = preference.unwrap_or(usage != "none");
                for _ in 0..2 {
                    let reopened = load(&dir).unwrap();
                    assert_eq!(reopened.stored.settings, expected, "{usage} {preference:?}");
                    assert_eq!(reopened.database.boards().unwrap(), boards);
                    let persisted: crate::models::Settings =
                        namespaced_metadata_json(&reopened.database.connection, "settings")
                            .unwrap()
                            .unwrap();
                    assert_eq!(persisted, expected);
                }
                fs::remove_dir_all(dir).unwrap();
            }
        }
    }

    #[test]
    fn access_probes_preserve_revocation_until_a_fresh_join_request() {
        let dir = test_dir("iroh-revocation-probe");
        let mut loaded = load(&dir).unwrap();
        let db = &mut loaded.database;
        db.save_iroh_invite(
            "invite",
            db.active_board_id(),
            "secret",
            IrohPermission::Editor,
        )
        .unwrap();
        db.iroh_device_access("invite", "secret", "device", true)
            .unwrap();
        db.set_iroh_device_approved("invite", "device", true)
            .unwrap();
        db.set_iroh_device_approved("invite", "device", false)
            .unwrap();
        assert_eq!(
            db.iroh_device_access("invite", "secret", "device", false)
                .unwrap(),
            Some(IrohDeviceStatus::Revoked)
        );
        assert_eq!(
            db.iroh_device_access("invite", "secret", "device", true)
                .unwrap(),
            Some(IrohDeviceStatus::Pending)
        );
        drop(loaded);
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn deleting_received_board_clears_scoped_data_and_preserves_local_boards() {
        let dir = test_dir("iroh-revoked-copy");
        let mut loaded = load(&dir).unwrap();
        let db = &mut loaded.database;
        let local_id = db.active_board_id();
        let note = db
            .create_note_with_content(1, "Local note".into(), "Keep".into())
            .unwrap();
        crate::plugins::storage::initialize(&db.connection).unwrap();
        let data: StoredData = serde_json::from_value(serde_json::json!({
            "columns": [{"id": 1, "name": "Shared column", "tasks": [{"id": 1, "title": "Shared task"}]}],
            "archives": [{"time": 1, "task": {"id": 2, "title": "Archived task"}}],
            "templates": [{"id": 1, "name": "Shared template", "task": {"title": "Template task"}}]
        })).unwrap();
        let doc =
            crate::loro_board::apply_local_delta(None, &StoredData::default(), &data).unwrap();
        let received = db
            .create_received_board(
                "Shared",
                &data,
                IrohPermission::Editor,
                1,
                "ticket",
                Some(&doc),
            )
            .unwrap();
        db.save_iroh_conflict(
            received.id,
            2,
            "Shared",
            IrohPermission::Editor,
            &data,
            Some(&doc),
        )
        .unwrap();
        let mut pending = data.clone();
        pending.columns[0].tasks[0].title = "Unsent local edit".into();
        db.replace_board_as_local_edit(received.id, &pending)
            .unwrap();
        assert!(db.iroh_loro_update(received.id).unwrap().is_some());
        assert_eq!(db.archived_task_count(received.id).unwrap(), 1);
        for board_id in [local_id, received.id] {
            db.connection
                .execute(
                    "INSERT INTO cardbe_plugin_sources(instance_id,external_key,board_id,task_id,last_import) VALUES('instance','key',?1,1,'')",
                    [board_id],
                )
                .unwrap();
        }
        db.delete_board(received.id).unwrap();
        assert_eq!(db.archived_task_count(received.id).unwrap(), 0);
        let sources: Vec<i64> = db
            .connection
            .prepare("SELECT board_id FROM cardbe_plugin_sources")
            .unwrap()
            .query_map([], |row| row.get(0))
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap();
        assert_eq!(sources, vec![local_id]);
        assert!(db.board_exists(local_id).unwrap());
        assert!(!db.board_exists(received.id).unwrap());
        assert!(db.iroh_remote(received.id).unwrap().is_none());
        assert!(db.iroh_conflict(received.id).unwrap().is_none());
        assert!(db.iroh_loro_update(received.id).unwrap().is_none());
        for table in [
            "cardbe_columns",
            "cardbe_tasks",
            "cardbe_archives",
            "cardbe_templates",
            "cardbe_board_metadata",
        ] {
            let remaining: i64 = db
                .connection
                .query_row(
                    &format!("SELECT COUNT(*) FROM {table} WHERE board_id=?1"),
                    [received.id],
                    |row| row.get(0),
                )
                .unwrap();
            assert_eq!(remaining, 0);
        }
        assert!(db
            .get_notes()
            .unwrap()
            .iter()
            .any(|item| item.id == note.id));
        drop(loaded);
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn deleted_invitations_keep_revocation_service_available_after_restart() {
        for cascade in [false, true] {
            let dir = test_dir("iroh-revocation-host");
            let mut loaded = load(&dir).unwrap();
            let db = &mut loaded.database;
            assert!(!db.iroh_host_required().unwrap());
            let board = db.create_board("Shared").unwrap();
            db.save_iroh_invite("invite", board.id, "secret", IrohPermission::Viewer)
                .unwrap();
            assert_eq!(
                db.iroh_invite("invite").unwrap(),
                Some((
                    "invite".into(),
                    board.id,
                    "secret".into(),
                    IrohPermission::Viewer,
                    true
                ))
            );
            assert_eq!(db.iroh_invite("other").unwrap(), None);
            assert!(db.iroh_host_required().unwrap());
            db.update_iroh_invite("invite", None, Some(false)).unwrap();
            assert!(!db.iroh_host_required().unwrap());
            if cascade {
                db.delete_board(board.id).unwrap();
            } else {
                db.delete_iroh_invite("invite").unwrap();
            }
            assert!(db.iroh_invites().unwrap().is_empty());
            assert!(db.iroh_host_required().unwrap());
            drop(loaded);
            let reopened = load(&dir).unwrap();
            assert!(reopened.database.iroh_host_required().unwrap());
            drop(reopened);
            fs::remove_dir_all(dir).unwrap();
        }
    }

    #[test]
    fn local_writer_is_stable_until_snapshot_replacement_or_reopen() {
        let dir = test_dir("loro-session-peer");
        fs::create_dir_all(&dir).unwrap();
        let mut loaded = load(&dir).unwrap();
        let board_id = loaded.database.active_board_id();
        let mut data = StoredData::default();
        for index in 0..10 {
            data.templates = vec![TaskTemplate {
                id: 1,
                name: format!("Edit {index}"),
                task: Task::default(),
            }];
            loaded
                .database
                .replace_board_as_local_edit(board_id, &data)
                .unwrap();
            let update = loaded.database.iroh_loro_update(board_id).unwrap().unwrap();
            let vector = loro::VersionVector::decode(
                &crate::loro_board::state_vector(Some(&update)).unwrap(),
            )
            .unwrap();
            assert_eq!(vector.len(), 1);
        }
        let update = loaded.database.iroh_loro_update(board_id).unwrap().unwrap();
        // Even a replacement containing our complete history starts a new writer.
        loaded
            .database
            .apply_iroh_snapshot(
                board_id,
                "Board",
                IrohPermission::Editor,
                1,
                &data,
                Some(&update),
            )
            .unwrap();
        data.templates[0].name = "After replacement".into();
        loaded
            .database
            .replace_board_as_local_edit(board_id, &data)
            .unwrap();
        let update = loaded.database.iroh_loro_update(board_id).unwrap().unwrap();
        let vector =
            loro::VersionVector::decode(&crate::loro_board::state_vector(Some(&update)).unwrap())
                .unwrap();
        assert_eq!(vector.len(), 2);
        drop(loaded);
        let mut loaded = load(&dir).unwrap();
        data.templates[0].name = "After restart".into();
        loaded
            .database
            .replace_board_as_local_edit(board_id, &data)
            .unwrap();
        let update = loaded.database.iroh_loro_update(board_id).unwrap().unwrap();
        let vector =
            loro::VersionVector::decode(&crate::loro_board::state_vector(Some(&update)).unwrap())
                .unwrap();
        assert_eq!(vector.len(), 3);
        assert_eq!(
            crate::loro_board::project(&update).unwrap().templates,
            data.templates
        );
        drop(loaded);
        fs::remove_dir_all(dir).unwrap();
    }
    use super::*;
    use crate::models::TaskItem;

    #[test]
    fn board_summaries_identify_received_and_hosted_sharing() {
        let dir = test_dir("shared-board-summaries");
        fs::create_dir_all(&dir).unwrap();
        let mut loaded = load(&dir).unwrap();
        let owner = loaded.database.active_board_id();
        assert!(!loaded.database.boards().unwrap()[0].is_shared);
        loaded
            .database
            .save_iroh_invite("invite", owner, "secret", IrohPermission::Viewer)
            .unwrap();
        assert!(loaded.database.boards().unwrap()[0].is_shared);
        loaded.database.delete_iroh_invite("invite").unwrap();
        assert!(!loaded.database.boards().unwrap()[0].is_shared);
        for role in [BoardRole::Viewer, BoardRole::Editor] {
            loaded
                .database
                .set_shared_board(owner, role, SyncStatus::Synced, 1)
                .unwrap();
            assert!(loaded.database.boards().unwrap()[0].is_shared);
        }
        drop(loaded);
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn missing_boards_preserve_domain_errors_at_the_command_boundary() {
        let dir = test_dir("missing-board-errors");
        fs::create_dir_all(&dir).unwrap();
        let mut loaded = load(&dir).unwrap();
        let active = loaded.database.active_board_id();
        for error in [
            loaded.database.board_role(-1).unwrap_err(),
            loaded
                .database
                .move_tasks_to_board(active, &[1], -1, 1)
                .unwrap_err(),
        ] {
            let value =
                serde_json::to_value(crate::errors::CommandError::repository(error)).unwrap();
            assert_eq!(value, serde_json::json!({"code": "BOARD_NOT_FOUND"}));
        }
        drop(loaded);
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn task_explorer_keeps_date_only_due_dates_on_time_until_the_day_ends() {
        use chrono::{Local, TimeZone};
        let at = |day, hour| {
            Local
                .with_ymd_and_hms(2026, 10, day, hour, 0, 0)
                .earliest()
                .unwrap()
                .timestamp_millis()
        };
        let dir = test_dir("task-explorer-date-only-overdue");
        fs::create_dir_all(&dir).unwrap();
        let mut loaded = load(&dir).unwrap();
        let board_id = loaded.database.active_board_id();
        let data = StoredData {
            columns: vec![Column {
                id: 1,
                name: "Todo".into(),
                color: String::new(),
                sort_order: Default::default(),
                sort_rules: Vec::new(),
                done: false,
                tasks: [
                    (1, at(13, 0), false),
                    (2, at(14, 0), false),
                    (3, at(14, 9), false),
                    (4, at(14, 11), false),
                    (5, at(14, 0), true),
                ]
                .into_iter()
                .map(|(id, due_time, due_at_midnight)| Task {
                    id,
                    title: format!("Task {id}"),
                    due_time: Some(u128::try_from(due_time).unwrap()),
                    due_at_midnight,
                    ..Task::default()
                })
                .collect(),
            }],
            ..StoredData::default()
        };
        loaded
            .database
            .replace_board_as_local_edit(board_id, &data)
            .unwrap();
        let filter = TaskExplorerQuery {
            status: "overdue".into(),
            sort: "due".into(),
            now: at(14, 10),
            ..Default::default()
        };

        let rows = loaded.database.list_all_task_page(&filter, 0, 10).unwrap();
        assert_eq!(
            rows.iter().map(|row| row.task.id).collect::<Vec<_>>(),
            vec![1, 5, 3]
        );
    }

    #[test]
    fn task_explorer_filters_and_sorts_by_priority() {
        use crate::models::TaskPriority::{High, Low, Urgent};
        let dir = test_dir("task-explorer-priority");
        fs::create_dir_all(&dir).unwrap();
        let mut loaded = load(&dir).unwrap();
        let board_id = loaded.database.active_board_id();
        let data = StoredData {
            columns: vec![Column {
                id: 1,
                name: "Todo".into(),
                color: String::new(),
                sort_order: Default::default(),
                sort_rules: Vec::new(),
                done: false,
                tasks: [
                    (1, None, Some(5)),
                    (2, Some(Low), None),
                    (3, Some(Urgent), Some(9)),
                    (4, Some(Urgent), Some(2)),
                    (5, Some(High), None),
                    (6, Some(Urgent), None),
                ]
                .into_iter()
                .map(|(id, priority, due_time)| Task {
                    id,
                    title: format!("Task {id}"),
                    priority,
                    due_time,
                    ..Task::default()
                })
                .collect(),
            }],
            ..StoredData::default()
        };
        loaded
            .database
            .replace_board_as_local_edit(board_id, &data)
            .unwrap();
        let ids = |filter: &TaskExplorerQuery| {
            loaded
                .database
                .list_all_task_page(filter, 0, 10)
                .unwrap()
                .iter()
                .map(|row| row.task.id)
                .collect::<Vec<_>>()
        };

        // Most important first, then by due date with undated tasks last.
        let sorted = TaskExplorerQuery {
            status: "all".into(),
            sort: "priority".into(),
            ..Default::default()
        };
        assert_eq!(ids(&sorted), vec![4, 3, 6, 5, 2, 1]);

        let filtered = TaskExplorerQuery {
            priorities: Some(vec!["urgent".into(), "none".into()]),
            sort: "due".into(),
            ..sorted
        };
        assert_eq!(ids(&filtered), vec![4, 1, 3, 6]);

        // Searches filter by priority before ranking.
        let searched = TaskExplorerQuery {
            query: "Task".into(),
            priorities: Some(vec!["high".into(), "low".into()]),
            ..filtered
        };
        assert_eq!(ids(&searched), vec![2, 5]);
        drop(loaded);
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn task_explorer_filters_by_completion_labels_waiting_and_recurrence() {
        let dir = test_dir("task-explorer-smart-filters");
        fs::create_dir_all(&dir).unwrap();
        let mut loaded = load(&dir).unwrap();
        let board_id = loaded.database.active_board_id();
        let task = |id: i64, labels: &[&str], blocked_by: &[i64]| Task {
            id,
            title: format!("Task {id}"),
            labels: labels.iter().map(|label| label.to_string()).collect(),
            blocked_by: blocked_by.to_vec(),
            created_time: 1_000 + id as u128,
            ..Task::default()
        };
        let column = |id: i64, done: bool, tasks: Vec<Task>| Column {
            id,
            name: format!("Column {id}"),
            color: String::new(),
            sort_order: Default::default(),
            sort_rules: Vec::new(),
            done,
            tasks,
        };
        let data = StoredData {
            columns: vec![
                column(
                    1,
                    false,
                    vec![
                        // Waits on an open task and on a done one.
                        task(1, &["工作"], &[2, 3]),
                        task(2, &["home"], &[]),
                        Task {
                            recurrence: Some(crate::models::Recurrence {
                                frequency: crate::models::RecurrenceFrequency::Daily,
                                interval: 1,
                            }),
                            ..task(4, &[], &[3, 9])
                        },
                    ],
                ),
                column(2, true, vec![task(3, &["工作", "home"], &[])]),
            ],
            archives: vec![Archive {
                time: 5,
                task: task(5, &["工作"], &[2]),
            }],
            ..StoredData::default()
        };
        loaded
            .database
            .replace_board_as_local_edit(board_id, &data)
            .unwrap();
        let rows = |filter: TaskExplorerQuery| {
            loaded
                .database
                .list_all_task_page(
                    &TaskExplorerQuery {
                        status: "all".into(),
                        sort: "title".into(),
                        ..filter
                    },
                    0,
                    10,
                )
                .unwrap()
        };
        let ids = |filter: TaskExplorerQuery| {
            rows(filter)
                .iter()
                .map(|row| row.task.id)
                .collect::<Vec<_>>()
        };

        let all = rows(TaskExplorerQuery::default());
        let row = |id: i64| all.iter().find(|row| row.task.id == id).unwrap();
        assert!(row(3).done && !row(1).done && !row(5).done);
        assert_eq!(row(1).waiting_count, 1);
        assert_eq!(row(3).column_id, Some(2));
        assert_eq!(row(5).column_id, None);
        // References to done, missing and archived tasks never block.
        assert_eq!(row(4).waiting_count, 0);
        assert_eq!(row(5).waiting_count, 0);

        let completion = |states: &[&str]| TaskExplorerQuery {
            completion: Some(states.iter().map(|state| state.to_string()).collect()),
            ..Default::default()
        };
        assert_eq!(ids(completion(&["open"])), vec![1, 2, 4]);
        assert_eq!(ids(completion(&["done", "archived"])), vec![3, 5]);
        assert_eq!(
            ids(TaskExplorerQuery {
                labels: Some(vec!["工作".into()]),
                ..completion(&["open", "done"])
            }),
            vec![1, 3]
        );
        let waiting = |value: &str| TaskExplorerQuery {
            waiting: Some(value.into()),
            ..Default::default()
        };
        assert_eq!(ids(waiting("waiting")), vec![1]);
        assert_eq!(ids(waiting("not_waiting")), vec![2, 3, 4, 5]);
        // Only open tasks that open tasks wait on; the archived task does not count.
        assert_eq!(ids(waiting("waited_on")), vec![2]);
        let recurring = |value: bool| TaskExplorerQuery {
            recurring: Some(value),
            ..Default::default()
        };
        assert_eq!(ids(recurring(true)), vec![4]);
        assert_eq!(ids(recurring(false)), vec![1, 2, 3, 5]);

        // Searches apply the same filters before ranking.
        assert_eq!(
            ids(TaskExplorerQuery {
                query: "Task".into(),
                ..waiting("waited_on")
            }),
            vec![2]
        );
        let newest = loaded
            .database
            .list_all_task_page(
                &TaskExplorerQuery {
                    status: "active".into(),
                    sort: "created".into(),
                    ..Default::default()
                },
                0,
                10,
            )
            .unwrap();
        assert_eq!(
            newest.iter().map(|row| row.task.id).collect::<Vec<_>>(),
            vec![4, 3, 2, 1]
        );
        drop(loaded);
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn task_explorer_filters_and_sorts_before_paginating() {
        let dir = test_dir("task-explorer-pagination");
        fs::create_dir_all(&dir).unwrap();
        let mut loaded = load(&dir).unwrap();
        let first = loaded.database.active_board_id();
        let second = loaded.database.create_board("Second").unwrap().id;
        for (board_id, name) in [(first, "Zulu"), (second, "Alpha")] {
            let data = StoredData {
                columns: vec![Column {
                    id: 1,
                    name: name.into(),
                    color: String::new(),
                    sort_order: Default::default(),
                    sort_rules: Vec::new(),
                    done: false,
                    tasks: (1..=60)
                        .map(|id| Task {
                            id,
                            title: format!("Task {:02}", 61 - id),
                            due_time: Some((61 - id) as u128),
                            recurrence: (id == 60).then_some(crate::models::Recurrence {
                                frequency: crate::models::RecurrenceFrequency::Daily,
                                interval: 1,
                            }),
                            ..Task::default()
                        })
                        .chain(std::iter::once(Task {
                            id: 61,
                            title: "No date".into(),
                            ..Task::default()
                        }))
                        .collect(),
                }],
                archives: vec![Archive {
                    time: 100,
                    task: Task {
                        id: 62,
                        title: "Archived".into(),
                        due_time: Some(1),
                        ..Task::default()
                    },
                }],
                ..StoredData::default()
            };
            loaded
                .database
                .replace_board_as_local_edit(board_id, &data)
                .unwrap();
        }
        loaded
            .database
            .set_shared_board(second, BoardRole::Viewer, SyncStatus::Synced, 1)
            .unwrap();
        let mut filter = TaskExplorerQuery {
            status: "active".into(),
            sort: "due".into(),
            ..Default::default()
        };
        let page = loaded.database.list_all_task_page(&filter, 0, 50).unwrap();
        assert_eq!(page[0].task.due_time, Some(1));
        assert_eq!(page[49].task.due_time, Some(25));
        let next = loaded.database.list_all_task_page(&filter, 50, 50).unwrap();
        assert_eq!(next[0].task.due_time, Some(26));
        assert!(page.iter().all(|row| !next
            .iter()
            .any(|other| row.board_id == other.board_id && row.task.id == other.task.id)));

        filter.board_ids = Some(vec![first, second]);
        let selected = loaded.database.list_all_task_page(&filter, 0, 50).unwrap();
        assert_eq!(
            selected
                .iter()
                .map(|row| (row.board_id, row.task.id))
                .collect::<Vec<_>>(),
            page.iter()
                .map(|row| (row.board_id, row.task.id))
                .collect::<Vec<_>>()
        );
        filter.board_ids = Some(vec![]);
        assert!(loaded
            .database
            .list_all_task_page(&filter, 0, 50)
            .unwrap()
            .is_empty());
        filter.board_ids = Some(vec![first, first]);
        let selected = loaded.database.list_all_task_page(&filter, 0, 200).unwrap();
        assert_eq!(selected.len(), 61);
        assert!(selected.iter().all(|row| row.board_id == first));

        filter.board_ids = Some(vec![second]);
        filter.column_id = Some(1);
        filter.due_start = Some(10);
        filter.due_end = Some(12);
        let rows = loaded.database.list_all_task_page(&filter, 0, 50).unwrap();
        assert_eq!(
            rows.iter().map(|row| row.task.due_time).collect::<Vec<_>>(),
            vec![Some(10), Some(11)]
        );
        assert!(rows.iter().all(|row| row.board_id == second));
        filter.due_start = None;
        filter.due_end = None;
        filter.status = "overdue".into();
        filter.now = 2;
        assert_eq!(
            loaded
                .database
                .list_all_task_page(&filter, 0, 50)
                .unwrap()
                .len(),
            1
        );
        filter.status = "recurring".into();
        assert_eq!(
            loaded.database.list_all_task_page(&filter, 0, 50).unwrap()[0]
                .task
                .id,
            60
        );
        filter.status = "active".into();
        filter.due = "none".into();
        assert_eq!(
            loaded.database.list_all_task_page(&filter, 0, 50).unwrap()[0]
                .task
                .id,
            61
        );
        filter.due = "all".into();
        filter.column_id = None;
        filter.status = "archived".into();
        filter.archive_start = Some(100);
        filter.archive_end = Some(101);
        assert_eq!(
            loaded
                .database
                .list_all_task_page(&filter, 0, 50)
                .unwrap()
                .len(),
            1
        );
        filter.archive_end = Some(100);
        assert!(loaded
            .database
            .list_all_task_page(&filter, 0, 50)
            .unwrap()
            .is_empty());
        filter = TaskExplorerQuery {
            status: "active".into(),
            sort: "column".into(),
            ..Default::default()
        };
        assert_eq!(
            loaded.database.list_all_task_page(&filter, 0, 1).unwrap()[0]
                .column_name
                .as_deref(),
            Some("Alpha")
        );
        filter.sort = "title".into();
        assert_eq!(
            loaded.database.list_all_task_page(&filter, 0, 1).unwrap()[0]
                .task
                .title,
            "No date"
        );
        drop(loaded);
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn task_search_finds_title_description_labels_and_checklist_text() {
        let dir = test_dir("task-search");
        fs::create_dir_all(&dir).unwrap();
        let mut loaded = load(&dir).unwrap();
        let board_id = loaded.database.active_board_id();
        let data = StoredData {
            columns: vec![Column {
                id: 1,
                name: "Todo".into(),
                color: String::new(),
                sort_order: Default::default(),
                sort_rules: Vec::new(),
                done: false,
                tasks: vec![Task {
                    id: 2,
                    title: "Write release notes".into(),
                    description: "Prepare the public changelog".into(),
                    labels: vec!["launch".into()],
                    items: vec![TaskItem {
                        id: "item_1".into(),
                        text: "Add screenshots".into(),
                        completed: false,
                    }],
                    ..Task::default()
                }],
            }],
            ..StoredData::default()
        };
        loaded
            .database
            .replace_board_as_local_edit(board_id, &data)
            .unwrap();
        let all_tasks = loaded
            .database
            .list_all_task_page(
                &TaskExplorerQuery {
                    query: "release notes".into(),
                    ..Default::default()
                },
                0,
                10,
            )
            .unwrap();
        assert_eq!(all_tasks.len(), 1);
        assert_eq!(all_tasks[0].board_id, board_id);
        assert_eq!(all_tasks[0].column_name.as_deref(), Some("Todo"));
        assert_eq!(all_tasks[0].task.title, "Write release notes");

        for query in ["RELEASE NOTES", "public changelog", "launch", "screenshots"] {
            assert_eq!(
                loaded.database.search_tasks(board_id, query).unwrap(),
                vec![2]
            );
        }
        assert!(loaded
            .database
            .search_tasks(board_id, "not found")
            .unwrap()
            .is_empty());

        drop(loaded);
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn hybrid_search_ranking_filters_and_live_changes() {
        let dir = test_dir("hybrid-search");
        let mut loaded = load(&dir).unwrap();
        let first = loaded.database.active_board_id();
        let second = loaded.database.create_board("Second").unwrap().id;
        let task = |id, title: &str, description: &str| Task {
            id,
            title: title.into(),
            description: description.into(),
            due_time: Some(10),
            ..Task::default()
        };
        let mut data = StoredData {
            columns: vec![Column {
                id: 1,
                name: "Todo".into(),
                color: String::new(),
                sort_order: Default::default(),
                sort_rules: Vec::new(),
                done: false,
                tasks: vec![
                    task(1, "Alpha", ""),
                    task(2, "Alpha beta", ""),
                    task(3, "a long phrase", ""),
                    task(4, "Notes", "alpha in description"),
                    task(5, "臺灣工作報告", ""),
                    task(6, "🚀 工作報告", ""),
                    task(7, "臺灣 Release", ""),
                ],
            }],
            archives: vec![Archive {
                time: 100,
                task: task(8, "a long phrase", ""),
            }],
            ..StoredData::default()
        };
        loaded
            .database
            .replace_board_as_local_edit(first, &data)
            .unwrap();
        loaded
            .database
            .replace_board_as_local_edit(second, &data)
            .unwrap();
        let ids = loaded.database.search_tasks(first, "alpha").unwrap();
        assert_eq!(&ids[..2], &[1, 2]);
        assert_eq!(ids.len(), 5);
        for id in [3, 4, 8] {
            assert!(ids.contains(&id));
        }
        assert_eq!(ids, loaded.database.search_tasks(first, "alpha").unwrap());
        for (query, id) in [("臺報", 5), ("🚀報", 6), ("臺R", 7)] {
            assert_eq!(
                loaded.database.search_tasks(first, query).unwrap(),
                vec![id]
            );
        }
        assert!(loaded
            .database
            .search_tasks(first, "  ")
            .unwrap()
            .is_empty());
        assert!(loaded
            .database
            .search_tasks(first, "missing-xyz")
            .unwrap()
            .is_empty());
        let mut filter = TaskExplorerQuery {
            query: "alpha".into(),
            status: "active".into(),
            sort: "title".into(),
            search_options: Some(crate::search::SearchOptions {
                candidate_limit: 1,
                result_limit: 2,
                ..Default::default()
            }),
            ..Default::default()
        };
        let invalid_filter = TaskExplorerQuery {
            search_options: Some(crate::search::SearchOptions {
                candidate_limit: 0,
                ..Default::default()
            }),
            ..Default::default()
        };
        let error = loaded
            .database
            .list_all_task_page(&invalid_filter, 0, 10)
            .err()
            .unwrap();
        assert!(matches!(
            error.downcast_ref::<DomainError>(),
            Some(DomainError::InvalidArgument)
        ));
        // Board restrictions must precede Top K, even when another board has identical IDs.
        filter.board_ids = Some(vec![second]);
        let rows = loaded.database.list_all_task_page(&filter, 0, 10).unwrap();
        assert!(!rows.is_empty());
        assert!(rows
            .iter()
            .all(|row| row.board_id == second && row.archived_at.is_none()));
        filter.search_options = None;
        filter.board_ids = None;
        assert_eq!(
            loaded
                .database
                .list_all_task_page(&filter, 0, 10)
                .unwrap()
                .len(),
            8
        );
        filter.board_ids = Some(vec![first]);
        let all = loaded.database.list_all_task_page(&filter, 0, 10).unwrap();
        let page = loaded.database.list_all_task_page(&filter, 1, 2).unwrap();
        assert_eq!(
            page.iter().map(|row| row.task.id).collect::<Vec<_>>(),
            all[1..3].iter().map(|row| row.task.id).collect::<Vec<_>>()
        );
        let count = |status: &str, filter: &TaskExplorerQuery| {
            loaded
                .database
                .list_all_task_page(
                    &TaskExplorerQuery {
                        status: status.into(),
                        column_id: if status == "archived" {
                            None
                        } else {
                            filter.column_id
                        },
                        ..filter.clone()
                    },
                    0,
                    100,
                )
                .unwrap()
                .len() as i64
        };
        for query in ["alpha", "", "a"] {
            let filter = TaskExplorerQuery {
                query: query.into(),
                now: 11,
                ..filter.clone()
            };
            assert_eq!(
                loaded.database.count_all_tasks(&filter).unwrap(),
                TaskStatusCounts {
                    all: count("all", &filter),
                    active: count("active", &filter),
                    overdue: count("overdue", &filter),
                    recurring: count("recurring", &filter),
                    archived: count("archived", &filter),
                },
                "{query}"
            );
        }
        // Description hits carry an excerpt; title hits do not.
        let notes = all.iter().find(|row| row.task.id == 4).unwrap();
        assert_eq!(notes.excerpt.as_deref(), Some("alpha in description"));
        assert!(all
            .iter()
            .find(|row| row.task.id == 1)
            .unwrap()
            .excerpt
            .is_none());
        // Relevance keeps the fused order (exact title, then prefix) across pages.
        filter.sort = "relevance".into();
        let ranked = loaded.database.list_all_task_page(&filter, 0, 10).unwrap();
        assert_eq!(
            ranked
                .iter()
                .map(|row| row.task.id)
                .take(2)
                .collect::<Vec<_>>(),
            vec![1, 2]
        );
        assert_eq!(ranked.len(), all.len());
        let page = loaded.database.list_all_task_page(&filter, 1, 2).unwrap();
        assert_eq!(
            page.iter().map(|row| row.task.id).collect::<Vec<_>>(),
            ranked[1..3]
                .iter()
                .map(|row| row.task.id)
                .collect::<Vec<_>>()
        );
        filter.sort = "title".into();
        filter.status = "archived".into();
        assert_eq!(
            loaded.database.list_all_task_page(&filter, 0, 10).unwrap()[0]
                .task
                .id,
            8
        );
        filter.archive_start = Some(101);
        assert!(loaded
            .database
            .list_all_task_page(&filter, 0, 10)
            .unwrap()
            .is_empty());
        filter.archive_start = None;
        filter.status = "active".into();
        filter.column_id = Some(99);
        assert!(loaded
            .database
            .list_all_task_page(&filter, 0, 10)
            .unwrap()
            .is_empty());
        filter.column_id = Some(1);
        filter.due_start = Some(11);
        assert!(loaded
            .database
            .list_all_task_page(&filter, 0, 10)
            .unwrap()
            .is_empty());
        filter.due_start = None;
        filter.status = "overdue".into();
        filter.now = 11;
        assert_eq!(
            loaded
                .database
                .list_all_task_page(&filter, 0, 10)
                .unwrap()
                .len(),
            4
        );
        filter.due = "none".into();
        assert!(loaded
            .database
            .list_all_task_page(&filter, 0, 10)
            .unwrap()
            .is_empty());

        data.columns[0].tasks[0].title = "Delta".into();
        data.columns[0].tasks.retain(|task| task.id != 2);
        let archived = data.columns[0].tasks.remove(1);
        data.archives.push(Archive {
            time: 200,
            task: archived,
        });
        data.columns[0].tasks.push(task(9, "Alphabet", ""));
        loaded
            .database
            .replace_board_as_local_edit(first, &data)
            .unwrap();
        let reader = Database::open_search_reader(loaded.database.path().to_owned()).unwrap();
        let ids = reader.search_tasks(first, "alpha").unwrap();
        assert!(!ids.contains(&1) && !ids.contains(&2));
        assert!(ids.contains(&3) && ids.contains(&9));
        drop(reader);
        let restored = data.archives.pop().unwrap().task;
        data.columns[0].tasks.push(restored);
        loaded
            .database
            .replace_board_as_local_edit(first, &data)
            .unwrap();
        filter.due = "all".into();
        filter.status = "active".into();
        assert!(loaded
            .database
            .list_all_task_page(&filter, 0, 10)
            .unwrap()
            .iter()
            .any(|row| row.task.id == 3));
        loaded
            .database
            .move_tasks_to_board(first, &[9], second, 1)
            .unwrap();
        assert!(!loaded
            .database
            .search_tasks(first, "alphabet")
            .unwrap()
            .contains(&9));
        // Moving allocates a fresh ID on the destination board.
        let moved = loaded.database.search_tasks(second, "alphabet").unwrap();
        assert!(!moved.is_empty());
        assert_eq!(
            loaded
                .database
                .load_task(second, moved[0])
                .unwrap()
                .unwrap()
                .title,
            "Alphabet"
        );
        data.columns[0].tasks = vec![task(10, "Remote release notes", "")];
        data.archives.clear();
        loaded
            .database
            .apply_iroh_snapshot(first, "Shared", IrohPermission::Viewer, 20, &data, None)
            .unwrap();
        assert_eq!(
            loaded.database.search_tasks(first, "rrn").unwrap(),
            vec![10]
        );
        assert!(loaded
            .database
            .search_tasks(first, "alpha")
            .unwrap()
            .is_empty());
        drop(loaded);
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    #[ignore = "Synthetic search benchmark: run alone with --ignored --nocapture"]
    fn hybrid_search_benchmark() {
        struct SearchLogger;
        impl log::Log for SearchLogger {
            fn enabled(&self, metadata: &log::Metadata) -> bool {
                metadata.target() == "storage.search"
            }
            fn log(&self, record: &log::Record) {
                if self.enabled(record.metadata()) {
                    println!("{}", record.args());
                }
            }
            fn flush(&self) {}
        }
        static LOGGER: SearchLogger = SearchLogger;
        log::set_logger(&LOGGER).unwrap();
        log::set_max_level(log::LevelFilter::Debug);
        for size in [1_000, 10_000, 50_000, 100_000] {
            let dir = test_dir(&format!("hybrid-bench-{size}"));
            let mut loaded = load(&dir).unwrap();
            let board = loaded.database.active_board_id();
            let data = StoredData {
                columns: vec![Column {
                    id: 1,
                    name: "Todo".into(),
                    color: String::new(),
                    sort_order: Default::default(),
                    sort_rules: Vec::new(),
                    done: false,
                    tasks: vec![],
                }],
                ..Default::default()
            };
            loaded
                .database
                .replace_board_as_local_edit(board, &data)
                .unwrap();
            let started = std::time::Instant::now();
            let tx = loaded.database.connection.transaction().unwrap();
            {
                let mut insert = tx.prepare("INSERT INTO cardbe_tasks(board_id,id,column_id,position,payload) VALUES(?1,?2,1,?2,?3)").unwrap();
                for id in 1..=size {
                    let task = Task {
                        id,
                        title: format!("Release planning task {id} 臺灣 🚀"),
                        description: "Public changelog and release documentation".into(),
                        ..Default::default()
                    };
                    insert
                        .execute(params![board, id, serde_json::to_string(&task).unwrap()])
                        .unwrap();
                }
            }
            tx.commit().unwrap();
            println!(
                "tasks={size} fixture_and_fts_init_ms={} pid={}",
                started.elapsed().as_millis(),
                std::process::id()
            );
            for query in ["release", "rpt", "臺🚀", "no-match-xyz"] {
                println!("query={query}");
                for _ in 0..3 {
                    let reader =
                        Database::open_search_reader(loaded.database.path().to_owned()).unwrap();
                    let started = std::time::Instant::now();
                    let ids = reader.search_tasks(board, query).unwrap();
                    assert!(ids.len() <= crate::search::SearchOptions::default().result_limit);
                    println!("command_storage_us={}", started.elapsed().as_micros());
                }
            }
            #[cfg(windows)]
            {
                let memory = std::process::Command::new("powershell.exe")
                    .args([
                        "-NoProfile",
                        "-NonInteractive",
                        "-Command",
                        &format!("(Get-Process -Id {}).PeakWorkingSet64", std::process::id()),
                    ])
                    .output()
                    .unwrap();
                assert!(memory.status.success());
                println!(
                    "tasks={size} process_peak_working_set_bytes={}",
                    String::from_utf8(memory.stdout).unwrap().trim()
                );
            }
            drop(loaded);
            fs::remove_dir_all(dir).unwrap();
        }
    }

    #[test]
    fn permission_downgrade_after_probe_rejects_upload_and_preserves_local_conflict() {
        let owner_dir = test_dir("downgrade-owner");
        let editor_dir = test_dir("downgrade-editor");
        let mut owner = load(&owner_dir).unwrap();
        let mut editor = load(&editor_dir).unwrap();
        let owner_id = owner.database.active_board_id();
        let mut base = StoredData::default();
        base.columns.push(Column {
            id: 1,
            name: "Before".into(),
            color: String::new(),
            sort_order: Default::default(),
            sort_rules: Vec::new(),
            done: false,
            tasks: vec![],
        });
        owner
            .database
            .replace_board_as_local_edit(owner_id, &base)
            .unwrap();
        owner
            .database
            .save_iroh_invite("invite", owner_id, "secret", IrohPermission::Editor)
            .unwrap();
        owner
            .database
            .iroh_device_access("invite", "secret", "device", true)
            .unwrap();
        owner
            .database
            .set_iroh_device_approved("invite", "device", true)
            .unwrap();
        let original = owner.database.iroh_loro_update(owner_id).unwrap().unwrap();
        let revision = owner.database.board_sync_state(owner_id).unwrap().0;
        let editor_id = editor
            .database
            .create_received_board(
                "Shared",
                &base,
                IrohPermission::Editor,
                revision,
                "ticket",
                Some(&original),
            )
            .unwrap()
            .id;
        let mut local = base.clone();
        local.columns[0].name = "Unsent local edit".into();
        editor
            .database
            .replace_board_as_local_edit(editor_id, &local)
            .unwrap();
        let upload = editor
            .database
            .iroh_loro_update(editor_id)
            .unwrap()
            .unwrap();
        // The editor has already received the owner's state vector when the
        // owner changes permission, before the upload transaction begins.
        let vector = crate::loro_board::state_vector(Some(&original)).unwrap();
        let diff = crate::loro_board::diff(Some(&upload), &vector).unwrap();
        owner
            .database
            .update_iroh_invite("invite", Some(IrohPermission::Viewer), None)
            .unwrap();
        assert!(owner
            .database
            .apply_iroh_loro_update(owner_id, "invite", "secret", "device", &diff)
            .is_err());
        assert_eq!(
            owner
                .database
                .read_board_complete(owner_id)
                .unwrap()
                .columns,
            base.columns
        );
        assert_eq!(
            owner.database.iroh_loro_update(owner_id).unwrap().unwrap(),
            original
        );
        assert_eq!(
            owner.database.board_sync_state(owner_id).unwrap().0,
            revision
        );
        editor
            .database
            .save_iroh_conflict(
                editor_id,
                revision,
                "Shared",
                IrohPermission::Viewer,
                &base,
                None,
            )
            .unwrap();
        assert_eq!(
            editor
                .database
                .read_board_complete(editor_id)
                .unwrap()
                .columns,
            local.columns
        );
        assert_eq!(
            editor
                .database
                .iroh_loro_update(editor_id)
                .unwrap()
                .unwrap(),
            upload
        );
        assert_eq!(
            editor.database.board_sync_state(editor_id).unwrap().1,
            SyncStatus::Conflict
        );
        assert_eq!(
            editor.database.board_role(editor_id).unwrap(),
            BoardRole::Viewer
        );
        assert_eq!(
            editor
                .database
                .iroh_conflict(editor_id)
                .unwrap()
                .unwrap()
                .data
                .columns,
            base.columns
        );
        let copy = editor
            .database
            .use_iroh_conflict_remote(editor_id, "Shared", &local)
            .unwrap();
        assert_eq!(
            editor
                .database
                .read_board_complete(copy.id)
                .unwrap()
                .columns,
            local.columns
        );
        assert_eq!(
            editor
                .database
                .read_board_complete(editor_id)
                .unwrap()
                .columns,
            base.columns
        );
        drop(owner);
        drop(editor);
        fs::remove_dir_all(owner_dir).unwrap();
        fs::remove_dir_all(editor_dir).unwrap();
    }

    #[test]
    fn loro_push_is_atomic_idempotent_and_checks_current_permission() {
        let dir = test_dir("loro-capability");
        fs::create_dir_all(&dir).unwrap();
        let mut loaded = load(&dir).unwrap();
        let board_id = loaded.database.active_board_id();
        let mut base = StoredData::default();
        base.columns.push(Column {
            id: 1,
            name: "Todo".into(),
            color: String::new(),
            sort_order: Default::default(),
            sort_rules: Vec::new(),
            done: false,
            tasks: vec![Task {
                id: 2,
                title: "Before".into(),
                ..Task::default()
            }],
        });
        loaded
            .database
            .replace_board_as_local_edit(board_id, &base)
            .unwrap();
        loaded
            .database
            .save_iroh_invite("editor", board_id, "secret", IrohPermission::Editor)
            .unwrap();
        loaded
            .database
            .iroh_device_access("editor", "secret", "device", true)
            .unwrap();
        loaded
            .database
            .set_iroh_device_approved("editor", "device", true)
            .unwrap();
        let original = loaded.database.iroh_loro_update(board_id).unwrap().unwrap();
        let mut edited = base.clone();
        edited.columns[0].tasks[0].title = "After".into();
        let update = crate::loro_board::apply_local_delta(Some(&original), &base, &edited).unwrap();
        let (changed, revision, data) = loaded
            .database
            .apply_iroh_loro_update(board_id, "editor", "secret", "device", &update)
            .unwrap();
        assert!(changed);
        assert_eq!(data.columns[0].tasks[0].title, "After");
        assert!(
            !loaded
                .database
                .apply_iroh_loro_update(board_id, "editor", "secret", "device", &update)
                .unwrap()
                .0
        );
        assert!(
            !loaded
                .database
                .apply_iroh_loro_update(board_id, "editor", "secret", "device", &[])
                .unwrap()
                .0
        );
        assert_eq!(
            loaded.database.board_sync_state(board_id).unwrap().0,
            revision
        );
        loaded
            .database
            .update_iroh_invite("editor", None, Some(false))
            .unwrap();
        assert!(loaded
            .database
            .apply_iroh_loro_update(board_id, "editor", "secret", "device", &update)
            .is_err());
        assert_eq!(
            loaded.database.board_sync_state(board_id).unwrap().0,
            revision
        );
        loaded
            .database
            .update_iroh_invite("editor", None, Some(true))
            .unwrap();
        loaded
            .database
            .connection
            .execute("DELETE FROM cardbe_loro_docs WHERE board_id=?1", [board_id])
            .unwrap();
        assert!(loaded
            .database
            .apply_iroh_loro_update(board_id, "editor", "secret", "device", &[])
            .is_err());
        assert_eq!(
            loaded
                .database
                .read_board_complete(board_id)
                .unwrap()
                .columns[0]
                .tasks[0]
                .title,
            "After"
        );
        drop(loaded);
        fs::remove_dir_all(dir).unwrap();
    }

    fn columns(count: usize) -> StoredData {
        StoredData {
            columns: (1..=count as i64)
                .map(|id| Column {
                    id,
                    name: format!("Column {id}"),
                    color: String::new(),
                    sort_order: Default::default(),
                    sort_rules: Vec::new(),
                    done: false,
                    tasks: Vec::new(),
                })
                .collect(),
            ..StoredData::default()
        }
    }

    fn is_share_limit(error: Box<dyn std::error::Error>) -> bool {
        matches!(
            DomainError::repository(error),
            DomainError::ShareLimitExceeded
        )
    }

    #[test]
    fn remote_size_bounds_reject_growth_but_keep_oversized_boards_syncing() {
        let at_limit = columns(REMOTE_MAX_COLUMNS);
        let over_limit = columns(REMOTE_MAX_COLUMNS + 1);
        assert!(validate_remote_board_size(None, &at_limit).is_ok());
        assert!(is_share_limit(
            validate_remote_board_size(None, &over_limit).unwrap_err()
        ));
        assert!(is_share_limit(
            validate_remote_board_size(Some(&at_limit), &over_limit).unwrap_err()
        ));
        // A board that grew locally past a bound may still receive changes
        // that do not enlarge it further.
        assert!(validate_remote_board_size(Some(&over_limit), &over_limit).is_ok());
        assert!(validate_remote_board_size(Some(&over_limit), &at_limit).is_ok());
        assert!(is_share_limit(
            validate_remote_board_size(Some(&over_limit), &columns(REMOTE_MAX_COLUMNS + 2))
                .unwrap_err()
        ));

        let archived = StoredData {
            archives: (0..=REMOTE_MAX_TASKS as i64)
                .map(|id| Archive {
                    time: 0,
                    task: Task {
                        id,
                        ..Task::default()
                    },
                })
                .collect(),
            ..StoredData::default()
        };
        assert!(is_share_limit(
            validate_remote_board_size(None, &archived).unwrap_err()
        ));

        let large = StoredData {
            columns: vec![Column {
                id: 1,
                name: "x".repeat(REMOTE_MAX_CONTENT_BYTES),
                color: String::new(),
                sort_order: Default::default(),
                sort_rules: Vec::new(),
                done: false,
                tasks: Vec::new(),
            }],
            ..StoredData::default()
        };
        assert!(is_share_limit(
            validate_remote_board_size(None, &large).unwrap_err()
        ));
    }

    #[test]
    fn prepared_editor_push_is_not_committed_over_a_newer_document() {
        let dir = test_dir("loro-stale-prepare");
        fs::create_dir_all(&dir).unwrap();
        let mut loaded = load(&dir).unwrap();
        let board_id = loaded.database.active_board_id();
        let base = columns(2);
        loaded
            .database
            .replace_board_as_local_edit(board_id, &base)
            .unwrap();
        loaded
            .database
            .save_iroh_invite("editor", board_id, "secret", IrohPermission::Editor)
            .unwrap();
        loaded
            .database
            .iroh_device_access("editor", "secret", "device", true)
            .unwrap();
        loaded
            .database
            .set_iroh_device_approved("editor", "device", true)
            .unwrap();
        let original = loaded.database.iroh_loro_update(board_id).unwrap().unwrap();
        let revision = loaded.database.board_sync_state(board_id).unwrap().0;
        let mut first = base.clone();
        first.columns[0].name = "First".into();
        let first = crate::loro_board::apply_local_delta(Some(&original), &base, &first).unwrap();
        let mut second = base.clone();
        second.columns[1].name = "Second".into();
        let second = crate::loro_board::apply_local_delta(Some(&original), &base, &second).unwrap();

        let stale = loaded
            .database
            .prepare_iroh_loro_update(board_id, "editor", "secret", "device", &first)
            .unwrap();
        loaded
            .database
            .apply_iroh_loro_update(board_id, "editor", "secret", "device", &second)
            .unwrap();
        let after_second = loaded.database.iroh_loro_update(board_id).unwrap().unwrap();
        assert!(loaded
            .database
            .commit_iroh_loro_update(stale, "editor", "secret", "device")
            .unwrap()
            .is_none());
        assert_eq!(
            loaded.database.iroh_loro_update(board_id).unwrap().unwrap(),
            after_second
        );
        assert_eq!(
            loaded.database.board_sync_state(board_id).unwrap().0,
            revision + 1
        );

        let (changed, pushed, _) = loaded
            .database
            .apply_iroh_loro_update(board_id, "editor", "secret", "device", &first)
            .unwrap();
        assert!(changed);
        assert_eq!(pushed, revision + 2);
        let names = loaded
            .database
            .read_board_complete(board_id)
            .unwrap()
            .columns
            .into_iter()
            .map(|column| column.name)
            .collect::<Vec<_>>();
        assert_eq!(names, ["First", "Second"]);

        // Revocation between preparation and commit wins.
        let unchanged = loaded
            .database
            .prepare_iroh_loro_update(board_id, "editor", "secret", "device", &first)
            .unwrap();
        loaded
            .database
            .update_iroh_invite("editor", None, Some(false))
            .unwrap();
        assert!(matches!(
            DomainError::repository(
                loaded
                    .database
                    .commit_iroh_loro_update(unchanged, "editor", "secret", "device")
                    .unwrap_err()
            ),
            DomainError::PermissionDenied
        ));
        assert!(matches!(
            DomainError::repository(
                loaded
                    .database
                    .prepare_iroh_loro_update(board_id, "editor", "secret", "device", &first)
                    .err()
                    .unwrap()
            ),
            DomainError::PermissionDenied
        ));
        drop(loaded);
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn iroh_share_reads_check_access_and_see_one_committed_state() {
        let dir = test_dir("iroh-share-read");
        fs::create_dir_all(&dir).unwrap();
        let mut loaded = load(&dir).unwrap();
        let board_id = loaded.database.active_board_id();
        let base = columns(1);
        loaded
            .database
            .replace_board_as_local_edit(board_id, &base)
            .unwrap();
        loaded
            .database
            .save_iroh_invite("editor", board_id, "secret", IrohPermission::Editor)
            .unwrap();
        loaded
            .database
            .iroh_device_access("editor", "secret", "device", true)
            .unwrap();
        let read = |db: &Database, node_id: &str, board_id: i64| {
            db.read_iroh_share(board_id, "editor", "secret", node_id, |_, _| {
                IrohShareRead {
                    loro_document: true,
                    data: true,
                }
            })
            .unwrap()
        };
        assert_eq!(
            read(&loaded.database, "device", board_id).err(),
            Some(IrohShareDenied::DeviceNotApproved)
        );
        loaded
            .database
            .set_iroh_device_approved("editor", "device", true)
            .unwrap();
        assert_eq!(
            read(&loaded.database, "device", board_id + 1).err(),
            Some(IrohShareDenied::InvitationUnavailable)
        );
        let contents = read(&loaded.database, "device", board_id).unwrap();
        assert_eq!(contents.permission, IrohPermission::Editor);
        assert_eq!(contents.data.unwrap().columns, base.columns);
        assert!(contents.loro_document.is_some());
        let summary = loaded
            .database
            .read_iroh_share(board_id, "editor", "secret", "device", |_, _| {
                IrohShareRead {
                    loro_document: false,
                    data: false,
                }
            })
            .unwrap()
            .unwrap();
        assert!(summary.loro_document.is_none() && summary.data.is_none());

        // A write committed through another connection during the read is
        // not observed by the rest of that read.
        let mut writer = Database::open(dir.join("data.sqlite3")).unwrap();
        let (before, after) = loaded
            .database
            .read_consistent(|db| {
                let before = db.read_board_complete(board_id).unwrap().columns;
                writer
                    .replace_board_as_local_edit(board_id, &columns(2))
                    .unwrap();
                (before, db.read_board_complete(board_id).unwrap().columns)
            })
            .unwrap();
        assert_eq!(before, after);
        assert_eq!(
            loaded
                .database
                .read_board_complete(board_id)
                .unwrap()
                .columns,
            columns(2).columns
        );

        loaded
            .database
            .update_iroh_invite("editor", None, Some(false))
            .unwrap();
        assert_eq!(
            read(&loaded.database, "device", board_id).err(),
            Some(IrohShareDenied::DeviceNotApproved)
        );
        drop(writer);
        drop(loaded);
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn oversized_editor_push_is_rejected_without_writing() {
        let dir = test_dir("loro-size-limit");
        fs::create_dir_all(&dir).unwrap();
        let mut loaded = load(&dir).unwrap();
        let board_id = loaded.database.active_board_id();
        let base = columns(1);
        loaded
            .database
            .replace_board_as_local_edit(board_id, &base)
            .unwrap();
        loaded
            .database
            .save_iroh_invite("editor", board_id, "secret", IrohPermission::Editor)
            .unwrap();
        loaded
            .database
            .iroh_device_access("editor", "secret", "device", true)
            .unwrap();
        loaded
            .database
            .set_iroh_device_approved("editor", "device", true)
            .unwrap();
        let original = loaded.database.iroh_loro_update(board_id).unwrap().unwrap();
        let revision = loaded.database.board_sync_state(board_id).unwrap().0;
        let update = crate::loro_board::apply_local_delta(
            Some(&original),
            &base,
            &columns(REMOTE_MAX_COLUMNS + 1),
        )
        .unwrap();
        assert!(is_share_limit(
            loaded
                .database
                .apply_iroh_loro_update(board_id, "editor", "secret", "device", &update)
                .unwrap_err()
        ));
        assert_eq!(
            loaded.database.board_sync_state(board_id).unwrap().0,
            revision
        );
        assert_eq!(
            loaded.database.iroh_loro_update(board_id).unwrap().unwrap(),
            original
        );
        assert_eq!(
            loaded
                .database
                .read_board_complete(board_id)
                .unwrap()
                .columns,
            base.columns
        );
        drop(loaded);
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn offline_owner_and_editor_edits_converge_after_exchange() {
        let owner_dir = test_dir("loro-owner");
        let editor_dir = test_dir("loro-editor");
        fs::create_dir_all(&owner_dir).unwrap();
        fs::create_dir_all(&editor_dir).unwrap();
        let mut owner = load(&owner_dir).unwrap();
        let mut editor = load(&editor_dir).unwrap();
        let owner_id = owner.database.active_board_id();
        let mut base = StoredData::default();
        base.columns.push(Column {
            id: 1,
            name: "Todo".into(),
            color: String::new(),
            sort_order: Default::default(),
            sort_rules: Vec::new(),
            done: false,
            tasks: vec![
                Task {
                    id: 2,
                    title: "A".into(),
                    ..Task::default()
                },
                Task {
                    id: 3,
                    title: "B".into(),
                    ..Task::default()
                },
            ],
        });
        owner
            .database
            .replace_board_as_local_edit(owner_id, &base)
            .unwrap();
        owner
            .database
            .save_iroh_invite("invite", owner_id, "secret", IrohPermission::Editor)
            .unwrap();
        owner
            .database
            .iroh_device_access("invite", "secret", "device", true)
            .unwrap();
        owner
            .database
            .set_iroh_device_approved("invite", "device", true)
            .unwrap();
        let initial = owner.database.iroh_loro_update(owner_id).unwrap().unwrap();
        let editor_id = editor
            .database
            .create_received_board(
                "Shared",
                &base,
                IrohPermission::Editor,
                1,
                "ticket",
                Some(&initial),
            )
            .unwrap()
            .id;
        let mut owner_edit = base.clone();
        owner_edit.columns[0].tasks[0].title = "Owner changed A".into();
        owner
            .database
            .replace_board_as_local_edit(owner_id, &owner_edit)
            .unwrap();
        let mut editor_edit = base.clone();
        editor_edit.columns[0].tasks[1].title = "Editor changed B".into();
        editor
            .database
            .replace_board_as_local_edit(editor_id, &editor_edit)
            .unwrap();
        let sent = editor
            .database
            .iroh_loro_update(editor_id)
            .unwrap()
            .unwrap();
        let vector = crate::loro_board::state_vector(Some(&sent)).unwrap();
        owner
            .database
            .apply_iroh_loro_update(owner_id, "invite", "secret", "device", &sent)
            .unwrap();
        let owner_doc = owner.database.iroh_loro_update(owner_id).unwrap().unwrap();
        let diff = crate::loro_board::diff(Some(&owner_doc), &vector).unwrap();
        let revision = owner.database.board_sync_state(owner_id).unwrap().0;
        let content_changed = editor
            .database
            .merge_iroh_loro_diff(
                editor_id,
                &diff,
                "Shared",
                IrohPermission::Editor,
                revision,
                &sent,
            )
            .unwrap();
        assert!(content_changed);
        let current = editor
            .database
            .iroh_loro_update(editor_id)
            .unwrap()
            .unwrap();
        let empty_diff = crate::loro_board::diff(
            Some(&owner_doc),
            &crate::loro_board::state_vector(Some(&current)).unwrap(),
        )
        .unwrap();
        editor
            .database
            .connection
            .execute_batch(
                "CREATE TEMP TRIGGER reject_empty_sync_rewrite BEFORE DELETE ON cardbe_tasks
             BEGIN SELECT RAISE(ABORT, 'empty sync rewrote tasks'); END;
             CREATE TEMP TRIGGER reject_empty_doc_rewrite BEFORE UPDATE ON cardbe_loro_docs
             BEGIN SELECT RAISE(ABORT, 'empty sync rewrote history'); END;",
            )
            .unwrap();
        assert!(!editor
            .database
            .merge_iroh_loro_diff(
                editor_id,
                &empty_diff,
                "Shared",
                IrohPermission::Editor,
                revision,
                &current
            )
            .unwrap());
        editor
            .database
            .connection
            .execute_batch(
                "DROP TRIGGER reject_empty_sync_rewrite; DROP TRIGGER reject_empty_doc_rewrite;",
            )
            .unwrap();
        let owner_data = owner.database.read_board_complete(owner_id).unwrap();
        let editor_data = editor.database.read_board_complete(editor_id).unwrap();
        assert_eq!(owner_data.columns, editor_data.columns);
        assert_eq!(owner_data.columns[0].tasks[0].title, "Owner changed A");
        assert_eq!(owner_data.columns[0].tasks[1].title, "Editor changed B");
        assert_eq!(
            editor.database.board_sync_state(editor_id).unwrap(),
            (revision, SyncStatus::Synced)
        );
        let mut during_sync = editor_data.clone();
        during_sync.columns[0].tasks[1].color = "green".into();
        editor
            .database
            .replace_board_as_local_edit(editor_id, &during_sync)
            .unwrap();
        assert!(!editor
            .database
            .merge_iroh_loro_diff(
                editor_id,
                &empty_diff,
                "Shared",
                IrohPermission::Editor,
                revision,
                &current
            )
            .unwrap());
        assert_eq!(
            editor.database.board_sync_state(editor_id).unwrap().1,
            SyncStatus::Pending
        );
        drop(editor);
        let reopened = load(&editor_dir).unwrap();
        assert_eq!(
            reopened
                .database
                .read_board_complete(editor_id)
                .unwrap()
                .columns,
            during_sync.columns
        );
        assert!(reopened
            .database
            .iroh_loro_update(editor_id)
            .unwrap()
            .is_some());
        drop(reopened);
        drop(owner);
        fs::remove_dir_all(editor_dir).unwrap();
        fs::remove_dir_all(owner_dir).unwrap();
    }

    #[test]
    fn received_loro_changes_are_logged_as_remote_events() {
        use crate::task_events::{EventKind::*, EventOrigin::*};
        let owner_dir = test_dir("loro-events-owner");
        let editor_dir = test_dir("loro-events-editor");
        fs::create_dir_all(&owner_dir).unwrap();
        fs::create_dir_all(&editor_dir).unwrap();
        let mut owner = load(&owner_dir).unwrap();
        let mut editor = load(&editor_dir).unwrap();
        let owner_id = owner.database.active_board_id();
        let column = |id, tasks| Column {
            id,
            name: String::new(),
            color: String::new(),
            sort_order: Default::default(),
            sort_rules: Vec::new(),
            done: false,
            tasks,
        };
        let task = |id| Task {
            id,
            ..Task::default()
        };
        let base = StoredData {
            columns: vec![column(1, vec![task(10), task(11)]), column(2, Vec::new())],
            next_column_id: 3,
            next_task_id: 12,
            ..StoredData::default()
        };
        owner
            .database
            .replace_board_as_local_edit(owner_id, &base)
            .unwrap();
        owner
            .database
            .save_iroh_invite("invite", owner_id, "secret", IrohPermission::Editor)
            .unwrap();
        owner
            .database
            .iroh_device_access("invite", "secret", "device", true)
            .unwrap();
        owner
            .database
            .set_iroh_device_approved("invite", "device", true)
            .unwrap();
        let initial = owner.database.iroh_loro_update(owner_id).unwrap().unwrap();
        let editor_id = editor
            .database
            .create_received_board(
                "Shared",
                &base,
                IrohPermission::Editor,
                1,
                "ticket",
                Some(&initial),
            )
            .unwrap()
            .id;
        assert!(editor
            .database
            .task_events(editor_id, None)
            .unwrap()
            .is_empty());

        // The editor moves 10; the owner receives it.
        let mut editor_edit = base.clone();
        let moved = editor_edit.columns[0].tasks.remove(0);
        editor_edit.columns[1].tasks.push(moved);
        editor
            .database
            .replace_board_as_local_edit(editor_id, &editor_edit)
            .unwrap();
        let sent = editor
            .database
            .iroh_loro_update(editor_id)
            .unwrap()
            .unwrap();
        owner
            .database
            .apply_iroh_loro_update(owner_id, "invite", "secret", "device", &sent)
            .unwrap();
        // The owner moves 11; the editor receives it.
        let mut owner_edit = owner.database.read_board_complete(owner_id).unwrap();
        let moved = owner_edit.columns[0].tasks.remove(0);
        owner_edit.columns[1].tasks.push(moved);
        owner
            .database
            .replace_board_as_local_edit(owner_id, &owner_edit)
            .unwrap();
        let owner_doc = owner.database.iroh_loro_update(owner_id).unwrap().unwrap();
        let vector = crate::loro_board::state_vector(Some(&sent)).unwrap();
        let diff = crate::loro_board::diff(Some(&owner_doc), &vector).unwrap();
        let revision = owner.database.board_sync_state(owner_id).unwrap().0;
        assert!(editor
            .database
            .merge_iroh_loro_diff(
                editor_id,
                &diff,
                "Shared",
                IrohPermission::Editor,
                revision,
                &sent,
            )
            .unwrap());

        let events = |database: &Database, board| {
            database
                .task_events(board, None)
                .unwrap()
                .iter()
                .map(|e| {
                    (
                        e.origin,
                        e.kind,
                        e.task_id,
                        e.from_column_id,
                        e.to_column_id,
                    )
                })
                .collect::<Vec<_>>()
        };
        let created = [
            (Local, Created, 10, None, Some(1)),
            (Local, Created, 11, None, Some(1)),
        ];
        assert_eq!(
            events(&owner.database, owner_id),
            [
                &created[..],
                &[
                    (Remote, Moved, 10, Some(1), Some(2)),
                    (Local, Moved, 11, Some(1), Some(2)),
                ],
            ]
            .concat()
        );
        assert_eq!(
            events(&editor.database, editor_id),
            vec![
                (Local, Moved, 10, Some(1), Some(2)),
                (Remote, Moved, 11, Some(1), Some(2)),
            ]
        );
        drop(editor);
        drop(owner);
        fs::remove_dir_all(editor_dir).unwrap();
        fs::remove_dir_all(owner_dir).unwrap();
    }

    #[test]
    fn shared_snapshot_replaces_deleted_rows_and_preserves_global_settings() {
        let dir = test_dir("shared-snapshot-replace");
        fs::create_dir_all(&dir).unwrap();
        let mut loaded = load(&dir).unwrap();
        let board_id = loaded.database.active_board_id();
        let mut first = StoredData::default();
        first.columns.push(Column {
            id: 1,
            name: "Old".into(),
            color: String::new(),
            sort_order: Default::default(),
            sort_rules: Vec::new(),
            done: false,
            tasks: vec![Task {
                id: 1,
                title: "Remove me".into(),
                ..Task::default()
            }],
        });
        first.next_column_id = 2;
        first.next_task_id = 2;
        loaded
            .database
            .apply_iroh_snapshot(board_id, "Shared", IrohPermission::Viewer, 1, &first, None)
            .unwrap();
        assert!(loaded
            .database
            .column_overviews()
            .unwrap()
            .contains(&ColumnOverview {
                board_id,
                id: 1,
                name: "Old".into(),
                done: false,
                task_count: 1,
            }));
        let empty = StoredData::default();
        loaded
            .database
            .apply_iroh_snapshot(board_id, "Shared", IrohPermission::Viewer, 2, &empty, None)
            .unwrap();
        assert!(loaded
            .database
            .read_board_complete(board_id)
            .unwrap()
            .columns
            .is_empty());
        assert_eq!(loaded.database.boards().unwrap()[0].task_count, 0);
        assert!(loaded
            .database
            .column_overviews()
            .unwrap()
            .iter()
            .all(|column| column.board_id != board_id));
        use crate::task_events::{EventKind, EventOrigin};
        assert_eq!(
            loaded
                .database
                .task_events(board_id, None)
                .unwrap()
                .iter()
                .map(|e| (
                    e.origin,
                    e.kind,
                    e.task_id,
                    e.from_column_id,
                    e.to_column_id
                ))
                .collect::<Vec<_>>(),
            vec![
                (EventOrigin::Remote, EventKind::Created, 1, None, Some(1)),
                (EventOrigin::Remote, EventKind::Deleted, 1, Some(1), None),
            ]
        );
        drop(loaded);
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn startup_normalizes_an_active_viewer_board_without_granting_edit_access() {
        let dir = test_dir("viewer-startup-normalization");
        fs::create_dir_all(&dir).unwrap();
        let mut loaded = load(&dir).unwrap();
        let owner_id = loaded.database.active_board_id();
        let owner = loaded.database.read_board_complete(owner_id).unwrap();
        let mut shared = StoredData::default();
        shared.columns.push(Column {
            id: 100,
            name: "Shared column".into(),
            color: String::new(),
            sort_order: Default::default(),
            sort_rules: Vec::new(),
            done: false,
            tasks: vec![Task {
                id: 500,
                title: "Shared task".into(),
                ..Task::default()
            }],
        });
        shared.archives.push(Archive {
            time: 123,
            task: Task {
                id: 501,
                title: "Archived task".into(),
                ..Task::default()
            },
        });
        shared.templates.push(TaskTemplate {
            id: 1000,
            name: "Shared template".into(),
            task: Task::default(),
        });
        shared.settings.iroh_network.relay_enabled = false;
        let received = loaded
            .database
            .create_received_board(
                "Read only",
                &shared,
                IrohPermission::Viewer,
                7,
                "invitation",
                None,
            )
            .unwrap();
        loaded.database.load_board(received.id).unwrap();
        drop(loaded);

        let mut expected = shared;
        expected.settings = owner.settings.clone();
        let (expected, changed) = expected.migrate().unwrap();
        assert!(changed);
        let mut reopened = load(&dir).unwrap();
        let mut visible = expected.clone();
        visible.archives.clear();
        assert!(!reopened.archives_loaded);
        assert_eq!(reopened.database.active_board_id(), received.id);
        assert_eq!(reopened.stored, visible);
        assert_eq!(
            reopened.database.read_board_complete(received.id).unwrap(),
            expected
        );
        assert_eq!(
            reopened.database.read_board_complete(owner_id).unwrap(),
            owner
        );
        assert_eq!(
            reopened.database.board_role(received.id).unwrap(),
            BoardRole::Viewer
        );
        assert_eq!(
            reopened.database.board_sync_state(received.id).unwrap(),
            (7, SyncStatus::Synced)
        );
        assert_eq!(
            reopened
                .database
                .iroh_remote(received.id)
                .unwrap()
                .as_deref(),
            Some("invitation")
        );
        let mut edited = expected.clone();
        edited.columns.clear();
        assert!(reopened.database.persist_diff(&expected, &edited).is_err());
        drop(reopened);
        let reopened = load(&dir).unwrap();
        assert_eq!(reopened.stored, visible);
        assert_eq!(
            reopened.database.read_board_complete(received.id).unwrap(),
            expected
        );
        drop(reopened);
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn local_edit_revision_is_atomic_and_viewer_cannot_write() {
        let dir = test_dir("shared-local-revision");
        fs::create_dir_all(&dir).unwrap();
        let mut loaded = load(&dir).unwrap();
        let board_id = loaded.database.active_board_id();
        let before = loaded.database.read_board_complete(board_id).unwrap();
        let mut after = before.clone();
        after.columns.push(Column {
            id: 1,
            name: "Added".into(),
            color: String::new(),
            sort_order: Default::default(),
            sort_rules: Vec::new(),
            done: false,
            tasks: vec![],
        });
        after.next_column_id = 2;
        loaded.database.persist_diff(&before, &after).unwrap();
        assert_eq!(loaded.database.board_sync_state(board_id).unwrap().0, 1);
        loaded
            .database
            .set_shared_board(board_id, BoardRole::Viewer, SyncStatus::Synced, 1)
            .unwrap();
        let mut blocked = after.clone();
        blocked.columns.clear();
        assert!(loaded.database.persist_diff(&after, &blocked).is_err());
        assert_eq!(loaded.database.board_sync_state(board_id).unwrap().0, 1);
        assert_eq!(
            loaded
                .database
                .read_board_complete(board_id)
                .unwrap()
                .columns
                .len(),
            1
        );
        drop(loaded);
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn conflict_keeps_local_board_and_using_remote_creates_a_copy() {
        let dir = test_dir("shared-conflict-copy");
        fs::create_dir_all(&dir).unwrap();
        let mut loaded = load(&dir).unwrap();
        let board_id = loaded.database.active_board_id();
        let mut local = StoredData::default();
        local.columns.push(Column {
            id: 1,
            name: "Local".into(),
            color: String::new(),
            sort_order: Default::default(),
            sort_rules: Vec::new(),
            done: false,
            tasks: vec![],
        });
        let mut remote = StoredData::default();
        remote.columns.push(Column {
            id: 2,
            name: "Owner".into(),
            color: String::new(),
            sort_order: Default::default(),
            sort_rules: Vec::new(),
            done: false,
            tasks: vec![],
        });
        let local_doc =
            crate::loro_board::apply_local_delta(None, &StoredData::default(), &local).unwrap();
        let remote_doc =
            crate::loro_board::apply_local_delta(Some(&local_doc), &local, &remote).unwrap();
        loaded
            .database
            .apply_iroh_snapshot(
                board_id,
                "Shared",
                IrohPermission::Editor,
                3,
                &local,
                Some(&local_doc),
            )
            .unwrap();
        loaded
            .database
            .save_iroh_conflict(
                board_id,
                4,
                "Shared",
                IrohPermission::Editor,
                &remote,
                Some(&remote_doc),
            )
            .unwrap();
        // Simulate a pre-migration conflict, then refresh it with the owner's document.
        loaded
            .database
            .connection
            .execute(
                "UPDATE cardbe_iroh_conflicts SET loro_update=NULL WHERE board_id=?1",
                [board_id],
            )
            .unwrap();
        assert!(loaded.database.keep_iroh_conflict_local(board_id).is_err());
        assert!(loaded
            .database
            .use_iroh_conflict_remote(board_id, "Shared", &local)
            .is_err());
        assert_eq!(
            loaded
                .database
                .read_board_complete(board_id)
                .unwrap()
                .columns,
            local.columns
        );
        loaded
            .database
            .save_iroh_conflict(
                board_id,
                4,
                "Shared",
                IrohPermission::Editor,
                &remote,
                Some(&remote_doc),
            )
            .unwrap();
        let mut newer_local = local.clone();
        newer_local.columns[0].name = "Local edited after conflict".into();
        loaded.database.persist_diff(&local, &newer_local).unwrap();
        assert_eq!(
            loaded.database.board_sync_state(board_id).unwrap().1,
            SyncStatus::Conflict
        );
        assert_eq!(
            loaded
                .database
                .read_board_complete(board_id)
                .unwrap()
                .columns[0]
                .name,
            "Local edited after conflict"
        );
        drop(loaded);
        let mut loaded = load(&dir).unwrap();
        let saved = loaded.database.iroh_conflict(board_id).unwrap().unwrap();
        assert_eq!(saved.loro_update.as_deref(), Some(remote_doc.as_slice()));
        let copy = loaded
            .database
            .use_iroh_conflict_remote(board_id, "Shared", &newer_local)
            .unwrap();
        assert_eq!(
            loaded
                .database
                .read_board_complete(copy.id)
                .unwrap()
                .columns[0]
                .name,
            "Local edited after conflict"
        );
        assert_eq!(
            loaded
                .database
                .read_board_complete(board_id)
                .unwrap()
                .columns[0]
                .name,
            "Owner"
        );
        assert!(loaded.database.iroh_conflict(board_id).unwrap().is_none());
        let restored = loaded.database.iroh_loro_update(board_id).unwrap().unwrap();
        assert!(crate::loro_board::same_state_vector(&restored, &remote_doc).unwrap());
        let vector = crate::loro_board::state_vector(Some(&restored)).unwrap();
        let diff = crate::loro_board::diff(Some(&remote_doc), &vector).unwrap();
        assert!(!loaded
            .database
            .merge_iroh_loro_diff(
                board_id,
                &diff,
                "Shared",
                IrohPermission::Editor,
                4,
                &restored
            )
            .unwrap());
        assert_eq!(
            loaded
                .database
                .read_board_complete(board_id)
                .unwrap()
                .columns,
            remote.columns
        );

        let mut next = remote.clone();
        next.columns[0].name = "Next edit".into();
        loaded
            .database
            .replace_board_as_local_edit(board_id, &next)
            .unwrap();
        let upload = loaded.database.iroh_loro_update(board_id).unwrap().unwrap();
        let owner = crate::loro_board::project(
            &crate::loro_board::merge(Some(&remote_doc), &upload).unwrap(),
        )
        .unwrap();
        assert_eq!(owner.columns, next.columns);

        loaded
            .database
            .save_iroh_conflict(board_id, 5, "Shared", IrohPermission::Viewer, &remote, None)
            .unwrap();
        loaded
            .database
            .use_iroh_conflict_remote(board_id, "Shared", &next)
            .unwrap();
        assert!(loaded
            .database
            .iroh_loro_update(board_id)
            .unwrap()
            .is_none());
        drop(loaded);
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn keeping_local_conflict_rebases_on_the_owner_document() {
        let dir = test_dir("shared-conflict-rebase");
        let mut loaded = load(&dir).unwrap();
        let board_id = loaded.database.active_board_id();
        let mut local = StoredData::default();
        local.columns.push(Column {
            id: 1,
            name: "Local".into(),
            color: String::new(),
            sort_order: Default::default(),
            sort_rules: Vec::new(),
            done: false,
            tasks: vec![],
        });
        let local_doc =
            crate::loro_board::apply_local_delta(None, &StoredData::default(), &local).unwrap();
        loaded
            .database
            .apply_iroh_snapshot(
                board_id,
                "Shared",
                IrohPermission::Editor,
                1,
                &local,
                Some(&local_doc),
            )
            .unwrap();
        let mut remote = local.clone();
        remote.columns[0].name = "Owner".into();
        remote.columns.push(Column {
            id: 2,
            name: "Owner-only".into(),
            color: String::new(),
            sort_order: Default::default(),
            sort_rules: Vec::new(),
            done: false,
            tasks: vec![],
        });
        let remote_doc =
            crate::loro_board::apply_local_delta(Some(&local_doc), &local, &remote).unwrap();
        loaded
            .database
            .save_iroh_conflict(
                board_id,
                2,
                "Shared",
                IrohPermission::Editor,
                &remote,
                Some(&remote_doc),
            )
            .unwrap();
        loaded.database.keep_iroh_conflict_local(board_id).unwrap();
        assert_eq!(
            loaded.database.board_sync_state(board_id).unwrap(),
            (2, SyncStatus::Pending)
        );
        assert!(loaded.database.iroh_conflict(board_id).unwrap().is_none());
        let upload = loaded.database.iroh_loro_update(board_id).unwrap().unwrap();
        let owner = crate::loro_board::project(
            &crate::loro_board::merge(Some(&remote_doc), &upload).unwrap(),
        )
        .unwrap();
        assert_eq!(owner.columns, local.columns);
        assert_eq!(
            loaded
                .database
                .read_board_complete(board_id)
                .unwrap()
                .columns,
            local.columns
        );
        drop(loaded);
        fs::remove_dir_all(dir).unwrap();
    }

    fn test_dir(name: &str) -> PathBuf {
        std::env::temp_dir().join(format!(
            "cardbe-{name}-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ))
    }

    #[test]
    fn automatic_sharing_port_persists_across_reopen() {
        let dir = test_dir("iroh-automatic-port");
        fs::create_dir_all(&dir).unwrap();
        let loaded = load(&dir).unwrap();
        let path = loaded.database.path().to_path_buf();
        assert_eq!(loaded.database.iroh_automatic_port().unwrap(), None);
        drop(loaded);
        let mut database = Database::open(path.clone()).unwrap();
        database.set_iroh_automatic_port(51_234).unwrap();
        database.set_iroh_automatic_port(51_235).unwrap();
        drop(database);
        let database = Database::open(path).unwrap();
        assert_eq!(database.iroh_automatic_port().unwrap(), Some(51_235));
        drop(database);
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn notification_check_time_persists_across_reopen() {
        let dir = test_dir("notifications-checked-until");
        fs::create_dir_all(&dir).unwrap();
        let loaded = load(&dir).unwrap();
        let path = loaded.database.path().to_path_buf();
        assert_eq!(loaded.database.notifications_checked_until().unwrap(), None);
        drop(loaded);
        let mut database = Database::open(path.clone()).unwrap();
        database.set_notifications_checked_until(100).unwrap();
        database.set_notifications_checked_until(200).unwrap();
        drop(database);
        let database = Database::open(path).unwrap();
        assert_eq!(database.notifications_checked_until().unwrap(), Some(200));
        drop(database);
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn due_tasks_are_found_by_due_time_without_archives() {
        let dir = test_dir("due-tasks");
        fs::create_dir_all(&dir).unwrap();
        let mut loaded = load(&dir).unwrap();
        let task = |id, due_time| Task {
            id,
            title: format!("Task {id}"),
            due_time,
            ..Task::default()
        };
        let mut data = StoredData::default();
        data.columns.push(Column {
            id: 5,
            name: "Todo".into(),
            color: String::new(),
            sort_order: Default::default(),
            sort_rules: Vec::new(),
            done: false,
            tasks: vec![
                task(1, Some(100)),
                task(2, Some(200)),
                task(3, None),
                task(4, Some(99)),
            ],
        });
        // Completed tasks are not reminded.
        data.columns.push(Column {
            id: 6,
            name: "Done".into(),
            color: String::new(),
            sort_order: Default::default(),
            sort_rules: Vec::new(),
            done: true,
            tasks: vec![task(7, Some(150))],
        });
        data.archives.push(crate::models::Archive {
            time: 1,
            task: task(6, Some(150)),
        });
        let board = loaded
            .database
            .create_board_with_data("Work 工作", &data)
            .unwrap();

        let due = loaded.database.tasks_due_between(100, 200).unwrap();
        assert_eq!(
            due.iter()
                .map(|due| (
                    due.board_id,
                    due.board_name.as_str(),
                    due.column_id,
                    due.task.id
                ))
                .collect::<Vec<_>>(),
            vec![(board.id, "Work 工作", 5, 1)]
        );

        // Tasks received from other members of a shared board notify too.
        data.columns.truncate(1);
        data.columns[0].tasks = vec![task(9, Some(150))];
        data.archives.clear();
        loaded
            .database
            .apply_iroh_snapshot(board.id, "Shared", IrohPermission::Viewer, 2, &data, None)
            .unwrap();
        let due = loaded.database.tasks_due_between(100, 200).unwrap();
        assert_eq!(
            due.iter()
                .map(|due| (due.board_name.as_str(), due.task.id))
                .collect::<Vec<_>>(),
            vec![("Shared", 9)]
        );
        drop(loaded);
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn note_search_fuses_content_and_fuzzy_title_matches() {
        use crate::search::SearchOptions;
        let dir = test_dir("note-search");
        fs::create_dir_all(&dir).unwrap();
        let mut database = Database::open(dir.join("data.sqlite3")).unwrap();
        let mut note = |title: &str, content: &str| {
            database
                .create_note_with_content(1, title.into(), content.into())
                .unwrap()
                .id
        };
        let body = note("Weekly", "Release checklist for alpha");
        let fuzzy = note("Annual planning", "");
        let prefix = note("Release notes", "");
        let exact = note("release", "");
        let chinese = note("會議紀錄", "討論 🚀 上線時程");
        let search = |database: &Database, query: &str| {
            database
                .search_notes(query, SearchOptions::default())
                .unwrap()
        };

        assert_eq!(search(&database, "release"), vec![exact, prefix, body]);
        assert_eq!(search(&database, "anpl"), vec![fuzzy]);
        assert_eq!(search(&database, "會紀"), vec![chinese]);
        assert_eq!(search(&database, "上線時程"), vec![chinese]);
        assert_eq!(search(&database, "🚀"), vec![chinese]);
        assert_eq!(search(&database, "ch"), vec![body]);
        assert!(search(&database, "  ").is_empty());
        assert!(search(&database, "missing-xyz").is_empty());

        let mut edited = database
            .get_notes()
            .unwrap()
            .into_iter()
            .find(|note| note.id == body)
            .unwrap();
        edited.content = "Nothing here".into();
        database.update_note(&edited).unwrap();
        database.delete_note(exact).unwrap();
        assert_eq!(search(&database, "release"), vec![prefix]);
        assert!(search(&database, "checklist").is_empty());
        assert!(database
            .search_notes(
                "release",
                SearchOptions {
                    candidate_limit: 0,
                    ..SearchOptions::default()
                }
            )
            .is_err());

        drop(database);
        let reopened = Database::open(dir.join("data.sqlite3")).unwrap();
        assert_eq!(search(&reopened, "release"), vec![prefix]);
        drop(reopened);
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn notes_are_created_updated_sorted_and_deleted() {
        let dir = test_dir("notes");
        fs::create_dir_all(&dir).unwrap();
        let mut database = Database::open(dir.join("data.sqlite3")).unwrap();

        let older = database.create_note(100).unwrap();
        let newer = database
            .create_note_with_content(200, "Captured".into(), "From quick add".into())
            .unwrap();
        assert_eq!(newer.title, "Captured");
        assert_eq!(newer.content, "From quick add");
        let mut pinned = older.clone();
        pinned.title = "Pinned idea".into();
        pinned.content = "Keep this at the top".into();
        pinned.pinned = true;
        pinned.updated_at = 300;
        database.update_note(&pinned).unwrap();

        let notes = database.get_notes().unwrap();
        assert_eq!(notes, vec![pinned.clone(), newer.clone()]);

        database.delete_note(pinned.id).unwrap();
        assert_eq!(database.get_notes().unwrap(), vec![newer]);
        for error in [
            database.delete_note(pinned.id).unwrap_err(),
            database.update_note(&pinned).unwrap_err(),
        ] {
            assert_eq!(
                serde_json::to_value(crate::errors::CommandError::repository(error)).unwrap(),
                serde_json::json!({"code": "NOTE_NOT_FOUND"})
            );
        }
        database.initialize_boards(&StoredData::default()).unwrap();
        assert_eq!(
            serde_json::to_value(crate::errors::CommandError::repository(
                database
                    .update_iroh_invite("missing", None, Some(false))
                    .unwrap_err()
            ))
            .unwrap(),
            serde_json::json!({"code": "INVITE_NOT_FOUND"})
        );
        assert_eq!(
            serde_json::to_value(crate::errors::CommandError::repository(
                database
                    .set_iroh_device_approved("missing", "device", true)
                    .unwrap_err()
            ))
            .unwrap(),
            serde_json::json!({"code": "DEVICE_REQUEST_NOT_FOUND"})
        );
        drop(database);
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn migrates_json_and_preserves_the_source() {
        let dir = test_dir("sqlite-migration");
        fs::create_dir_all(&dir).unwrap();
        let mut stored = StoredData::default();
        stored.columns.push(Column {
            id: 0,
            name: "Todo".into(),
            color: String::new(),
            sort_order: ColumnSort::DueDateDesc,
            sort_rules: Vec::new(),
            done: false,
            tasks: vec![Task {
                id: 0,
                title: "Migrated".into(),
                ..Task::default()
            }],
        });
        stored.next_column_id = 1;
        stored.next_task_id = 1;
        write_document(&dir.join("data.json"), &stored).unwrap();
        // Data written before sort rules existed keeps its legacy sort.
        stored.columns[0].reconcile_sort();
        assert_eq!(stored.columns[0].sort_rules.len(), 1);

        let loaded = load(&dir).unwrap();
        assert_eq!(loaded.stored, stored);
        assert_eq!(loaded.database.boards().unwrap().len(), 1);
        assert_eq!(loaded.database.boards().unwrap()[0].name, "My board");
        assert!(dir.join("data.sqlite3").exists());
        assert!(dir.join("data.migrated.json").exists());
        drop(loaded);

        let reloaded = load(&dir).unwrap();
        assert_eq!(reloaded.stored, stored);
        drop(reloaded);
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn migrates_legacy_sqlite_before_persisting_schema_changes() {
        let dir = test_dir("sqlite-schema-migration");
        fs::create_dir_all(&dir).unwrap();
        let database_path = dir.join("data.sqlite3");
        let legacy = StoredData {
            schema_version: CURRENT_SCHEMA_VERSION - 1,
            columns: vec![Column {
                id: 0,
                name: "Legacy".into(),
                color: String::new(),
                sort_order: Default::default(),
                sort_rules: Vec::new(),
                done: false,
                tasks: Vec::new(),
            }],
            next_column_id: 1,
            ..Default::default()
        };

        let mut database = Database::open(database_path).unwrap();
        database.replace_all(&legacy).unwrap();
        drop(database);

        let loaded = load(&dir).unwrap();
        assert_ne!(loaded.database.active_board_id(), 0);
        assert_eq!(loaded.database.boards().unwrap().len(), 1);
        assert_eq!(loaded.stored.columns[0].name, "Legacy");
        drop(loaded);
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn incomplete_database_creation_retries_json_migration() {
        let dir = test_dir("interrupted-migration");
        fs::create_dir_all(&dir).unwrap();
        let mut stored = StoredData::default();
        stored.columns.push(Column {
            id: 0,
            name: "Still here".into(),
            color: String::new(),
            sort_order: Default::default(),
            sort_rules: Vec::new(),
            done: false,
            tasks: Vec::new(),
        });
        stored.next_column_id = 1;
        write_document(&dir.join("data.json"), &stored).unwrap();

        // Opening creates the SQLite file and schema, but intentionally does
        // not commit imported application data or the initialization marker.
        drop(Database::open(dir.join("data.sqlite3")).unwrap());

        let loaded = load(&dir).unwrap();
        assert_eq!(loaded.stored, stored);
        assert!(loaded.database.is_initialized().unwrap());
        assert!(dir.join("data.migrated.json").exists());
        drop(loaded);
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn failed_json_migration_rolls_back_and_can_be_retried() {
        let dir = test_dir("failed-migration-retry");
        fs::create_dir_all(&dir).unwrap();
        let mut stored = StoredData::default();
        stored.columns.push(Column {
            id: 0,
            name: "Must survive".into(),
            color: String::new(),
            sort_order: Default::default(),
            sort_rules: Vec::new(),
            done: false,
            tasks: Vec::new(),
        });
        stored.next_column_id = 1;
        write_document(&dir.join("data.json"), &stored).unwrap();

        let database_path = dir.join("data.sqlite3");
        let database = Database::open(database_path.clone()).unwrap();
        database
            .connection
            .execute_batch(
                "CREATE TRIGGER reject_migration
                 BEFORE INSERT ON columns
                 BEGIN
                   SELECT RAISE(ABORT, 'simulated migration failure');
                 END;",
            )
            .unwrap();
        drop(database);

        let error = match load(&dir) {
            Ok(_) => panic!("migration should fail"),
            Err(error) => error,
        };
        assert!(error
            .to_string()
            .contains("source files were left unchanged"));
        assert!(dir.join("data.json").exists());
        assert!(!dir.join("data.migrated.json").exists());
        let database = Database::open(database_path).unwrap();
        assert!(!database.is_initialized().unwrap());
        assert!(database.load().unwrap().columns.is_empty());
        database
            .connection
            .execute_batch("DROP TRIGGER reject_migration;")
            .unwrap();
        drop(database);

        let loaded = load(&dir).unwrap();
        assert_eq!(loaded.stored, stored);
        assert!(loaded.database.is_initialized().unwrap());
        assert!(dir.join("data.migrated.json").exists());
        drop(loaded);
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn updating_one_task_only_persists_that_task() {
        let dir = test_dir("partial-update");
        fs::create_dir_all(&dir).unwrap();
        let mut loaded = load(&dir).unwrap();
        let mut before = StoredData::default();
        before.columns.push(Column {
            id: 0,
            name: "Todo".into(),
            color: String::new(),
            sort_order: Default::default(),
            sort_rules: Vec::new(),
            done: false,
            tasks: vec![
                Task {
                    id: 0,
                    title: "First".into(),
                    ..Task::default()
                },
                Task {
                    id: 1,
                    title: "Second".into(),
                    ..Task::default()
                },
            ],
        });
        before.next_column_id = 1;
        before.next_task_id = 2;
        loaded.database.replace_all(&before).unwrap();
        let mut after = before.clone();
        after.columns[0].tasks[1].title = "Changed".into();

        let stats = loaded.database.persist_diff(&before, &after).unwrap();
        assert_eq!(stats.tasks_changed, 1);
        assert_eq!(stats.columns_changed, 0);
        assert_eq!(stats.metadata_changed, 0);
        assert_eq!(loaded.database.load().unwrap(), after);
        drop(loaded);
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn moving_a_task_only_updates_the_moved_row() {
        let dir = test_dir("sparse-task-move");
        fs::create_dir_all(&dir).unwrap();
        let mut loaded = load(&dir).unwrap();
        let mut before = StoredData::default();
        before.columns.push(Column {
            id: 0,
            name: "Todo".into(),
            color: String::new(),
            sort_order: Default::default(),
            sort_rules: Vec::new(),
            done: false,
            tasks: (0..5)
                .map(|id| Task {
                    id,
                    title: format!("Task {id}"),
                    ..Task::default()
                })
                .collect(),
        });
        before.next_column_id = 1;
        before.next_task_id = 5;
        loaded.database.replace_all(&before).unwrap();

        let mut after = before.clone();
        let moved = after.columns[0].tasks.remove(0);
        after.columns[0].tasks.push(moved);
        let stats = loaded.database.persist_diff(&before, &after).unwrap();

        assert_eq!(stats.tasks_changed, 1);
        assert_eq!(loaded.database.load().unwrap(), after);
        drop(loaded);
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn database_uses_normal_synchronous_mode() {
        let dir = test_dir("normal-synchronous");
        fs::create_dir_all(&dir).unwrap();
        let database = Database::open(dir.join("data.sqlite3")).unwrap();
        let synchronous: i64 = database
            .connection
            .query_row("PRAGMA synchronous", [], |row| row.get(0))
            .unwrap();
        assert_eq!(synchronous, 1);
        drop(database);
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn conflicting_text_boards_table_is_preserved_and_ignored() {
        let dir = test_dir("foreign-boards-table");
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("data.sqlite3");
        {
            let connection = Connection::open(&path).unwrap();
            connection
                .execute_batch(
                    "CREATE TABLE boards(id TEXT PRIMARY KEY, name TEXT NOT NULL);
                 INSERT INTO boards(id,name) VALUES ('foreign-id','Foreign board');",
                )
                .unwrap();
        }
        let loaded = load(&dir).unwrap();
        assert_eq!(loaded.database.boards().unwrap().len(), 1);
        let foreign: (String, String) = loaded
            .database
            .connection
            .query_row("SELECT id,name FROM boards", [], |r| {
                Ok((r.get(0)?, r.get(1)?))
            })
            .unwrap();
        assert_eq!(foreign, ("foreign-id".into(), "Foreign board".into()));
        drop(loaded);
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn archives_are_loaded_on_demand_after_initialization() {
        let dir = test_dir("lazy-archives");
        fs::create_dir_all(&dir).unwrap();
        let mut first = load(&dir).unwrap();
        let before = first.stored.clone();
        let mut after = before.clone();
        after.archives.push(Archive {
            time: 123,
            task: Task {
                id: 42,
                title: "Archived".into(),
                ..Task::default()
            },
        });
        after.next_task_id = 43;
        first.database.persist_diff(&before, &after).unwrap();
        drop(first);

        let mut second = load(&dir).unwrap();
        assert!(!second.archives_loaded);
        assert!(second.stored.archives.is_empty());
        assert_eq!(second.database.load_archives().unwrap(), after.archives);
        // Legacy databases can have archived cards but no CRDT document yet.
        second
            .database
            .connection
            .execute("DELETE FROM cardbe_loro_docs", [])
            .unwrap();
        let mut edited = second.stored.clone();
        edited.columns.push(Column {
            id: 1,
            name: "Edited".into(),
            color: String::new(),
            sort_order: Default::default(),
            sort_rules: Vec::new(),
            done: false,
            tasks: Vec::new(),
        });
        second
            .database
            .persist_diff_without_archives(&second.stored, &edited)
            .unwrap();
        let update = second
            .database
            .ensure_iroh_loro_doc(second.database.active_board_id())
            .unwrap();
        assert_eq!(
            crate::loro_board::project(&update).unwrap().archives,
            after.archives
        );
        assert_eq!(second.database.load_archives().unwrap(), after.archives);
        // Existing documents must not read archived payloads during ordinary edits.
        // Valid JSON with an invalid task shape makes accidental hydration fail.
        second
            .database
            .connection
            .execute("UPDATE cardbe_archives SET payload='null'", [])
            .unwrap();
        let mut edited_again = edited.clone();
        edited_again.columns[0].name = "Another edit".into();
        second
            .database
            .persist_diff_without_archives(&edited, &edited_again)
            .unwrap();
        let update = second
            .database
            .ensure_iroh_loro_doc(second.database.active_board_id())
            .unwrap();
        let projected = crate::loro_board::project(&update).unwrap();
        assert_eq!(projected.archives, after.archives);
        assert_eq!(projected.columns, edited_again.columns);
        drop(second);
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn failed_transaction_keeps_the_previous_complete_state() {
        let dir = test_dir("transaction-rollback");
        fs::create_dir_all(&dir).unwrap();
        let mut loaded = load(&dir).unwrap();
        let mut before = StoredData::default();
        before.columns.push(Column {
            id: 0,
            name: "Todo".into(),
            color: String::new(),
            sort_order: Default::default(),
            sort_rules: Vec::new(),
            done: false,
            tasks: vec![Task {
                id: 0,
                title: "Safe".into(),
                ..Task::default()
            }],
        });
        before.next_column_id = 1;
        before.next_task_id = 1;
        loaded.database.replace_all(&before).unwrap();
        loaded
            .database
            .connection
            .execute_batch(
                "CREATE TRIGGER reject_task_update
                 BEFORE UPDATE OF payload ON cardbe_tasks
                 BEGIN
                   SELECT RAISE(ABORT, 'simulated write failure');
                 END;",
            )
            .unwrap();

        let mut after = before.clone();
        after.columns[0].tasks[0].title = "Must roll back".into();
        assert!(loaded.database.persist_diff(&before, &after).is_err());
        assert_eq!(loaded.database.load().unwrap(), before);
        drop(loaded);
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn corrupt_json_recovers_from_backup_before_migration() {
        let dir = test_dir("backup-migration");
        fs::create_dir_all(&dir).unwrap();
        let mut stored = StoredData::default();
        stored.columns.push(Column {
            id: 0,
            name: "Recovered".into(),
            color: String::new(),
            sort_order: Default::default(),
            sort_rules: Vec::new(),
            done: false,
            tasks: Vec::new(),
        });
        write_document(&backup_path(&dir.join("data.json")), &stored).unwrap();
        fs::write(dir.join("data.json"), "not json").unwrap();

        let loaded = load(&dir).unwrap();
        assert_eq!(loaded.stored.columns[0].name, "Recovered");
        assert!(!loaded.recovery_messages.is_empty());
        drop(loaded);
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn corrupt_current_json_does_not_resurrect_stale_legacy_data() {
        let dir = test_dir("no-stale-legacy");
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("data.json"), "not json").unwrap();
        fs::write(
            dir.join("tasks.json"),
            r#"[{"name":"Stale","color":"","tasks":[]}]"#,
        )
        .unwrap();

        let loaded = load(&dir).unwrap();
        assert!(loaded.stored.columns.is_empty());
        drop(loaded);
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn boards_isolate_same_ids_and_restore_active_board() {
        let dir = test_dir("board-isolation");
        fs::create_dir_all(&dir).unwrap();
        let mut loaded = load(&dir).unwrap();
        let first_id = loaded.database.active_board_id();
        let second = loaded.database.create_board("Second").unwrap();

        let first = loaded.stored.clone();
        let mut first_changed = first.clone();
        first_changed.columns.push(Column {
            id: 0,
            name: "First".into(),
            color: String::new(),
            sort_order: Default::default(),
            sort_rules: Vec::new(),
            done: false,
            tasks: vec![Task {
                id: 0,
                title: "Only first".into(),
                ..Task::default()
            }],
        });
        first_changed.next_column_id = 1;
        first_changed.next_task_id = 1;
        loaded
            .database
            .persist_diff(&first, &first_changed)
            .unwrap();

        let second_before = loaded.database.load_board(second.id).unwrap();
        let mut second_changed = second_before.clone();
        second_changed.columns.push(Column {
            id: 0,
            name: "Second".into(),
            color: String::new(),
            sort_order: Default::default(),
            sort_rules: Vec::new(),
            done: false,
            tasks: vec![Task {
                id: 0,
                title: "Only second".into(),
                ..Task::default()
            }],
        });
        second_changed.next_column_id = 1;
        second_changed.archives.push(Archive {
            time: 456,
            task: Task {
                id: 1,
                title: "Archived second".into(),
                ..Task::default()
            },
        });
        second_changed.next_task_id = 2;
        loaded
            .database
            .persist_diff(&second_before, &second_changed)
            .unwrap();

        let summaries = loaded.database.boards().unwrap();
        assert_eq!(summaries.len(), 2);
        assert_eq!(summaries[0].task_count, 1);
        assert_eq!(summaries[1].task_count, 1);

        let first_again = loaded.database.load_board(first_id).unwrap();
        assert_eq!(first_again.columns[0].name, "First");
        assert_eq!(first_again.columns[0].tasks[0].title, "Only first");
        drop(loaded);

        let reopened = load(&dir).unwrap();
        assert_eq!(reopened.database.active_board_id(), first_id);
        assert_eq!(reopened.stored.columns[0].name, "First");
        drop(reopened);
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn cross_board_template_copy_preserves_source_and_syncs_independent_copy() {
        let dir = test_dir("cross-board-template-copy");
        fs::create_dir_all(&dir).unwrap();
        let mut loaded = load(&dir).unwrap();
        let mut data = StoredData::default();
        data.templates.push(TaskTemplate {
            id: 0,
            name: "Template".into(),
            task: Task {
                title: "Keep content 中文".into(),
                description: "Details".into(),
                labels: vec!["label".into()],
                ..Task::default()
            },
        });
        data.next_template_id = 1;
        let source = loaded
            .database
            .create_board_with_data("Source", &data)
            .unwrap()
            .id;
        let target = loaded
            .database
            .create_board_with_data("Target", &data)
            .unwrap()
            .id;
        for (from, template, to, code) in [
            (source, 0, source, "INVALID_ARGUMENT"),
            (source, 99, target, "TEMPLATE_NOT_FOUND"),
            (source, 0, -1, "BOARD_NOT_FOUND"),
            (-1, 0, target, "BOARD_NOT_FOUND"),
        ] {
            let error = loaded
                .database
                .copy_task_template_to_board(from, template, to)
                .unwrap_err();
            assert_eq!(
                serde_json::to_value(crate::errors::CommandError::repository(error)).unwrap()
                    ["code"],
                code
            );
        }
        loaded
            .database
            .connection
            .execute(
                "UPDATE cardbe_boards SET shared_role='viewer' WHERE id=?1",
                [target],
            )
            .unwrap();
        let error = loaded
            .database
            .copy_task_template_to_board(source, 0, target)
            .unwrap_err();
        assert_eq!(
            serde_json::to_value(crate::errors::CommandError::repository(error)).unwrap()["code"],
            "PERMISSION_DENIED"
        );
        assert_eq!(
            loaded
                .database
                .read_board_complete(target)
                .unwrap()
                .templates,
            data.templates
        );
        loaded
            .database
            .connection
            .execute(
                "UPDATE cardbe_boards SET shared_role='editor' WHERE id=?1",
                [target],
            )
            .unwrap();
        // Reading a viewer board is allowed; only the destination is modified.
        loaded
            .database
            .connection
            .execute(
                "UPDATE cardbe_boards SET shared_role='viewer' WHERE id=?1",
                [source],
            )
            .unwrap();
        loaded
            .database
            .copy_task_template_to_board(source, 0, target)
            .unwrap();
        assert_eq!(
            loaded
                .database
                .read_board_complete(source)
                .unwrap()
                .templates,
            data.templates
        );
        let copied = loaded.database.read_board_complete(target).unwrap();
        let mut expected = data.templates[0].clone();
        expected.id = copied.templates[1].id;
        assert_ne!(expected.id, data.templates[0].id);
        assert_eq!(copied.templates, vec![data.templates[0].clone(), expected]);
        let update = loaded.database.iroh_loro_update(target).unwrap().unwrap();
        assert_eq!(
            crate::loro_board::project(&update).unwrap().templates,
            copied.templates
        );
        drop(loaded);
        let loaded = load(&dir).unwrap();
        assert_eq!(
            loaded
                .database
                .read_board_complete(target)
                .unwrap()
                .templates,
            copied.templates
        );
        drop(loaded);
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn cross_board_batch_move_keeps_board_order_or_moves_nothing() {
        let dir = test_dir("cross-board-batch-move");
        fs::create_dir_all(&dir).unwrap();
        let mut loaded = load(&dir).unwrap();
        let task = |id, title: &str| Task {
            id,
            title: title.into(),
            ..Task::default()
        };
        let column = |id, tasks| Column {
            id,
            name: "Tasks".into(),
            color: String::new(),
            sort_order: Default::default(),
            sort_rules: Vec::new(),
            done: false,
            tasks,
        };
        let data = StoredData {
            columns: vec![
                column(
                    0,
                    vec![
                        Task {
                            blocked_by: vec![1],
                            ..task(0, "A")
                        },
                        task(1, "B"),
                    ],
                ),
                column(1, vec![task(2, "C")]),
            ],
            next_column_id: 2,
            next_task_id: 3,
            ..StoredData::default()
        };
        let source = loaded
            .database
            .create_board_with_data("Source", &data)
            .unwrap()
            .id;
        let target = loaded
            .database
            .create_board_with_data("Target", &data)
            .unwrap()
            .id;
        assert!(loaded
            .database
            .move_tasks_to_board(source, &[0, 99], target, 1)
            .is_err());
        assert_eq!(
            loaded.database.read_board_complete(source).unwrap().columns,
            data.columns
        );

        let remaining = loaded
            .database
            .move_tasks_to_board(source, &[2, 0], target, 1)
            .unwrap();
        let titles = |columns: &[Column]| -> Vec<Vec<String>> {
            columns
                .iter()
                .map(|column| column.tasks.iter().map(|t| t.title.clone()).collect())
                .collect()
        };
        assert_eq!(titles(&remaining.columns), vec![vec!["B"], vec![]]);
        let moved = loaded.database.read_board_complete(target).unwrap();
        assert_eq!(
            titles(&moved.columns),
            vec![vec!["A", "B"], vec!["C", "A", "C"]]
        );
        // Waiting on a task of the source board means nothing on the target.
        assert!(moved.columns[1].tasks[1].blocked_by.is_empty());
        let new_ids = [moved.columns[1].tasks[1].id, moved.columns[1].tasks[2].id];
        assert!(new_ids[0] != new_ids[1] && new_ids.iter().all(|id| *id > 2));
        use crate::task_events::EventKind::{TransferredIn, TransferredOut};
        let events = |database: &Database, board| {
            database
                .task_events(board, None)
                .unwrap()
                .iter()
                .map(|e| (e.kind, e.task_id, e.from_column_id, e.to_column_id))
                .collect::<Vec<_>>()
        };
        assert_eq!(
            events(&loaded.database, source),
            vec![
                (TransferredOut, 0, Some(0), None),
                (TransferredOut, 2, Some(1), None),
            ]
        );
        assert_eq!(
            events(&loaded.database, target),
            vec![
                (TransferredIn, new_ids[0], None, Some(1)),
                (TransferredIn, new_ids[1], None, Some(1)),
            ]
        );
        // The log belongs to its board and is removed with it.
        loaded.database.delete_board(source).unwrap();
        let remaining_ops: i64 = loaded
            .database
            .connection
            .query_row(
                "SELECT COUNT(*) FROM cardbe_task_event_ops WHERE board_id=?1",
                [source],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(remaining_ops, 0);
        assert_eq!(events(&loaded.database, target).len(), 2);
        drop(loaded);
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn cross_board_move_preserves_task_and_rejects_read_only_boards() {
        let dir = test_dir("cross-board-move");
        fs::create_dir_all(&dir).unwrap();
        let mut loaded = load(&dir).unwrap();
        let mut data = StoredData::default();
        let task = Task {
            id: 0,
            title: "Move me".into(),
            description: "Keep content".into(),
            labels: vec!["label".into()],
            ..Task::default()
        };
        data.columns.push(Column {
            id: 0,
            name: "Tasks".into(),
            color: String::new(),
            sort_order: Default::default(),
            sort_rules: Vec::new(),
            done: false,
            tasks: vec![task.clone()],
        });
        data.next_column_id = 1;
        data.next_task_id = 1;
        let source = loaded
            .database
            .create_board_with_data("Source", &data)
            .unwrap()
            .id;
        let target = loaded
            .database
            .create_board_with_data("Target", &data)
            .unwrap()
            .id;
        assert!(loaded
            .database
            .move_tasks_to_board(source, &[0], target, 99)
            .is_err());
        for id in [target, source] {
            loaded
                .database
                .connection
                .execute(
                    "UPDATE cardbe_boards SET shared_role='viewer' WHERE id=?1",
                    [id],
                )
                .unwrap();
            assert!(loaded
                .database
                .move_tasks_to_board(source, &[0], target, 0)
                .is_err());
            assert_eq!(
                loaded.database.read_board_complete(source).unwrap().columns[0].tasks,
                vec![task.clone()]
            );
            assert_eq!(
                loaded.database.read_board_complete(target).unwrap().columns[0].tasks,
                vec![task.clone()]
            );
            loaded
                .database
                .connection
                .execute(
                    "UPDATE cardbe_boards SET shared_role='owner' WHERE id=?1",
                    [id],
                )
                .unwrap();
        }
        loaded
            .database
            .move_tasks_to_board(source, &[0], target, 0)
            .unwrap();
        assert!(
            loaded.database.read_board_complete(source).unwrap().columns[0]
                .tasks
                .is_empty()
        );
        let moved = loaded.database.read_board_complete(target).unwrap();
        let mut expected = task.clone();
        expected.id = moved.columns[0].tasks[1].id;
        assert_ne!(expected.id, task.id);
        assert_eq!(moved.columns[0].tasks, vec![task, expected]);
        drop(loaded);
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn done_columns_and_completion_times_persist() {
        let dir = test_dir("done-column");
        fs::create_dir_all(&dir).unwrap();
        let mut loaded = load(&dir).unwrap();
        let board_id = loaded.database.active_board_id();
        let mut data = StoredData::default();
        data.columns.push(Column {
            id: 1,
            name: "Done".into(),
            color: String::new(),
            sort_order: Default::default(),
            sort_rules: Vec::new(),
            done: true,
            tasks: vec![Task {
                id: 2,
                completed_at: Some(1_800_000_000_000),
                ..Task::default()
            }],
        });
        loaded
            .database
            .replace_board_as_local_edit(board_id, &data)
            .unwrap();
        let read = loaded.database.read_board_complete(board_id).unwrap();
        assert_eq!(read.columns, data.columns);

        data.columns[0].done = false;
        data.columns[0].tasks[0].completed_at = None;
        loaded
            .database
            .replace_board_as_local_edit(board_id, &data)
            .unwrap();
        let read = loaded.database.read_board_complete(board_id).unwrap();
        assert_eq!(read.columns, data.columns);
        drop(loaded);
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn replace_all_boards_round_trips_same_logical_ids_and_global_data() {
        let dir = test_dir("all-board-round-trip");
        fs::create_dir_all(&dir).unwrap();
        let mut loaded = load(&dir).unwrap();

        let mut first = StoredData::default();
        first.columns.push(Column {
            id: 0,
            name: "First column".into(),
            color: String::new(),
            sort_order: Default::default(),
            sort_rules: Vec::new(),
            done: false,
            tasks: vec![Task {
                id: 0,
                title: "First task".into(),
                pinned: true,
                ..Task::default()
            }],
        });
        first.next_column_id = 1;
        first.next_task_id = 1;
        let mut second = first.clone();
        second.columns[0].name = "Second column".into();
        second.columns[0].tasks[0].title = "Second task".into();
        let settings = crate::models::Settings {
            theme: Some("morandi-dark-sage".into()),
            iroh_network: crate::models::IrohNetworkSettings {
                enabled: true,
                ..Default::default()
            },
            language: crate::models::LanguagePreference::TraditionalChinese,
            notify_enabled: true,
            global_shortcuts_enabled: false,
            ..Default::default()
        };
        let notes = vec![Note {
            id: 88,
            title: "Global note".into(),
            content: "Preserved across boards".into(),
            pinned: true,
            created_at: 1,
            updated_at: 2,
        }];

        let (boards, active) = loaded
            .database
            .replace_all_boards(
                &[
                    ("First".into(), first, Default::default()),
                    ("Second".into(), second, Default::default()),
                ],
                1,
                &notes,
                &settings,
                Some(&[crate::focuses::Focus {
                    id: "second".into(),
                    name: "Second board".into(),
                    filter: crate::focuses::FocusFilter {
                        board_ids: Some(vec![1]),
                        ..Default::default()
                    },
                    sort: Default::default(),
                    group: Default::default(),
                }]),
            )
            .unwrap();

        assert_eq!(boards.len(), 2);
        // Backup positions become the restored boards' IDs.
        let views = crate::focuses::load(&loaded.database.connection)
            .unwrap()
            .unwrap();
        assert_eq!(views[0].filter.board_ids, Some(vec![boards[1].id]));
        assert_eq!(active.columns[0].tasks[0].title, "Second task");
        assert_eq!(loaded.database.active_board_id(), boards[1].id);
        assert_eq!(loaded.database.get_notes().unwrap(), notes);
        let restored_first = loaded.database.read_board(boards[0].id).unwrap();
        let restored_second = loaded.database.read_board(boards[1].id).unwrap();
        assert_eq!(restored_first.columns[0].id, restored_second.columns[0].id);
        assert_eq!(
            restored_first.columns[0].tasks[0].id,
            restored_second.columns[0].tasks[0].id
        );
        assert_eq!(restored_first.columns[0].tasks[0].title, "First task");
        assert!(restored_first.columns[0].tasks[0].pinned);
        assert_eq!(restored_second.columns[0].tasks[0].title, "Second task");
        assert_eq!(restored_first.settings, settings);

        drop(loaded);
        let reopened = load(&dir).unwrap();
        assert_eq!(reopened.database.active_board_id(), boards[1].id);
        assert_eq!(reopened.stored.columns[0].tasks[0].title, "Second task");
        assert!(reopened.stored.columns[0].tasks[0].pinned);
        assert_eq!(reopened.database.get_notes().unwrap(), notes);
        assert_eq!(reopened.stored.settings, settings);
        drop(reopened);
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn replace_all_boards_rolls_back_everything_when_notes_violate_a_constraint() {
        let dir = test_dir("all-board-rollback");
        fs::create_dir_all(&dir).unwrap();
        let mut loaded = load(&dir).unwrap();
        let original_id = loaded.database.active_board_id();
        let original = loaded.database.read_board(original_id).unwrap();
        let original_note = loaded
            .database
            .create_note_with_content(1, "Original".into(), "Keep me".into())
            .unwrap();
        let invalid_notes = vec![original_note.clone(), original_note.clone()];

        let result = loaded.database.replace_all_boards(
            &[(
                "Replacement".into(),
                StoredData::default(),
                Default::default(),
            )],
            0,
            &invalid_notes,
            &crate::models::Settings {
                language: crate::models::LanguagePreference::English,
                notify_enabled: true,
                global_shortcuts_enabled: false,
                ..Default::default()
            },
            None,
        );

        assert!(result.is_err());
        assert_eq!(loaded.database.active_board_id(), original_id);
        assert_eq!(loaded.database.boards().unwrap().len(), 1);
        assert_eq!(loaded.database.read_board(original_id).unwrap(), original);
        assert_eq!(loaded.database.get_notes().unwrap(), vec![original_note]);
        drop(loaded);
        fs::remove_dir_all(dir).unwrap();
    }

    fn single_board_fixture(
        note_id: i64,
        missing_note_id: i64,
    ) -> (StoredData, crate::flow::FlowData) {
        let mut data: StoredData = serde_json::from_value(serde_json::json!({
            "columns": [{"id": 1, "name": "工作", "color": "", "tasks": [
                {"id": 1, "title": "第一張卡"}, {"id": 2, "title": "第二張卡"}
            ]}], "archives": [{"time": 123, "task": {"id": 3, "title": "已封存"}}]
        }))
        .unwrap();
        data.schema_version = CURRENT_SCHEMA_VERSION;
        data.repair_column_ids();
        data.repair_task_ids();
        let flow: crate::flow::FlowData = serde_json::from_value(serde_json::json!({
            "tab_id": "first",
            "layout": {"nodes": [{"task_id": 1, "x": 12, "y": 24, "scale": 1.5}], "viewport": {"x": 3, "y": 4, "zoom": 1.25},
                "annotations": [{"id": "note-first", "x": 20, "y": 30, "width": 240, "kind": "note", "note_id": note_id}],
                "connections": {"1:2": {"source_handle": "right", "target_handle": "left"}}},
            "relationships": [{"source": 1, "target": 2, "source_arrow": true, "target_arrow": false}, {"source": 2, "target": 3, "target_arrow": true}],
            "tabs": [
                {"id": "first", "name": "規劃"},
                {"id": "second", "name": "草稿", "layout": {"nodes": [{"task_id": 2, "x": -5, "y": 8}, {"task_id": 3, "x": 30, "y": 40}], "viewport": null,
                    "annotations": [
                        {"id": "note-second", "x": 0, "y": 0, "width": 240, "kind": "note", "note_id": note_id},
                        {"id": "missing", "x": 0, "y": 0, "width": 240, "kind": "note", "note_id": missing_note_id},
                        {"id": "text", "x": 1, "y": 2, "width": 300, "scale": 1.5, "kind": "text", "text": "中文備註"}
                    ]}}
            ]
        })).unwrap();
        (data, flow)
    }

    #[test]
    fn single_board_backup_round_trips_all_tabs_and_remaps_only_referenced_notes() {
        let dir = test_dir("single-board-flow-round-trip");
        fs::create_dir_all(&dir).unwrap();
        let mut loaded = load(&dir).unwrap();
        let note = loaded
            .database
            .create_note_with_content(123, "說明".into(), "完整內容".into())
            .unwrap();
        let unrelated = loaded
            .database
            .create_note_with_content(456, "私人便箋".into(), "不應匯出".into())
            .unwrap();
        let (data, flow) = single_board_fixture(note.id, unrelated.id);
        let source = loaded
            .database
            .create_board_with_data("Source", &data)
            .unwrap();
        let tx = loaded.database.connection.transaction().unwrap();
        crate::flow::restore(&tx, source.id, &flow).unwrap();
        tx.commit().unwrap();
        loaded.database.delete_note(unrelated.id).unwrap();
        let private_note = loaded
            .database
            .create_note_with_content(789, "Unrelated".into(), "Private".into())
            .unwrap();
        let backup = loaded.database.read_single_board_backup(source.id).unwrap();
        assert_eq!(backup.notes, vec![note.clone()]);
        let flow = backup.flow.unwrap();
        assert!(flow.tabs.iter().all(|tab| tab.layout.is_some()));
        let imported = loaded
            .database
            .create_board_with_backup("Imported", &data, Some(&flow), &backup.notes)
            .unwrap();
        let actual = loaded.database.read_flow_backup(imported.id).unwrap();
        let imported_notes = loaded.database.get_notes().unwrap();
        assert_eq!(imported_notes.len(), 3);
        assert!(imported_notes.contains(&private_note));
        let copied = imported_notes
            .iter()
            .find(|item| item.id != note.id && item.title == note.title)
            .unwrap();
        assert_eq!(copied.content, note.content);
        assert_eq!(copied.created_at, note.created_at);
        let missing = flow_note_ids(&actual)
            .into_iter()
            .find(|id| *id != copied.id)
            .unwrap();
        assert!(!imported_notes.iter().any(|item| item.id == missing));
        assert_ne!(missing, unrelated.id);
        let mut expected = flow.clone();
        for layout in std::iter::once(&mut expected.layout).chain(
            expected
                .tabs
                .iter_mut()
                .filter_map(|tab| tab.layout.as_mut()),
        ) {
            for annotation in &mut layout.annotations {
                if let crate::flow::AnnotationContent::Note { note_id } = &mut annotation.content {
                    *note_id = if *note_id == note.id {
                        copied.id
                    } else {
                        missing
                    };
                }
            }
        }
        assert_eq!(actual, expected);
        assert_eq!(loaded.database.read_flow_backup(source.id).unwrap(), flow);
        let future = loaded.database.create_note(999).unwrap();
        assert!(future.id > missing);
        drop(loaded);
        let reopened = load(&dir).unwrap();
        assert_eq!(
            reopened.database.read_flow_backup(imported.id).unwrap(),
            expected
        );
        assert_eq!(
            reopened
                .database
                .read_board_complete(imported.id)
                .unwrap()
                .columns,
            data.columns
        );
        drop(reopened);
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn single_board_import_failure_rolls_back_board_flow_notes_and_sync_state() {
        let dir = test_dir("single-board-flow-rollback");
        fs::create_dir_all(&dir).unwrap();
        let mut loaded = load(&dir).unwrap();
        let active = loaded.database.active_board_id();
        let before = loaded.database.read_board_complete(active).unwrap();
        let before_flow = loaded.database.read_flow_backup(active).unwrap();
        let before_sync = loaded.database.board_sync_state(active).unwrap();
        let note = Note {
            id: 5,
            title: "Imported".into(),
            content: "Content".into(),
            pinned: true,
            created_at: 1,
            updated_at: 2,
        };
        let (data, flow) = single_board_fixture(note.id, 6);
        // Fail after task and note writes, when restoring the second canvas.
        loaded.database.connection.execute_batch("CREATE TRIGGER fail_flow_import BEFORE INSERT ON cardbe_flow_layouts WHEN NEW.layout_id='second' BEGIN SELECT RAISE(ABORT,'test failure'); END;").unwrap();
        assert!(loaded
            .database
            .create_board_with_backup(
                "Must roll back",
                &data,
                Some(&flow),
                std::slice::from_ref(&note)
            )
            .is_err());
        assert_eq!(loaded.database.boards().unwrap().len(), 1);
        assert!(loaded.database.get_notes().unwrap().is_empty());
        assert!(loaded
            .database
            .import_board_data(&before, &data, Some(&flow), &[note])
            .is_err());
        assert_eq!(loaded.database.read_board_complete(active).unwrap(), before);
        assert_eq!(
            loaded.database.read_flow_backup(active).unwrap(),
            before_flow
        );
        assert_eq!(
            loaded.database.board_sync_state(active).unwrap(),
            before_sync
        );
        assert!(loaded.database.get_notes().unwrap().is_empty());
        loaded
            .database
            .connection
            .execute_batch("DROP TRIGGER fail_flow_import;")
            .unwrap();
        // A failed transaction must also leave the in-memory position cache usable.
        loaded
            .database
            .import_board_data(&before, &data, Some(&flow), &[])
            .unwrap();
        assert_eq!(
            loaded.database.read_board_complete(active).unwrap().columns,
            data.columns
        );
        loaded
            .database
            .set_shared_board(active, BoardRole::Viewer, SyncStatus::Synced, 1)
            .unwrap();
        let current = loaded.database.read_board_complete(active).unwrap();
        assert!(loaded
            .database
            .import_board_data(
                &current,
                &current,
                Some(&crate::flow::FlowData::default()),
                &[]
            )
            .is_err());
        drop(loaded);
        fs::remove_dir_all(dir).unwrap();
    }
}
