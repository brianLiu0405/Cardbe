use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};

pub const CURRENT_SCHEMA_VERSION: u32 = 8;

#[derive(Deserialize, Serialize, Clone, Copy, Debug, Default, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum BoardRole {
    #[default]
    Owner,
    Editor,
    Viewer,
}

#[derive(Deserialize, Serialize, Clone, Copy, Debug, Default, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum SyncStatus {
    #[default]
    Local,
    Pending,
    Synced,
    Conflict,
}

#[derive(Deserialize, Serialize, Clone, Copy, Debug, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum IrohPermission {
    Viewer,
    Editor,
}

impl From<IrohPermission> for BoardRole {
    fn from(permission: IrohPermission) -> Self {
        match permission {
            IrohPermission::Viewer => Self::Viewer,
            IrohPermission::Editor => Self::Editor,
        }
    }
}

#[derive(Serialize, Clone, Copy, Debug, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum IrohDeviceStatus {
    Pending,
    Approved,
    Revoked,
}

/// A board column for overviews, without its tasks.
#[derive(Serialize, Clone, Debug, PartialEq, Eq)]
pub struct ColumnOverview {
    pub board_id: i64,
    pub id: i64,
    pub name: String,
    pub done: bool,
    pub task_count: i64,
}

#[derive(Deserialize, Serialize, Clone, Debug, PartialEq, Eq)]
pub struct Board {
    pub id: i64,
    pub name: String,
    pub task_count: i64,
    #[serde(default)]
    pub shared_role: BoardRole,
    #[serde(default)]
    pub is_shared: bool,
    #[serde(default)]
    pub sync_status: SyncStatus,
    #[serde(default)]
    pub sync_revision: i64,
}

#[derive(Deserialize, Serialize, Clone, Debug, PartialEq, Eq)]
pub struct Note {
    pub id: i64,
    pub title: String,
    pub content: String,
    pub pinned: bool,
    pub created_at: i64,
    pub updated_at: i64,
}

fn default_id() -> i64 {
    -1
}

// Numeric IDs cross the Svelte boundary, so stay within JavaScript's exact integer range.
fn fresh_id(exists: impl Fn(i64) -> bool) -> i64 {
    loop {
        let bytes = iroh::SecretKey::generate().to_bytes();
        let id = (u64::from_le_bytes(bytes[..8].try_into().unwrap()) & ((1_u64 << 53) - 1)) as i64;
        if id != 0 && !exists(id) {
            return id;
        }
    }
}

#[derive(Deserialize, Serialize, Clone, Debug, PartialEq, Eq)]
pub struct TaskItem {
    pub id: String,
    #[serde(default)]
    pub text: String,
    #[serde(default)]
    pub completed: bool,
}

#[derive(Deserialize, Serialize, Clone, Copy, Debug, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum RecurrenceFrequency {
    Daily,
    Weekly,
    Monthly,
}

fn default_recurrence_interval() -> u32 {
    1
}

#[derive(Deserialize, Serialize, Clone, Debug, PartialEq, Eq)]
pub struct Recurrence {
    pub frequency: RecurrenceFrequency,
    #[serde(default = "default_recurrence_interval")]
    pub interval: u32,
}

/// Ordered from least to most important.
#[derive(Deserialize, Serialize, Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
#[serde(rename_all = "lowercase")]
pub enum TaskPriority {
    Low,
    Medium,
    High,
    Urgent,
}

fn is_false(value: &bool) -> bool {
    !*value
}

