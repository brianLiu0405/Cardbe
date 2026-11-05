use crate::errors::{CommandError, DomainError, ShareError};
use crate::{
    models::{Archive, Board, BoardRole, Column, StoredData, TaskTemplate},
    storage::{self, Database, EventWrite},
    task_events::EventSource,
};
use std::{
    collections::{BTreeSet, HashSet, VecDeque},
    fmt,
    ops::{Deref, DerefMut},
    path::PathBuf,
    sync::{Arc, Mutex, MutexGuard},
};
use tauri::State;

pub struct AppData {
    pub stored: StoredData,
    pub boards: Vec<Board>,
    pub active_board_id: i64,
    pub undo_history: VecDeque<UndoEntry>,
    /// Contents undone since the last local change, most recent last.
    pub redo_history: Vec<UndoEntry>,
    pub labels: BTreeSet<String>,
    pub database: Database,
    pub recovery_messages: Vec<String>,
    pub archives_loaded: bool,
    /// Announces writes to `database` when the state lock is released.
    pub on_data_changed: Option<Arc<dyn Fn() + Send + Sync>>,
}

/// Board contents before a local change, with the task event operation the
/// change recorded so undoing it can name that operation.
///
/// Entries are undone newest first, and changes outside the history clear it,
/// so archives and templates the change left alone equal the current ones
/// when the entry is undone and are not kept. Settings are never undone.
#[derive(Clone, Debug, PartialEq)]
pub struct UndoEntry {
    pub columns: Vec<Column>,
    pub archives: Option<Vec<Archive>>,
    pub templates: Option<Vec<TaskTemplate>>,
    pub label_recency: Vec<String>,
    pub next_column_id: i64,
    pub next_task_id: i64,
    pub next_template_id: i64,
    pub event_op: Option<i64>,
}

impl UndoEntry {
    fn new(previous: StoredData, current: &StoredData, event_op: Option<i64>) -> Self {
        Self {
            columns: previous.columns,
            archives: (previous.archives != current.archives).then_some(previous.archives),
            templates: (previous.templates != current.templates).then_some(previous.templates),
            label_recency: previous.label_recency,
            next_column_id: previous.next_column_id,
            next_task_id: previous.next_task_id,
            next_template_id: previous.next_template_id,
            event_op,
        }
    }

    fn restore(self, current: &StoredData) -> StoredData {
        StoredData {
            schema_version: current.schema_version,
            columns: self.columns,
            archives: self.archives.unwrap_or_else(|| current.archives.clone()),
            templates: self.templates.unwrap_or_else(|| current.templates.clone()),
            settings: current.settings.clone(),
            label_recency: self.label_recency,
            next_column_id: self.next_column_id,
            next_task_id: self.next_task_id,
            next_template_id: self.next_template_id,
        }
    }
}

impl From<StoredData> for UndoEntry {
    fn from(data: StoredData) -> Self {
        Self {
            columns: data.columns,
            archives: Some(data.archives),
            templates: Some(data.templates),
            label_recency: data.label_recency,
            next_column_id: data.next_column_id,
            next_task_id: data.next_task_id,
            next_template_id: data.next_template_id,
            event_op: None,
        }
    }
}

pub type SharedAppData = Mutex<AppData>;

/// A panic while holding the application state left it possibly inconsistent.
#[derive(Debug)]
pub struct AppLockPoisoned;

impl fmt::Display for AppLockPoisoned {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Application state lock is poisoned")
    }
}

impl std::error::Error for AppLockPoisoned {}

impl From<AppLockPoisoned> for String {
    fn from(error: AppLockPoisoned) -> Self {
        error.to_string()
    }
}

impl From<AppLockPoisoned> for DomainError {
    fn from(error: AppLockPoisoned) -> Self {
        Self::Internal(error.to_string())
    }
}

impl From<AppLockPoisoned> for ShareError {
    fn from(error: AppLockPoisoned) -> Self {
        Self::Internal(error.to_string())
    }
}

impl From<AppLockPoisoned> for CommandError {
    fn from(error: AppLockPoisoned) -> Self {
        Self::internal(error)
    }
}

pub fn lock_app(state: &SharedAppData) -> Result<AppGuard<'_>, AppLockPoisoned> {
    let guard = state.lock().map_err(|_| AppLockPoisoned)?;
    let changes = guard.database.connection.total_changes();
    Ok(AppGuard { guard, changes })
}

/// The locked application state. Other windows reload when it is released if
/// anything was written through its connection, so no write path has to
/// remember to announce itself.
pub struct AppGuard<'a> {
    guard: MutexGuard<'a, AppData>,
    changes: u64,
}

impl AppGuard<'_> {
    /// Leaves the writes made so far unannounced, for caches no window shows.
    pub fn forget_writes(&mut self) {
        self.changes = self.guard.database.connection.total_changes();
    }
}

impl Deref for AppGuard<'_> {
    type Target = AppData;

    fn deref(&self) -> &AppData {
        &self.guard
    }
}

impl DerefMut for AppGuard<'_> {
    fn deref_mut(&mut self) -> &mut AppData {
        &mut self.guard
    }
}

impl Drop for AppGuard<'_> {
    fn drop(&mut self) {
        if self.guard.database.connection.total_changes() == self.changes {
            return;
        }
        if let Some(notify) = &self.guard.on_data_changed {
            notify();
        }
    }
}