#[derive(Deserialize, Serialize, Clone, Debug, PartialEq, Eq)]
pub struct Task {
    #[serde(default)]
    pub pinned: bool,
    #[serde(default = "default_id")]
    pub id: i64,
    #[serde(default)]
    pub title: String,
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub color: String,
    #[serde(default)]
    pub start_time: u128,
    #[serde(default)]
    pub due_time: Option<u128>,
    /// A start at local midnight is a date without a time unless the user
    /// chose 00:00; see `crate::task_dates`. Omitted when false.
    #[serde(default, skip_serializing_if = "is_false")]
    pub start_at_midnight: bool,
    /// Like `start_at_midnight`, for the due time.
    #[serde(default, skip_serializing_if = "is_false")]
    pub due_at_midnight: bool,
    /// Milliseconds since the Unix epoch when the task was created; `0` is unknown.
    #[serde(default)]
    pub created_time: u128,
    #[serde(default)]
    pub labels: Vec<String>,
    #[serde(default)]
    pub items: Vec<TaskItem>,
    #[serde(default)]
    pub recurrence: Option<Recurrence>,
    /// Omitted when unset so tasks without a priority keep their stored shape.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub priority: Option<TaskPriority>,
    /// When the task entered the board's done column, in milliseconds since
    /// the Unix epoch. Derived from its position by `completion::reconcile`;
    /// an archived task outside the done column completed at its archive time.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub completed_at: Option<u128>,
    /// Tasks of the same board this one waits on; see `crate::blocking`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub blocked_by: Vec<i64>,
}

#[derive(Serialize, Clone, Debug, PartialEq, Eq)]
pub struct TaskSummary {
    pub id: i64,
    pub title: String,
    pub color: String,
    pub start_time: u128,
    pub due_time: Option<u128>,
    #[serde(skip_serializing_if = "is_false")]
    pub start_at_midnight: bool,
    #[serde(skip_serializing_if = "is_false")]
    pub due_at_midnight: bool,
    pub created_time: u128,
    pub labels: Vec<String>,
    pub recurrence: Option<Recurrence>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub priority: Option<TaskPriority>,
}

impl From<&Task> for TaskSummary {
    fn from(task: &Task) -> Self {
        Self {
            id: task.id,
            title: task.title.clone(),
            color: task.color.clone(),
            start_time: task.start_time,
            due_time: task.due_time,
            start_at_midnight: task.start_at_midnight,
            due_at_midnight: task.due_at_midnight,
            created_time: task.created_time,
            labels: task.labels.clone(),
            recurrence: task.recurrence.clone(),
            priority: task.priority,
        }
    }
}

impl Default for Task {
    fn default() -> Self {
        Self {
            pinned: false,
            id: -1,
            title: String::new(),
            description: String::new(),
            color: String::new(),
            start_time: 0,
            due_time: None,
            start_at_midnight: false,
            due_at_midnight: false,
            created_time: 0,
            labels: Vec::new(),
            items: Vec::new(),
            recurrence: None,
            priority: None,
            completed_at: None,
            blocked_by: Vec::new(),
        }
    }
}

#[derive(Deserialize, Serialize, Clone, Debug, PartialEq, Eq)]
pub struct TaskTemplate {
    #[serde(default = "default_id")]
    pub id: i64,
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub task: Task,
}

/// The sort understood by older peers and plugins; see `Column::sort_rules`.
#[derive(Deserialize, Serialize, Clone, Debug, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
#[derive(Default)]
pub enum ColumnSort {
    #[default]
    Custom,
    DueDateAsc,
    DueDateDesc,
}

pub const MAX_SORT_RULES: usize = 3;
pub const SORT_FIELD_DUE_TIME: &str = "due_time";
/// Fields this version can sort by. Rules are stored by name so that fields
/// added by newer versions survive a round trip through this one.
pub const SORT_FIELDS: [&str; 7] = [
    SORT_FIELD_DUE_TIME,
    "start_time",
    "created_time",
    "title",
    "checklist_progress",
    "priority",
    "dependency",
];

#[derive(Deserialize, Serialize, Clone, Copy, Debug, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum SortDirection {
    Asc,
    Desc,
}

#[derive(Deserialize, Serialize, Clone, Debug, PartialEq, Eq)]
pub struct SortRule {
    pub field: String,
    pub direction: SortDirection,
}

/// Whether rules can be saved: at most three known, distinct fields.
pub fn valid_sort_rules(rules: &[SortRule]) -> bool {
    let mut seen = std::collections::HashSet::new();
    rules.len() <= MAX_SORT_RULES
        && rules.iter().all(|rule| {
            SORT_FIELDS.contains(&rule.field.as_str()) && seen.insert(rule.field.as_str())
        })
}

impl ColumnSort {
    fn from_rules(rules: &[SortRule]) -> Self {
        match rules {
            [rule] if rule.field == SORT_FIELD_DUE_TIME => match rule.direction {
                SortDirection::Asc => Self::DueDateAsc,
                SortDirection::Desc => Self::DueDateDesc,
            },
            _ => Self::Custom,
        }
    }

    fn rules(&self) -> Vec<SortRule> {
        let direction = match self {
            Self::Custom => return Vec::new(),
            Self::DueDateAsc => SortDirection::Asc,
            Self::DueDateDesc => SortDirection::Desc,
        };
        vec![SortRule {
            field: SORT_FIELD_DUE_TIME.into(),
            direction,
        }]
    }
}

#[derive(Deserialize, Serialize, Clone, Debug, PartialEq, Eq)]
pub struct Column {
    #[serde(default = "default_id")]
    pub id: i64,
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub color: String,
    /// Derived from `sort_rules` for older peers and plugins.
    #[serde(default)]
    pub sort_order: ColumnSort,
    /// Display order only: tasks keep their manual order in storage.
    #[serde(default)]
    pub sort_rules: Vec<SortRule>,
    /// Tasks in the board's done column are completed. Only the first flagged
    /// column counts; see `StoredData::done_column_id`. Omitted when false.
    #[serde(default, skip_serializing_if = "is_false")]
    pub done: bool,
    #[serde(default)]
    pub tasks: Vec<Task>,
}

impl Column {
    pub fn set_sort_rules(&mut self, rules: Vec<SortRule>) {
        self.sort_order = ColumnSort::from_rules(&rules);
        self.sort_rules = rules;
    }

    /// Applies a legacy sort chosen by a plugin. Rules that already present
    /// this legacy sort are kept.
    pub fn set_legacy_sort_order(&mut self, sort_order: ColumnSort) {
        if ColumnSort::from_rules(&self.sort_rules) != sort_order {
            self.set_sort_rules(sort_order.rules());
        }
    }

    /// Older apps and peers write only `sort_order`. When it disagrees with
    /// the rules, it is the newer choice and replaces them.
    pub fn reconcile_sort(&mut self) {
        let sort_order = self.sort_order.clone();
        self.set_legacy_sort_order(sort_order);
    }
}

#[derive(Deserialize, Serialize, Clone, Debug, PartialEq, Eq)]
pub struct Archive {
    #[serde(default)]
    pub time: u128,
    #[serde(default)]
    pub task: Task,
}

#[derive(Deserialize, Serialize, Clone, Debug, PartialEq, Eq)]
pub struct IrohNetworkSettings {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default = "default_true")]
    pub direct_ip_enabled: bool,
    #[serde(default = "default_true")]
    pub discovery_enabled: bool,
    #[serde(default)]
    pub discovery_urls: Vec<String>,
    #[serde(default = "default_true")]
    pub relay_enabled: bool,
    #[serde(default)]
    pub relay_urls: Vec<String>,
    #[serde(default)]
    pub direct_addresses: Vec<String>,
    #[serde(default)]
    pub listen_port: u16,
}

impl Default for IrohNetworkSettings {
    fn default() -> Self {
        Self {
            enabled: false,
            direct_ip_enabled: true,
            discovery_enabled: true,
            discovery_urls: Vec::new(),
            relay_enabled: true,
            relay_urls: Vec::new(),
            direct_addresses: Vec::new(),
            listen_port: 0,
        }
    }
}

#[derive(Deserialize, Serialize, Clone, Debug, PartialEq, Eq)]
pub struct Settings {
    #[serde(default)]
    pub theme: Option<String>,
    #[serde(default)]
    pub language: LanguagePreference,
    #[serde(default)]
    pub notify_enabled: bool,
    /// Minutes before a due time to send a reminder; `0` sends none.
    #[serde(default)]
    pub notify_reminder_minutes: u32,
    #[serde(default = "default_true")]
    pub global_shortcuts_enabled: bool,
    /// Fallback for boards without their own choice; older versions kept one
    /// mode for every board.
    #[serde(default = "default_true")]
    pub task_expand_mode: bool,
    /// Device-local task detail visibility, keyed by board ID.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub board_task_expand_modes: BTreeMap<i64, bool>,
    /// Device-local calendar layouts that show every task instead of "+N",
    /// keyed by board ID.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub board_calendar_expand_all: BTreeMap<i64, bool>,
    /// Device-local columns whose tasks never send due notifications, keyed
    /// by board ID.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub notify_muted_columns: BTreeMap<i64, BTreeSet<i64>>,
    #[serde(default)]
    pub global_shortcuts: GlobalShortcutSettings,
    #[serde(default)]
    pub iroh_network: IrohNetworkSettings,
}