impl AppData {
    pub fn new(
        stored: StoredData,
        database: Database,
        recovery_messages: Vec<String>,
        archives_loaded: bool,
    ) -> Result<Self, String> {
        let boards = database.boards().map_err(|error| error.to_string())?;
        let active_board_id = database.active_board_id();
        let mut data = Self {
            stored,
            boards,
            active_board_id,
            undo_history: VecDeque::new(),
            redo_history: Vec::new(),
            labels: BTreeSet::new(),
            database,
            recovery_messages,
            archives_loaded,
            on_data_changed: None,
        };
        data.refresh_labels();
        data.forget_deleted_muted_columns();
        Ok(data)
    }

    /// Forget muted columns that were deleted here or by other members of a
    /// shared board. Deleting a column cannot do this itself, because undoing
    /// the deletion keeps the current settings; at startup no undo history
    /// can restore a column.
    fn forget_deleted_muted_columns(&mut self) {
        if self.stored.settings.notify_muted_columns.is_empty() {
            return;
        }
        let columns = match self.database.column_ids() {
            Ok(columns) => columns,
            Err(error) => {
                log::warn!(target: "notifications", "Could not read columns to forget deleted muted columns: {error}");
                return;
            }
        };
        let mut muted = self.stored.settings.notify_muted_columns.clone();
        muted.retain(|board_id, column_ids| {
            column_ids.retain(|column_id| columns.contains(&(*board_id, *column_id)));
            !column_ids.is_empty()
        });
        if muted == self.stored.settings.notify_muted_columns {
            return;
        }
        if let Err(error) = update_locked(self, |data| {
            data.settings.notify_muted_columns = muted;
            Ok(())
        }) {
            log::warn!(target: "notifications", "Could not forget deleted muted columns: {error}");
        }
    }

    /// Install board contents that changed outside the local undo history.
    /// Undo snapshots describe the previous contents, so they are discarded.
    pub fn replace_board_contents(&mut self, stored: StoredData) {
        self.stored = stored;
        self.undo_history.clear();
        self.redo_history.clear();
        self.refresh_labels();
    }

    /// Make `board_id` the active board, reloaded from SQLite with archives.
    pub fn load_active_board(&mut self, board_id: i64) -> storage::StorageResult<()> {
        let stored = self.database.load_board(board_id)?;
        self.active_board_id = board_id;
        self.archives_loaded = true;
        self.replace_board_contents(stored);
        Ok(())
    }

    pub fn refresh_labels(&mut self) {
        self.labels = self
            .stored
            .columns
            .iter()
            .flat_map(|column| column.tasks.iter())
            .flat_map(|task| task.labels.iter().cloned())
            .collect();
    }

    pub fn ordered_labels(&self) -> Vec<String> {
        let mut labels = self
            .stored
            .label_recency
            .iter()
            .filter(|label| self.labels.contains(*label))
            .cloned()
            .collect::<Vec<_>>();
        let ordered = labels.iter().cloned().collect::<HashSet<_>>();
        labels.extend(
            self.labels
                .iter()
                .filter(|label| !ordered.contains(label.as_str()))
                .cloned(),
        );
        labels
    }
}

pub fn update_stored<R>(
    state: &State<'_, SharedAppData>,
    change: impl FnOnce(&mut StoredData) -> Result<R, String>,
) -> Result<R, String> {
    let mut guard = lock_app(state)?;
    update_locked(&mut guard, change)
}

/// Apply a board-owned mutation only when the caller's board context is still
/// current.  This prevents delayed UI requests from being written to a board
/// selected after the request was created.
pub fn update_stored_for_board<R>(
    state: &State<'_, SharedAppData>,
    expected_board_id: i64,
    change: impl FnOnce(&mut StoredData) -> Result<R, DomainError>,
) -> Result<R, DomainError> {
    let mut guard = lock_app(state)?;
    if guard.active_board_id != expected_board_id {
        return Err(DomainError::StaleBoardRequest);
    }
    ensure_board_can_edit(&guard)?;
    update_locked_typed(&mut guard, change)
}

pub fn update_stored_with_archives_for_board<R>(
    state: &State<'_, SharedAppData>,
    expected_board_id: i64,
    change: impl FnOnce(&mut StoredData) -> Result<R, DomainError>,
) -> Result<R, DomainError> {
    let mut guard = lock_app(state)?;
    if guard.active_board_id != expected_board_id {
        return Err(DomainError::StaleBoardRequest);
    }
    ensure_board_can_edit(&guard)?;
    ensure_archives_loaded_locked(&mut guard)?;
    update_locked_typed(&mut guard, change)
}

pub fn load_archives_for_board(
    state: &State<'_, SharedAppData>,
    expected_board_id: i64,
) -> Result<Vec<crate::models::Archive>, DomainError> {
    let mut guard = lock_app(state)?;
    if guard.active_board_id != expected_board_id {
        return Err(DomainError::StaleBoardRequest);
    }
    ensure_archives_loaded_locked(&mut guard)?;
    Ok(guard.stored.archives.clone())
}

pub fn read_with_archives_for_board<R>(
    state: &State<'_, SharedAppData>,
    expected_board_id: i64,
    read: impl FnOnce(&AppData) -> R,
) -> Result<R, DomainError> {
    let mut guard = lock_app(state)?;
    if guard.active_board_id != expected_board_id {
        return Err(DomainError::StaleBoardRequest);
    }
    ensure_archives_loaded_locked(&mut guard)?;
    Ok(read(&guard))
}