/// Device-local key bindings. `None` uses the action's default and an empty
/// string turns the shortcut off.
#[derive(Deserialize, Serialize, Clone, Debug, Default, PartialEq, Eq)]
pub struct GlobalShortcutSettings {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub quick_task: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub quick_note: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub floating_tasks: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub show_cardbe: Option<String>,
}

#[derive(Deserialize, Serialize, Clone, Copy, Debug, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ShortcutAction {
    QuickTask,
    QuickNote,
    FloatingTasks,
    ShowCardbe,
}

impl ShortcutAction {
    pub const ALL: [Self; 4] = [
        Self::QuickTask,
        Self::QuickNote,
        Self::FloatingTasks,
        Self::ShowCardbe,
    ];

    pub fn default_shortcut(self) -> &'static str {
        match self {
            Self::QuickTask => "Ctrl+Alt+T",
            Self::QuickNote => "Ctrl+Alt+N",
            Self::FloatingTasks => "Ctrl+Alt+F",
            Self::ShowCardbe => "Ctrl+Alt+C",
        }
    }

    pub fn setting(self, settings: &GlobalShortcutSettings) -> Option<&str> {
        match self {
            Self::QuickTask => settings.quick_task.as_deref(),
            Self::QuickNote => settings.quick_note.as_deref(),
            Self::FloatingTasks => settings.floating_tasks.as_deref(),
            Self::ShowCardbe => settings.show_cardbe.as_deref(),
        }
    }

    pub fn setting_mut(self, settings: &mut GlobalShortcutSettings) -> &mut Option<String> {
        match self {
            Self::QuickTask => &mut settings.quick_task,
            Self::QuickNote => &mut settings.quick_note,
            Self::FloatingTasks => &mut settings.floating_tasks,
            Self::ShowCardbe => &mut settings.show_cardbe,
        }
    }
}

fn default_true() -> bool {
    true
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            theme: None,
            language: LanguagePreference::default(),
            notify_enabled: false,
            notify_reminder_minutes: 0,
            global_shortcuts_enabled: true,
            task_expand_mode: true,
            board_task_expand_modes: BTreeMap::new(),
            board_calendar_expand_all: BTreeMap::new(),
            notify_muted_columns: BTreeMap::new(),
            global_shortcuts: GlobalShortcutSettings::default(),
            iroh_network: IrohNetworkSettings::default(),
        }
    }
}

#[derive(Deserialize, Serialize, Clone, Debug, Default, PartialEq, Eq)]
pub enum LanguagePreference {
    #[default]
    #[serde(rename = "system")]
    System,
    #[serde(rename = "en")]
    English,
    #[serde(rename = "zh-TW")]
    TraditionalChinese,
}

#[derive(Deserialize, Serialize, Clone, Debug, PartialEq, Eq)]
pub struct StoredData {
    #[serde(default)]
    pub schema_version: u32,
    #[serde(default)]
    pub columns: Vec<Column>,
    #[serde(default)]
    pub archives: Vec<Archive>,
    #[serde(default)]
    pub templates: Vec<TaskTemplate>,
    #[serde(default)]
    pub settings: Settings,
    /// Labels in most-recently-used order. Labels themselves are still derived
    /// from active tasks, so removing a label from every task removes it here.
    #[serde(default)]
    pub label_recency: Vec<String>,
    #[serde(default)]
    pub next_column_id: i64,
    #[serde(default)]
    pub next_task_id: i64,
    #[serde(default)]
    pub next_template_id: i64,
}

impl Default for StoredData {
    fn default() -> Self {
        Self {
            schema_version: CURRENT_SCHEMA_VERSION,
            columns: Vec::new(),
            archives: Vec::new(),
            templates: Vec::new(),
            settings: Settings::default(),
            label_recency: Vec::new(),
            next_column_id: 0,
            next_task_id: 0,
            next_template_id: 0,
        }
    }
}

impl StoredData {
    /// Stored task order is the manual order. Sort rules only change how a
    /// column is displayed, so storage just keeps pinned tasks first.
    /// The done column. Concurrent edits may flag several; the first one counts.
    pub fn done_column_id(&self) -> Option<i64> {
        self.columns
            .iter()
            .find(|column| column.done)
            .map(|column| column.id)
    }

    pub fn keep_pinned_tasks_first(&mut self) {
        for column in &mut self.columns {
            column
                .tasks
                .sort_by_key(|task| std::cmp::Reverse(task.pinned));
        }
    }

    pub fn migrate(mut self) -> Result<(Self, bool), String> {
        if self.schema_version > CURRENT_SCHEMA_VERSION {
            return Err(format!(
                "Data schema version {} is newer than this application supports ({}).",
                self.schema_version, CURRENT_SCHEMA_VERSION
            ));
        }

        let version_changed = self.schema_version != CURRENT_SCHEMA_VERSION;
        for column in &mut self.columns {
            column.reconcile_sort();
        }
        let column_ids_changed = self.repair_column_ids();
        let task_ids_changed = self.repair_task_ids();
        let template_ids_changed = self.repair_template_ids();
        let label_recency_changed = self.normalize_label_recency();
        let changed = version_changed
            || column_ids_changed
            || task_ids_changed
            || template_ids_changed
            || label_recency_changed;
        self.schema_version = CURRENT_SCHEMA_VERSION;
        Ok((self, changed))
    }

    pub fn touch_label(&mut self, label: String) {
        self.label_recency.retain(|candidate| candidate != &label);
        self.label_recency.insert(0, label);
    }

    fn normalize_label_recency(&mut self) -> bool {
        let previous = self.label_recency.clone();
        let active_labels = self
            .columns
            .iter()
            .flat_map(|column| column.tasks.iter())
            .flat_map(|task| task.labels.iter())
            .collect::<std::collections::HashSet<_>>();
        let mut seen = std::collections::HashSet::new();
        self.label_recency
            .retain(|label| active_labels.contains(label) && seen.insert(label.clone()));
        self.label_recency != previous
    }

    pub fn repair_column_ids(&mut self) -> bool {
        use std::collections::HashSet;

        let existing_ids = self
            .columns
            .iter()
            .filter_map(|column| (column.id >= 0).then_some(column.id))
            .collect::<HashSet<_>>();
        let mut next_id = existing_ids
            .iter()
            .max()
            .map_or(0, |id| id.saturating_add(1))
            .max(self.next_column_id.max(0));
        let mut seen = HashSet::new();
        let mut changed = false;

        for column in &mut self.columns {
            if column.id >= 0 && seen.insert(column.id) {
                continue;
            }
            while existing_ids.contains(&next_id) || seen.contains(&next_id) {
                next_id = next_id.saturating_add(1);
            }
            column.id = next_id;
            seen.insert(next_id);
            next_id = next_id.saturating_add(1);
            changed = true;
        }

        if self.next_column_id != next_id {
            self.next_column_id = next_id;
            changed = true;
        }
        changed
    }

    pub fn repair_task_ids(&mut self) -> bool {
        use std::collections::HashSet;

        let existing_ids = self
            .columns
            .iter()
            .flat_map(|column| column.tasks.iter())
            .chain(self.archives.iter().map(|archive| &archive.task))
            .filter_map(|task| (task.id >= 0).then_some(task.id))
            .collect::<HashSet<_>>();
        let mut next_id = existing_ids
            .iter()
            .max()
            .map_or(0, |id| id.saturating_add(1))
            .max(self.next_task_id.max(0));
        let mut seen = HashSet::new();
        let mut changed = false;

        for task in self
            .columns
            .iter_mut()
            .flat_map(|column| column.tasks.iter_mut())
            .chain(self.archives.iter_mut().map(|archive| &mut archive.task))
        {
            if task.id >= 0 && seen.insert(task.id) {
                continue;
            }

            while existing_ids.contains(&next_id) || seen.contains(&next_id) {
                next_id = next_id.saturating_add(1);
            }
            task.id = next_id;
            seen.insert(next_id);
            next_id = next_id.saturating_add(1);
            changed = true;
        }

        if self.next_task_id != next_id {
            self.next_task_id = next_id;
            changed = true;
        }
        changed
    }