pub fn update_stored_with_pre_import_snapshot_for_board<R>(
    state: &State<'_, SharedAppData>,
    expected_board_id: i64,
    flow: Option<&crate::flow::FlowData>,
    notes: &[crate::models::Note],
    change: impl FnOnce(&mut StoredData) -> Result<R, DomainError>,
) -> Result<(R, PathBuf), DomainError> {
    let mut guard = lock_app(state)?;
    if guard.active_board_id != expected_board_id {
        return Err(DomainError::StaleBoardRequest);
    }
    ensure_board_can_edit(&guard)?;
    ensure_archives_loaded_locked(&mut guard)?;
    let snapshot_path = write_pre_import_snapshot(&guard)?;
    let result = import_locked(&mut guard, flow, notes, change)?;
    Ok((result, snapshot_path))
}

fn write_pre_import_snapshot(guard: &AppData) -> Result<PathBuf, String> {
    let snapshot_path = guard.database.path().with_file_name("data.pre-import.json");
    let backup = guard
        .database
        .read_single_board_backup(guard.active_board_id)
        .map_err(|error| error.to_string())?;
    // Keep the original StoredData fields for legacy recovery readers, adding
    // the portable Flow/notes fields understood by single-board import.
    let mut snapshot = serde_json::to_value(&guard.stored).map_err(|error| error.to_string())?;
    snapshot["flow"] = serde_json::to_value(backup.flow).map_err(|error| error.to_string())?;
    snapshot["notes"] = serde_json::to_value(backup.notes).map_err(|error| error.to_string())?;
    storage::write_document(&snapshot_path, &snapshot)
        .map_err(|error| format!("Could not create pre-import backup: {error}"))?;
    Ok(snapshot_path)
}

fn import_locked<R>(
    guard: &mut AppData,
    flow: Option<&crate::flow::FlowData>,
    notes: &[crate::models::Note],
    change: impl FnOnce(&mut StoredData) -> Result<R, DomainError>,
) -> Result<R, DomainError> {
    let mut candidate = guard.stored.clone();
    let result = change(&mut candidate)?;
    let now = crate::commands::archive::current_time_millis()?;
    crate::completion::reconcile(&guard.stored, &mut candidate, now)?;
    candidate.keep_pinned_tasks_first();
    let board_changed = candidate.columns != guard.stored.columns
        || candidate.archives != guard.stored.archives
        || candidate.templates != guard.stored.templates;
    let stats = guard
        .database
        .import_board_data(&guard.stored, &candidate, flow, notes)
        .map_err(DomainError::repository)?;
    let previous = std::mem::replace(&mut guard.stored, candidate);
    refresh_active_board_summary(guard)?;
    if flow.is_some() {
        // Task-only Undo cannot restore canvases or global notes. The complete
        // pre-import file is the recovery point for a portable board import.
        guard.undo_history.clear();
        guard.redo_history.clear();
    } else if board_changed {
        guard.record_board_undo(previous, stats.event_op);
    }
    guard.refresh_labels();
    Ok(result)
}

fn update_locked<R>(
    guard: &mut AppData,
    change: impl FnOnce(&mut StoredData) -> Result<R, String>,
) -> Result<R, String> {
    update_locked_typed(guard, change)
}

fn update_locked_typed<R, E: From<String>>(
    guard: &mut AppData,
    change: impl FnOnce(&mut StoredData) -> Result<R, E>,
) -> Result<R, E> {
    update_locked_with_events(guard, EventWrite::Record(EventSource::local()), change)
}

fn update_locked_with_events<R, E: From<String>>(
    guard: &mut AppData,
    events: EventWrite,
    change: impl FnOnce(&mut StoredData) -> Result<R, E>,
) -> Result<R, E> {
    let mut candidate = guard.stored.clone();
    let result = change(&mut candidate)?;
    let now = crate::commands::archive::current_time_millis()
        .map_err(|error| E::from(error.to_string()))?;
    crate::completion::reconcile(&guard.stored, &mut candidate, now)
        .map_err(|error| E::from(error.to_string()))?;
    candidate.keep_pinned_tasks_first();
    let board_changed = candidate.columns != guard.stored.columns
        || candidate.archives != guard.stored.archives
        || candidate.templates != guard.stored.templates;

    let archives_loaded = guard.archives_loaded;
    let stats = guard
        .database
        .persist_diff_with_events(&guard.stored, &candidate, archives_loaded, events)
        .map_err(|error| error.to_string())?;

    let previous = std::mem::replace(&mut guard.stored, candidate);
    refresh_active_board_summary(guard)?;
    if board_changed {
        guard.record_board_undo(previous, stats.event_op);
    }
    guard.refresh_labels();
    Ok(result)
}

const MAX_UNDO_HISTORY: usize = 50;

impl AppData {
    /// Record a new local change; it replaces whatever could be redone.
    pub(crate) fn record_board_undo(&mut self, previous: StoredData, event_op: Option<i64>) {
        self.redo_history.clear();
        self.push_undo(previous, event_op);
    }

    /// Records `previous` once `stored` holds the changed contents.
    fn push_undo(&mut self, previous: StoredData, event_op: Option<i64>) {
        let entry = UndoEntry::new(previous, &self.stored, event_op);
        self.undo_history.push_back(entry);
        if self.undo_history.len() > MAX_UNDO_HISTORY {
            self.undo_history.pop_front();
        }
    }
}