    pub fn allocate_task_id(&mut self) -> Result<i64, String> {
        Ok(fresh_id(|id| {
            self.columns
                .iter()
                .flat_map(|column| &column.tasks)
                .any(|task| task.id == id)
                || self.archives.iter().any(|archive| archive.task.id == id)
        }))
    }

    pub fn allocate_column_id(&mut self) -> Result<i64, String> {
        Ok(fresh_id(|id| {
            self.columns.iter().any(|column| column.id == id)
        }))
    }

    pub fn repair_template_ids(&mut self) -> bool {
        use std::collections::HashSet;

        let existing_ids = self
            .templates
            .iter()
            .filter_map(|template| (template.id >= 0).then_some(template.id))
            .collect::<HashSet<_>>();
        let mut next_id = existing_ids
            .iter()
            .max()
            .map_or(0, |id| id.saturating_add(1))
            .max(self.next_template_id.max(0));
        let mut seen = HashSet::new();
        let mut changed = false;

        for template in &mut self.templates {
            if template.id >= 0 && seen.insert(template.id) {
                continue;
            }
            while existing_ids.contains(&next_id) || seen.contains(&next_id) {
                next_id = next_id.saturating_add(1);
            }
            template.id = next_id;
            seen.insert(next_id);
            next_id = next_id.saturating_add(1);
            changed = true;
        }

        if self.next_template_id != next_id {
            self.next_template_id = next_id;
            changed = true;
        }
        changed
    }