fn refresh_active_board_summary(guard: &mut AppData) -> Result<(), String> {
    let (revision, status) = guard
        .database
        .board_sync_state(guard.active_board_id)
        .map_err(|e| e.to_string())?;
    if let Some(board) = guard
        .boards
        .iter_mut()
        .find(|board| board.id == guard.active_board_id)
    {
        board.sync_revision = revision;
        board.sync_status = status;
        board.task_count = guard
            .stored
            .columns
            .iter()
            .map(|column| column.tasks.len() as i64)
            .sum();
    }
    Ok(())
}

// Backend enforcement matters: a client must not bypass the disabled UI by
// directly invoking a mutation command for a received read-only board.
fn ensure_board_can_edit(guard: &AppData) -> Result<(), DomainError> {
    if guard
        .database
        .board_role(guard.active_board_id)
        .map_err(DomainError::repository)?
        == BoardRole::Viewer
    {
        Err(DomainError::PermissionDenied)
    } else {
        Ok(())
    }
}

fn ensure_archives_loaded_locked(guard: &mut AppData) -> Result<(), String> {
    if guard.archives_loaded {
        return Ok(());
    }
    let archives = guard
        .database
        .load_archives()
        .map_err(|error| error.to_string())?;
    // Undo entries without archives take these loaded ones.
    guard.stored.archives = archives;
    guard.archives_loaded = true;
    Ok(())
}

pub fn undo_last_change_for_board(
    state: &State<'_, SharedAppData>,
    expected_board_id: i64,
) -> Result<bool, DomainError> {
    let mut guard = lock_app(state)?;
    undo_locked(&mut guard, expected_board_id)
}

fn undo_locked(guard: &mut AppData, expected_board_id: i64) -> Result<bool, DomainError> {
    ensure_can_restore(guard, expected_board_id)?;
    let entry = guard
        .undo_history
        .pop_back()
        .ok_or(DomainError::NothingToUndo)?;
    // On failure the change stays undoable.
    let (current, op) = restore_locked(guard, entry, |guard, entry| {
        guard.undo_history.push_back(entry)
    })?;
    let entry = UndoEntry::new(current, &guard.stored, op);
    guard.redo_history.push(entry);
    finish_restore(guard)?;
    Ok(!guard.undo_history.is_empty())
}

pub fn redo_last_change_for_board(
    state: &State<'_, SharedAppData>,
    expected_board_id: i64,
) -> Result<bool, DomainError> {
    let mut guard = lock_app(state)?;
    redo_locked(&mut guard, expected_board_id)
}

/// Reapply the most recently undone change. Its task events revert the undo
/// operation, so readers see an undo of an undo.
fn redo_locked(guard: &mut AppData, expected_board_id: i64) -> Result<bool, DomainError> {
    ensure_can_restore(guard, expected_board_id)?;
    let entry = guard.redo_history.pop().ok_or(DomainError::NothingToRedo)?;
    // On failure the change stays redoable.
    let (current, op) =
        restore_locked(guard, entry, |guard, entry| guard.redo_history.push(entry))?;
    guard.push_undo(current, op);
    finish_restore(guard)?;
    Ok(!guard.redo_history.is_empty())
}

fn ensure_can_restore(guard: &AppData, expected_board_id: i64) -> Result<(), DomainError> {
    if guard.active_board_id != expected_board_id {
        return Err(DomainError::StaleBoardRequest);
    }
    ensure_board_can_edit(guard)
}

/// Install a recorded entry as a local change that reverts its `event_op`, and
/// return the replaced contents with the operation recorded for this change.
/// When nothing was written, the entry is handed to `put_back`.
fn restore_locked(
    guard: &mut AppData,
    entry: UndoEntry,
    put_back: impl FnOnce(&mut AppData, UndoEntry),
) -> Result<(StoredData, Option<i64>), DomainError> {
    let event_op = entry.event_op;
    let target = entry.restore(&guard.stored);

    let archives_loaded = guard.archives_loaded;
    let AppData {
        database, stored, ..
    } = &mut *guard;
    let stats = match database.persist_diff_with_events(
        stored,
        &target,
        archives_loaded,
        EventWrite::Record(EventSource::undo(event_op)),
    ) {
        Ok(stats) => stats,
        Err(error) => {
            let entry = UndoEntry::new(target, &guard.stored, event_op);
            put_back(guard, entry);
            return Err(error.to_string().into());
        }
    };
    let current = std::mem::replace(&mut guard.stored, target);
    Ok((current, stats.event_op))
}