    pub fn allocate_template_id(&mut self) -> Result<i64, String> {
        Ok(fresh_id(|id| {
            self.templates.iter().any(|template| template.id == id)
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn new_ids_do_not_reuse_a_local_id_or_exceed_js_safe_integer() {
        let first = fresh_id(|_| false);
        let second = fresh_id(|id| id == first);
        assert_ne!(first, second);
        assert!((1..=9_007_199_254_740_991).contains(&first));
        assert!((1..=9_007_199_254_740_991).contains(&second));
    }

    #[test]
    fn old_settings_keep_global_shortcuts_enabled() {
        let settings: Settings = serde_json::from_str(r#"{"notify_enabled":true}"#).unwrap();

        assert!(settings.notify_enabled);
        assert!(settings.global_shortcuts_enabled);
        assert!(settings.task_expand_mode);
        assert!(settings.board_calendar_expand_all.is_empty());
    }

    #[test]
    fn calendar_expand_all_roundtrips() {
        let settings: Settings =
            serde_json::from_str(r#"{"board_calendar_expand_all":{"7":true}}"#).unwrap();
        assert_eq!(settings.board_calendar_expand_all.get(&7), Some(&true));
        let restored: Settings =
            serde_json::from_str(&serde_json::to_string(&settings).unwrap()).unwrap();
        assert_eq!(restored, settings);
    }

    #[test]
    fn task_expand_mode_roundtrips() {
        let settings: Settings = serde_json::from_str(
            r#"{"task_expand_mode":false,"board_task_expand_modes":{"7":true}}"#,
        )
        .unwrap();
        assert!(!settings.task_expand_mode);
        assert_eq!(settings.board_task_expand_modes.get(&7), Some(&true));
        let restored: Settings =
            serde_json::from_str(&serde_json::to_string(&settings).unwrap()).unwrap();
        assert_eq!(restored, settings);
    }

    #[test]
    fn old_document_defaults_settings_and_preserves_valid_ids() {
        let json = r#"{
            "columns": [{"name":"Todo","color":"","tasks":[
                {"id":-1,"title":"New"},
                {"id":5,"title":"Existing"}
            ]}],
            "archives": []
        }"#;
        let old: StoredData = serde_json::from_str(json).unwrap();
        let (migrated, changed) = old.migrate().unwrap();

        assert!(changed);
        assert!(!migrated.settings.notify_enabled);
        assert!(migrated.settings.global_shortcuts_enabled);
        assert_eq!(migrated.columns[0].id, 0);
        assert_eq!(migrated.columns[0].tasks[1].id, 5);
        assert!(migrated.columns[0].tasks[0].id > 5);
        assert_eq!(migrated.schema_version, CURRENT_SCHEMA_VERSION);
        assert!(migrated.columns[0].tasks[0].items.is_empty());
        assert!(migrated.columns[0].tasks[0].recurrence.is_none());
        assert!(!migrated.columns[0].tasks[0].pinned);
    }

    #[test]
    fn sorted_columns_keep_manual_order_with_pinned_tasks_first() {
        let mut stored = StoredData::default();
        let mut column = Column {
            id: 1,
            name: "Todo".into(),
            color: String::new(),
            sort_order: ColumnSort::Custom,
            sort_rules: Vec::new(),
            done: false,
            tasks: (1..=3)
                .map(|id| Task {
                    id,
                    due_time: Some(40 - id as u128),
                    ..Task::default()
                })
                .collect(),
        };
        column.set_sort_rules(vec![rule("due_time", SortDirection::Asc)]);
        column.tasks[2].pinned = true;
        stored.columns.push(column);

        stored.keep_pinned_tasks_first();
        assert_eq!(
            stored.columns[0]
                .tasks
                .iter()
                .map(|task| task.id)
                .collect::<Vec<_>>(),
            vec![3, 1, 2]
        );
    }

    fn rule(field: &str, direction: SortDirection) -> SortRule {
        SortRule {
            field: field.into(),
            direction,
        }
    }

    fn empty_column(sort_rules: Vec<SortRule>) -> Column {
        Column {
            id: 1,
            name: String::new(),
            color: String::new(),
            sort_order: ColumnSort::Custom,
            sort_rules,
            done: false,
            tasks: Vec::new(),
        }
    }

    #[test]
    fn sort_rules_derive_the_legacy_sort_order() {
        let mut column = empty_column(Vec::new());
        column.set_sort_rules(vec![rule("due_time", SortDirection::Desc)]);
        assert_eq!(column.sort_order, ColumnSort::DueDateDesc);
        column.set_sort_rules(vec![
            rule("due_time", SortDirection::Asc),
            rule("title", SortDirection::Asc),
        ]);
        assert_eq!(column.sort_order, ColumnSort::Custom);
        column.set_sort_rules(vec![rule("start_time", SortDirection::Asc)]);
        assert_eq!(column.sort_order, ColumnSort::Custom);
    }

    #[test]
    fn legacy_sort_order_replaces_only_disagreeing_rules() {
        let rules = vec![rule("start_time", SortDirection::Asc)];
        let mut column = empty_column(rules.clone());
        column.reconcile_sort();
        assert_eq!(column.sort_rules, rules);

        // An older peer chose a due date sort.
        column.sort_order = ColumnSort::DueDateAsc;
        column.reconcile_sort();
        assert_eq!(
            column.sort_rules,
            vec![rule("due_time", SortDirection::Asc)]
        );

        // Data written before sort rules existed.
        column.sort_rules = Vec::new();
        column.sort_order = ColumnSort::DueDateDesc;
        column.reconcile_sort();
        assert_eq!(
            column.sort_rules,
            vec![rule("due_time", SortDirection::Desc)]
        );

        column.set_legacy_sort_order(ColumnSort::Custom);
        assert!(column.sort_rules.is_empty());
    }

    #[test]
    fn sort_rules_are_validated_and_unknown_fields_round_trip() {
        assert!(valid_sort_rules(&[]));
        assert!(valid_sort_rules(&[
            rule("due_time", SortDirection::Asc),
            rule("title", SortDirection::Desc),
            rule("checklist_progress", SortDirection::Asc),
        ]));
        assert!(!valid_sort_rules(&[
            rule("due_time", SortDirection::Asc),
            rule("due_time", SortDirection::Desc),
        ]));
        assert!(valid_sort_rules(&[rule("priority", SortDirection::Desc)]));
        assert!(valid_sort_rules(&[rule("dependency", SortDirection::Asc)]));
        assert!(!valid_sort_rules(&[rule("effort", SortDirection::Asc)]));
        assert!(!valid_sort_rules(&[
            rule("due_time", SortDirection::Asc),
            rule("start_time", SortDirection::Asc),
            rule("created_time", SortDirection::Asc),
            rule("title", SortDirection::Asc),
        ]));

        let json = r#"[{"field":"effort","direction":"desc"}]"#;
        let newer: Vec<SortRule> = serde_json::from_str(json).unwrap();
        assert_eq!(serde_json::to_string(&newer).unwrap(), json);
    }
}

#[derive(Serialize, Deserialize)]
pub struct ExportData {
    pub columns: Vec<Column>,
    pub archives: Vec<Archive>,
    #[serde(default)]
    pub templates: Vec<TaskTemplate>,
    #[serde(default)]
    pub labels: Vec<String>,
    #[serde(default)]
    pub label_recency: Vec<String>,
}

/// Portable single-board backup. The flattened task fields remain compatible
/// with older backups and import previews; sharing still uses `ExportData`.
#[derive(Serialize, Deserialize)]
pub struct SingleBoardExport {
    #[serde(flatten)]
    pub data: ExportData,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub flow: Option<crate::flow::FlowData>,
    /// Only notes referenced by this board's Flow canvases, using backup IDs.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub notes: Vec<Note>,
}

/// Complete, portable backup. Unlike `ExportData`, this contains every board
/// plus global data and is deliberately versioned for future migrations.
#[derive(Serialize, Deserialize)]
pub struct AllBoardsExport {
    pub schema_version: u32,
    pub boards: Vec<AllBoardsExportBoard>,
    pub active_board_index: usize,
    pub settings: Settings,
    pub notes: Vec<Note>,
    /// Board filters hold positions in `boards`. Absent in older backups,
    /// which leave the current views in place.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub focuses: Option<Vec<crate::focuses::Focus>>,
}

#[derive(Serialize, Deserialize)]
pub struct AllBoardsExportBoard {
    #[serde(default)]
    pub flow: crate::flow::FlowData,
    pub name: String,
    pub data: ExportData,
}

#[cfg(test)]
mod language_tests {
    use super::{LanguagePreference, Settings};

    #[test]
    fn old_settings_default_to_system_language() {
        let settings: Settings =
            serde_json::from_str(r#"{"notify_enabled":true,"global_shortcuts_enabled":false}"#)
                .unwrap();
        assert_eq!(settings.language, LanguagePreference::System);
        assert!(settings.notify_enabled);
        assert!(!settings.global_shortcuts_enabled);
    }

    #[test]
    fn language_preferences_roundtrip_and_reject_unsupported_values() {
        for value in ["system", "en", "zh-TW"] {
            let json = format!("\"{value}\"");
            let preference: LanguagePreference = serde_json::from_str(&json).unwrap();
            assert_eq!(serde_json::to_string(&preference).unwrap(), json);
        }
        assert!(serde_json::from_str::<LanguagePreference>(r#""ja""#).is_err());
    }
}

#[cfg(test)]
mod task_priority_tests {
    use super::{Task, TaskPriority};

    #[test]
    fn priority_is_optional_ordered_and_omitted_when_unset() {
        let task: Task = serde_json::from_str(r#"{"title":"Old"}"#).unwrap();
        assert_eq!(task.priority, None);
        assert!(!serde_json::to_string(&task).unwrap().contains("priority"));

        let task: Task = serde_json::from_str(r#"{"title":"New","priority":"urgent"}"#).unwrap();
        assert_eq!(task.priority, Some(TaskPriority::Urgent));
        assert!(serde_json::to_string(&task)
            .unwrap()
            .contains(r#""priority":"urgent""#));
        assert!(serde_json::from_str::<Task>(r#"{"priority":"critical"}"#).is_err());
        assert!(TaskPriority::Low < TaskPriority::Medium);
        assert!(TaskPriority::High < TaskPriority::Urgent);
    }
}