fn finish_restore(guard: &mut AppData) -> Result<(), DomainError> {
    refresh_active_board_summary(guard)?;
    guard.refresh_labels();
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::models::Task;
    use std::{
        fs,
        time::{SystemTime, UNIX_EPOCH},
    };

    fn app_data(dir: &std::path::Path, stored: StoredData) -> AppData {
        let mut database = Database::open(dir.join("data.sqlite3")).unwrap();
        database.initialize_boards(&stored).unwrap();
        AppData::new(stored, database, Vec::new(), true).unwrap()
    }

    #[test]
    fn releasing_the_state_announces_only_writes() {
        let dir = std::env::temp_dir().join(format!(
            "cardbe-data-changed-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir_all(&dir).unwrap();
        let mut app = app_data(&dir, StoredData::default());
        let announced = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let counter = announced.clone();
        app.on_data_changed = Some(Arc::new(move || {
            counter.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        }));
        let state = Mutex::new(app);
        let count = || announced.load(std::sync::atomic::Ordering::SeqCst);

        drop(lock_app(&state).unwrap().database.boards().unwrap());
        assert_eq!(count(), 0);

        let mut guard = lock_app(&state).unwrap();
        update_locked(&mut guard, |data| {
            data.label_recency = vec!["changed".into()];
            Ok(())
        })
        .unwrap();
        assert_eq!(count(), 0, "announced before the lock was released");
        drop(guard);
        assert_eq!(count(), 1);

        let mut guard = lock_app(&state).unwrap();
        update_locked(&mut guard, |data| {
            data.label_recency = vec!["cached".into()];
            Ok(())
        })
        .unwrap();
        guard.forget_writes();
        drop(guard);
        assert_eq!(count(), 1);

        drop(state);
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn local_writes_complete_tasks_in_the_done_column_and_undo_reopens_them() {
        let dir = std::env::temp_dir().join(format!(
            "cardbe-done-column-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir_all(&dir).unwrap();
        let column = |id, done, tasks| Column {
            id,
            name: format!("Column {id}"),
            color: String::new(),
            sort_order: Default::default(),
            sort_rules: Vec::new(),
            done,
            tasks,
        };
        let stored = StoredData {
            columns: vec![
                column(
                    1,
                    false,
                    vec![Task {
                        id: 3,
                        ..Task::default()
                    }],
                ),
                column(2, true, vec![]),
            ],
            ..StoredData::default()
        };
        let mut app = app_data(&dir, stored);
        let board_id = app.active_board_id;
        update_locked(&mut app, |data| {
            crate::board_mutations::move_task(data, 3, 2, None).map_err(|e| e.to_string())
        })
        .unwrap();
        let completed = app.stored.columns[1].tasks[0].completed_at;
        assert!(completed.is_some());
        assert_eq!(
            app.database.read_board_complete(board_id).unwrap().columns,
            app.stored.columns
        );

        assert!(!undo_locked(&mut app, board_id).unwrap());
        assert_eq!(app.stored.columns[0].tasks[0].completed_at, None);
        drop(app);
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn startup_forgets_muted_columns_that_no_longer_exist() {
        let dir = std::env::temp_dir().join(format!(
            "cardbe-muted-columns-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir_all(&dir).unwrap();
        let column = |id| Column {
            id,
            name: format!("Column {id}"),
            color: String::new(),
            sort_order: Default::default(),
            sort_rules: Vec::new(),
            done: false,
            tasks: Vec::new(),
        };
        let stored = StoredData {
            columns: vec![column(10), column(11)],
            ..StoredData::default()
        };
        let app = app_data(&dir, stored);
        let board_id = app.active_board_id;
        drop(app);

        let mut database = Database::open(dir.join("data.sqlite3")).unwrap();
        let before = database.load_board(board_id).unwrap();
        let mut stored = before.clone();
        stored.settings.notify_muted_columns = std::collections::BTreeMap::from([
            (board_id, BTreeSet::from([10, 99])),
            (board_id + 1, BTreeSet::from([10])),
        ]);
        database.persist_diff(&before, &stored).unwrap();
        let app = AppData::new(stored, database, Vec::new(), true).unwrap();

        let expected = std::collections::BTreeMap::from([(board_id, BTreeSet::from([10]))]);
        assert_eq!(app.stored.settings.notify_muted_columns, expected);
        assert!(app.undo_history.is_empty());
        assert_eq!(
            app.database
                .read_board(board_id)
                .unwrap()
                .settings
                .notify_muted_columns,
            expected
        );
        drop(app);
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn failed_change_does_not_commit_memory_candidate() {
        let dir = std::env::temp_dir().join(format!(
            "cardbe-failed-change-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir_all(&dir).unwrap();
        let mut app = app_data(&dir, StoredData::default());

        let result: Result<(), String> = update_locked(&mut app, |data| {
            data.columns.push(Column {
                id: 0,
                name: "Must not commit".into(),
                color: String::new(),
                sort_order: Default::default(),
                sort_rules: Vec::new(),
                done: false,
                tasks: Vec::new(),
            });
            Err("Rejected change".to_string())
        });

        assert!(result.is_err());
        assert!(app.stored.columns.is_empty());
        drop(app);
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn pre_import_snapshot_survives_later_regular_writes() {
        let dir = std::env::temp_dir().join(format!(
            "cardbe-pre-import-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir_all(&dir).unwrap();

        let mut stored = StoredData::default();
        stored.columns.push(Column {
            id: 0,
            name: "Before import".into(),
            color: String::new(),
            sort_order: Default::default(),
            sort_rules: Vec::new(),
            done: false,
            tasks: Vec::new(),
        });
        let mut app = app_data(&dir, stored);
        let note = app
            .database
            .create_note_with_content(123, "備註".into(), "必須保留".into())
            .unwrap();
        let layout: crate::flow::FlowLayout = serde_json::from_value(serde_json::json!({
            "nodes": [], "viewport": {"x": 12, "y": 24, "zoom": 2},
            "annotations": [{"id": "note", "kind": "note", "note_id": note.id, "x": 0, "y": 0, "width": 240}]
        })).unwrap();
        app.database
            .save_flow_layout(app.active_board_id, crate::flow::DEFAULT_TAB_ID, &layout)
            .unwrap();
        let snapshot_path = write_pre_import_snapshot(&app).unwrap();

        update_locked(&mut app, |data| {
            data.columns[0].name = "Imported".into();
            Ok(())
        })
        .unwrap();
        update_locked(&mut app, |data| {
            data.columns[0].name = "Edited after import".into();
            Ok(())
        })
        .unwrap();

        let json = fs::read_to_string(snapshot_path).unwrap();
        let snapshot: StoredData = serde_json::from_str(&json).unwrap();
        assert_eq!(snapshot.columns[0].name, "Before import");
        let portable: crate::models::SingleBoardExport = serde_json::from_str(&json).unwrap();
        assert_eq!(portable.flow.unwrap().layout, layout);
        assert_eq!(portable.notes, vec![note]);
        drop(app);
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn single_board_flow_import_commits_memory_and_clears_task_only_undo_on_success() {
        let dir = std::env::temp_dir().join(format!(
            "cardbe-flow-import-state-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir_all(&dir).unwrap();
        let mut app = app_data(&dir, StoredData::default());
        update_locked(&mut app, |data| {
            data.columns.push(Column {
                id: 1,
                name: "Before".into(),
                color: String::new(),
                sort_order: Default::default(),
                sort_rules: vec![],
                done: false,
                tasks: vec![],
            });
            Ok(())
        })
        .unwrap();
        assert_eq!(app.undo_history.len(), 1);
        let before = app.stored.clone();
        let flow = crate::flow::FlowData::default();
        app.database.connection.execute_batch("CREATE TRIGGER fail_import BEFORE INSERT ON cardbe_flow_layouts BEGIN SELECT RAISE(ABORT,'test failure'); END;").unwrap();
        assert!(import_locked(&mut app, Some(&flow), &[], |data| {
            data.columns[0].name = "Rejected".into();
            Ok(())
        })
        .is_err());
        assert_eq!(app.stored, before);
        assert_eq!(app.undo_history.len(), 1);
        app.database
            .connection
            .execute_batch("DROP TRIGGER fail_import;")
            .unwrap();
        import_locked(&mut app, Some(&flow), &[], |data| {
            data.columns[0].name = "Imported".into();
            Ok(())
        })
        .unwrap();
        assert_eq!(app.stored.columns[0].name, "Imported");
        assert!(app.undo_history.is_empty());
        assert!(app.redo_history.is_empty());
        // Legacy task-only imports preserve their established Undo behavior.
        import_locked(&mut app, None, &[], |data| {
            data.columns[0].name = "Legacy import".into();
            Ok(())
        })
        .unwrap();
        assert_eq!(app.undo_history.len(), 1);
        drop(app);
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn explicit_board_column_creation_preserves_active_board_and_undo() {
        let dir = std::env::temp_dir().join(format!(
            "cardbe-column-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir_all(&dir).unwrap();
        let mut app = app_data(&dir, StoredData::default());
        let active = app.active_board_id;
        let target = app.database.create_board("Target").unwrap().id;
        let id =
            crate::commands::board::add_column_to_board_locked(&mut app, target, " Todo ".into())
                .unwrap();
        assert_eq!(app.active_board_id, active);
        assert!(app.stored.columns.is_empty());
        assert!(app.undo_history.is_empty());
        let data = app.database.read_board_complete(target).unwrap();
        assert_eq!(data.columns[0].id, id);
        assert_eq!(data.columns[0].name, "Todo");
        assert!(
            crate::commands::board::add_column_to_board_locked(&mut app, target, " ".into())
                .is_err()
        );
        crate::commands::board::add_column_to_board_locked(&mut app, active, "Active".into())
            .unwrap();
        assert!(!undo_locked(&mut app, active).unwrap());
        assert!(app.stored.columns.is_empty());
        drop(app);
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn successful_updates_are_kept_for_undo() {
        let dir = std::env::temp_dir().join(format!(
            "cardbe-undo-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir_all(&dir).unwrap();
        let mut app = app_data(&dir, StoredData::default());

        update_locked(&mut app, |data| {
            data.columns.push(Column {
                id: 0,
                name: "Added".into(),
                color: String::new(),
                sort_order: Default::default(),
                sort_rules: Vec::new(),
                done: false,
                tasks: Vec::new(),
            });
            Ok(())
        })
        .unwrap();

        assert_eq!(app.undo_history.len(), 1);
        assert!(app.undo_history[0].columns.is_empty());
        assert_eq!(app.undo_history[0].archives, None);
        assert_eq!(app.undo_history[0].templates, None);
        update_locked(&mut app, |data| {
            data.settings.notify_enabled = !data.settings.notify_enabled;
            data.settings.theme = Some("morandi-dark-blue".into());
            data.settings.iroh_network.listen_port = 12345;
            Ok(())
        })
        .unwrap();
        assert_eq!(app.undo_history.len(), 1);
        let settings = app.stored.settings.clone();
        let board_id = app.active_board_id;
        crate::commands::board::add_task_to_board_locked(&mut app, board_id, 0, Task::default())
            .unwrap();
        assert_eq!(app.undo_history.len(), 2);
        assert_eq!(app.stored.columns[0].tasks.len(), 1);
        assert!(undo_locked(&mut app, board_id).unwrap());
        assert_eq!(app.stored.columns.len(), 1);
        assert!(app.stored.columns[0].tasks.is_empty());
        assert!(!undo_locked(&mut app, board_id).unwrap());
        assert!(app.stored.columns.is_empty());
        assert_eq!(app.stored.settings, settings);
        assert_eq!(
            app.database.read_board_complete(board_id).unwrap().settings,
            settings
        );
        drop(app);
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn undo_entries_keep_only_the_archives_a_change_replaced() {
        let dir = std::env::temp_dir().join(format!(
            "cardbe-undo-archives-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir_all(&dir).unwrap();
        let stored = StoredData {
            columns: vec![Column {
                id: 1,
                name: "Todo".into(),
                color: String::new(),
                sort_order: Default::default(),
                sort_rules: Vec::new(),
                done: false,
                tasks: vec![Task {
                    id: 1,
                    ..Task::default()
                }],
            }],
            next_task_id: 2,
            ..StoredData::default()
        };
        let mut app = app_data(&dir, stored);
        let board_id = app.active_board_id;
        update_locked(&mut app, |data| {
            let task = data.columns[0].tasks.remove(0);
            data.archives.push(Archive { time: 1, task });
            Ok(())
        })
        .unwrap();
        assert_eq!(app.undo_history[0].archives, Some(Vec::new()));
        assert_eq!(app.undo_history[0].templates, None);

        assert!(!undo_locked(&mut app, board_id).unwrap());
        assert!(app.stored.archives.is_empty());
        assert_eq!(app.stored.columns[0].tasks[0].id, 1);
        let persisted = app.database.read_board_complete(board_id).unwrap();
        assert!(persisted.archives.is_empty());
        assert_eq!(persisted.columns, app.stored.columns);
        drop(app);
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn redo_reapplies_undone_changes_until_a_new_change() {
        let dir = std::env::temp_dir().join(format!(
            "cardbe-redo-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir_all(&dir).unwrap();
        let mut app = app_data(&dir, StoredData::default());
        let board_id = app.active_board_id;
        let add_column = |app: &mut AppData, name: &str| {
            update_locked_typed(app, |data| {
                crate::board_mutations::add_column(data, name.into(), String::new())
            })
            .unwrap();
        };
        add_column(&mut app, "First");
        add_column(&mut app, "Second");

        assert!(matches!(
            redo_locked(&mut app, board_id),
            Err(DomainError::NothingToRedo)
        ));
        assert!(undo_locked(&mut app, board_id).unwrap());
        assert!(!undo_locked(&mut app, board_id).unwrap());
        assert!(app.stored.columns.is_empty());
        assert!(matches!(
            redo_locked(&mut app, board_id + 1),
            Err(DomainError::StaleBoardRequest)
        ));

        assert!(redo_locked(&mut app, board_id).unwrap());
        assert_eq!(app.stored.columns.len(), 1);
        assert_eq!(app.undo_history.len(), 1);
        assert_eq!(
            app.database
                .read_board_complete(board_id)
                .unwrap()
                .columns
                .len(),
            1
        );
        // Undoing a redo goes back again.
        assert!(!undo_locked(&mut app, board_id).unwrap());
        assert_eq!(app.redo_history.len(), 2);
        assert!(redo_locked(&mut app, board_id).unwrap());

        // A new local change discards what could still be redone.
        add_column(&mut app, "Third");
        assert!(app.redo_history.is_empty());

        // So do changes from outside the local history.
        assert!(undo_locked(&mut app, board_id).unwrap());
        app.replace_board_contents(app.stored.clone());
        assert!(app.redo_history.is_empty());
        assert!(app.undo_history.is_empty());
        drop(app);
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn local_changes_and_undo_append_task_events() {
        use crate::task_events::{EventKind::*, EventOrigin::*};
        let dir = std::env::temp_dir().join(format!(
            "cardbe-task-events-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir_all(&dir).unwrap();
        let mut app = app_data(&dir, StoredData::default());
        let board_id = app.active_board_id;
        let column = |id: i64| Column {
            id,
            name: String::new(),
            color: String::new(),
            sort_order: Default::default(),
            sort_rules: Vec::new(),
            done: false,
            tasks: Vec::new(),
        };
        let events = |app: &AppData| {
            app.database
                .task_events(board_id, None)
                .unwrap()
                .into_iter()
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

        update_locked(&mut app, |data| {
            data.columns.extend([column(1), column(2)]);
            Ok(())
        })
        .unwrap();
        assert!(events(&app).is_empty());
        update_locked(&mut app, |data| {
            data.columns[0].tasks.push(Task {
                id: 10,
                ..Task::default()
            });
            Ok(())
        })
        .unwrap();
        update_locked(&mut app, |data| {
            let task = data.columns[0].tasks.remove(0);
            data.columns[1].tasks.push(task);
            Ok(())
        })
        .unwrap();
        // Content edits are not lifecycle events.
        update_locked(&mut app, |data| {
            data.columns[1].tasks[0].title = "Renamed".into();
            Ok(())
        })
        .unwrap();
        assert!(undo_locked(&mut app, board_id).unwrap());
        assert!(undo_locked(&mut app, board_id).unwrap());
        update_locked(&mut app, |data| {
            let task = data.columns[0].tasks.remove(0);
            data.archives.push(Archive { time: 1, task });
            Ok(())
        })
        .unwrap();
        assert_eq!(
            events(&app),
            vec![
                (Local, Created, 10, None, Some(1)),
                (Local, Moved, 10, Some(1), Some(2)),
                (Undo, Moved, 10, Some(2), Some(1)),
                (Local, Archived, 10, Some(1), None),
            ]
        );
        let logged = app.database.task_events(board_id, None).unwrap();
        assert_eq!(logged[2].reverts_op_id, Some(logged[1].op_id));

        let task_id = crate::commands::board::add_task_to_board_locked(
            &mut app,
            board_id,
            1,
            Task::default(),
        )
        .unwrap();
        undo_locked(&mut app, board_id).unwrap();
        update_locked_with_events(
            &mut app,
            EventWrite::Reset(EventSource::local()),
            |data: &mut StoredData| -> Result<(), String> {
                data.archives.clear();
                Ok(())
            },
        )
        .unwrap();
        let logged = app.database.task_events(board_id, None).unwrap();
        assert_eq!(
            events(&app)[4..],
            [
                (Local, Created, task_id, None, Some(1)),
                (Undo, Deleted, task_id, Some(1), None),
                (Local, BoardReset, 0, None, None),
            ]
        );
        assert_eq!(logged[5].reverts_op_id, Some(logged[4].op_id));
        // A task's history keeps board resets, which end older ids.
        assert_eq!(
            app.database
                .task_events(board_id, Some(task_id))
                .unwrap()
                .iter()
                .map(|e| e.id)
                .collect::<Vec<_>>(),
            [logged[4].id, logged[5].id, logged[6].id]
        );
        assert!(logged.windows(2).all(|pair| pair[0].id < pair[1].id));
        // A loaded log grows with only the events recorded after it.
        assert_eq!(
            app.database
                .task_events_after(board_id, Some(logged[4].id))
                .unwrap(),
            logged[5..]
        );

        drop(app);
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn undo_after_loading_archives_keeps_them() {
        let dir = std::env::temp_dir().join(format!(
            "cardbe-lazy-undo-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir_all(&dir).unwrap();
        let archive = Archive {
            time: 123,
            task: Task {
                id: 7,
                title: "Archived".into(),
                ..Task::default()
            },
        };
        let mut persisted = StoredData::default();
        persisted.archives.push(archive.clone());
        persisted.next_task_id = 8;
        let mut database = Database::open(dir.join("data.sqlite3")).unwrap();
        database.initialize_boards(&persisted).unwrap();

        let mut unloaded = persisted;
        unloaded.archives.clear();
        let mut app = AppData::new(unloaded, database, Vec::new(), false).unwrap();
        update_locked(&mut app, |data| {
            data.columns.push(Column {
                id: 0,
                name: "Added before opening archives".into(),
                color: String::new(),
                sort_order: Default::default(),
                sort_rules: Vec::new(),
                done: false,
                tasks: Vec::new(),
            });
            Ok(())
        })
        .unwrap();

        ensure_archives_loaded_locked(&mut app).unwrap();
        assert_eq!(app.stored.archives, vec![archive.clone()]);
        let board_id = app.active_board_id;
        assert!(!undo_locked(&mut app, board_id).unwrap());
        assert!(app.stored.columns.is_empty());
        assert_eq!(app.stored.archives, vec![archive.clone()]);
        assert_eq!(
            app.database.read_board_complete(board_id).unwrap().archives,
            vec![archive]
        );
        drop(app);
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn reloading_the_active_board_discards_undo_and_refreshes_derived_state() {
        let dir = std::env::temp_dir().join(format!(
            "cardbe-reload-board-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir_all(&dir).unwrap();
        let mut persisted = StoredData::default();
        persisted.columns.push(Column {
            id: 1,
            name: "Persisted".into(),
            color: String::new(),
            sort_order: Default::default(),
            sort_rules: Vec::new(),
            done: false,
            tasks: vec![Task {
                id: 1,
                title: "Labelled".into(),
                labels: vec!["saved".into()],
                ..Task::default()
            }],
        });
        persisted.archives.push(Archive {
            time: 123,
            task: Task {
                id: 2,
                title: "Archived".into(),
                ..Task::default()
            },
        });
        persisted.next_task_id = 3;
        let mut database = Database::open(dir.join("data.sqlite3")).unwrap();
        database.initialize_boards(&persisted).unwrap();
        let board_id = database.active_board_id();
        let mut app = AppData::new(StoredData::default(), database, Vec::new(), false).unwrap();
        app.undo_history.push_back(StoredData::default().into());

        app.load_active_board(board_id).unwrap();

        assert_eq!(app.active_board_id, board_id);
        assert!(app.archives_loaded);
        assert_eq!(app.stored.columns, persisted.columns);
        assert_eq!(app.stored.archives, persisted.archives);
        assert!(app.undo_history.is_empty());
        assert_eq!(app.ordered_labels(), vec!["saved".to_string()]);
        drop(app);
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn poisoned_app_lock_keeps_the_internal_error_for_every_boundary() {
        let dir = std::env::temp_dir().join(format!(
            "cardbe-poisoned-lock-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir_all(&dir).unwrap();
        let state: SharedAppData = Mutex::new(app_data(&dir, StoredData::default()));
        assert!(lock_app(&state).is_ok());
        std::thread::scope(|scope| {
            let _ = scope
                .spawn(|| {
                    let _guard = state.lock().unwrap();
                    panic!("poison the application state");
                })
                .join();
        });

        let message = "Application state lock is poisoned";
        let poisoned = || lock_app(&state).err().unwrap();
        assert_eq!(String::from(poisoned()), message);
        assert!(matches!(
            DomainError::from(poisoned()),
            DomainError::Internal(detail) if detail == message
        ));
        assert!(matches!(
            ShareError::from(poisoned()),
            ShareError::Internal(detail) if detail == message
        ));
        assert!(matches!(
            CommandError::from(poisoned()),
            CommandError::InternalError
        ));
        drop(state);
        fs::remove_dir_all(dir).unwrap();
    }
}
