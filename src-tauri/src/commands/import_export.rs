use crate::errors::CommandError;
use crate::{
    models::{
        AllBoardsExport, AllBoardsExportBoard, ExportData, SingleBoardExport, StoredData,
        CURRENT_SCHEMA_VERSION,
    },
    state::{
        lock_app, read_with_archives_for_board, update_stored_with_pre_import_snapshot_for_board,
        SharedAppData,
    },
};
use tauri::State;

#[tauri::command]
pub fn export_data(
    state: State<'_, SharedAppData>,
    expected_board_id: i64,
) -> Result<String, CommandError> {
    let export = read_with_archives_for_board(&state, expected_board_id, |guard| {
        guard.database.read_single_board_backup(expected_board_id)
    })?
    .map_err(CommandError::repository)?;
    serde_json::to_string(&export).map_err(CommandError::internal)
}

fn parse_single_board_backup(json_data: &str) -> Result<SingleBoardExport, CommandError> {
    let backup: SingleBoardExport =
        serde_json::from_str(json_data).map_err(|_| CommandError::InvalidImport)?;
    if let Some(flow) = &backup.flow {
        // Legacy task-only imports can repair IDs. A canvas refers to those
        // IDs, so ambiguous or missing identities cannot be repaired safely.
        let mut task_ids = std::collections::HashSet::new();
        if backup
            .data
            .columns
            .iter()
            .flat_map(|column| &column.tasks)
            .chain(backup.data.archives.iter().map(|archive| &archive.task))
            .any(|task| task.id < 0 || !task_ids.insert(task.id))
        {
            return Err(CommandError::InvalidImport);
        }
        crate::flow::validate_data(flow, &backup.data).map_err(|_| CommandError::InvalidImport)?;
        crate::storage::validate_single_board_notes(flow, &backup.notes)
            .map_err(|_| CommandError::InvalidImport)?;
    } else if !backup.notes.is_empty() {
        return Err(CommandError::InvalidImport);
    }
    Ok(backup)
}

#[tauri::command]
pub fn export_all_boards(state: State<'_, SharedAppData>) -> Result<String, CommandError> {
    let guard = lock_app(&state)?;
    let active_board_id = guard.active_board_id;
    let boards = guard
        .boards
        .clone()
        .into_iter()
        .map(|board| {
            let data = guard
                .database
                .read_board(board.id)
                .map_err(CommandError::repository)?;
            Ok(AllBoardsExportBoard {
                flow: guard
                    .database
                    .read_flow_backup(board.id)
                    .map_err(CommandError::repository)?,
                name: board.name,
                data: ExportData {
                    columns: data.columns,
                    archives: guard
                        .database
                        .load_board_archives_for_export(board.id)
                        .map_err(CommandError::repository)?,
                    templates: data.templates,
                    labels: Vec::new(),
                    label_recency: data.label_recency,
                },
            })
        })
        .collect::<Result<Vec<_>, CommandError>>()?;
    let notes = guard
        .database
        .get_notes()
        .map_err(CommandError::repository)?;
    let active_board_index = guard
        .boards
        .iter()
        .position(|board| board.id == active_board_id)
        .unwrap_or(0);
    let mut focuses =
        crate::focuses::load(&guard.database.connection).map_err(CommandError::from)?;
    if let Some(views) = &mut focuses {
        crate::focuses::map_boards(views, |id| {
            guard
                .boards
                .iter()
                .position(|board| board.id == id)
                .map(|index| index as i64)
        });
    }
    serde_json::to_string(&AllBoardsExport {
        schema_version: 1,
        boards,
        active_board_index,
        settings: guard.stored.settings.clone(),
        notes,
        focuses,
    })
    .map_err(CommandError::internal)
}

fn parse_all_boards_backup(json_data: &str) -> Result<AllBoardsExport, CommandError> {
    let backup: AllBoardsExport =
        serde_json::from_str(json_data).map_err(|_| CommandError::InvalidImport)?;
    if backup.schema_version != 1 {
        return Err(CommandError::UnsupportedBackupVersion);
    }
    if backup.boards.is_empty() || backup.active_board_index >= backup.boards.len() {
        return Err(CommandError::InvalidImport);
    }
    for board in &backup.boards {
        crate::flow::validate_data(&board.flow, &board.data)
            .map_err(|_| CommandError::InvalidImport)?;
        super::boards::board_name(board.name.clone())
            .map_err(|_| CommandError::BoardNameInvalid)?;
    }
    let mut note_ids = std::collections::HashSet::new();
    if backup.notes.iter().any(|note| !note_ids.insert(note.id)) {
        return Err(CommandError::InvalidImport);
    }
    if let Some(views) = &backup.focuses {
        let board_count = backup.boards.len() as i64;
        let positions_valid = views.iter().all(|view| {
            view.filter
                .board_ids
                .iter()
                .flatten()
                .all(|index| (0..board_count).contains(index))
        });
        if crate::focuses::validate(views).is_err() || !positions_valid {
            return Err(CommandError::InvalidImport);
        }
    }
    Ok(backup)
}

#[tauri::command]
pub fn validate_all_boards_backup(json_data: String) -> Result<(), CommandError> {
    parse_all_boards_backup(&json_data).map(|_| ())
}

#[tauri::command]
pub fn import_all_boards(
    state: State<'_, SharedAppData>,
    app_handle: tauri::AppHandle,
    json_data: String,
) -> Result<(), CommandError> {
    let backup = parse_all_boards_backup(&json_data)?;
    let mut prepared = Vec::with_capacity(backup.boards.len());
    for board in backup.boards {
        let name = super::boards::board_name(board.name)?;
        let mut data = StoredData::default();
        data.columns = board.data.columns;
        data.archives = board.data.archives;
        data.templates = board.data.templates;
        data.label_recency = board.data.label_recency;
        data.schema_version = CURRENT_SCHEMA_VERSION;
        data.repair_column_ids();
        data.repair_task_ids();
        data.repair_template_ids();
        crate::flow::validate_data(
            &board.flow,
            &crate::models::ExportData {
                columns: data.columns.clone(),
                archives: data.archives.clone(),
                templates: vec![],
                labels: vec![],
                label_recency: vec![],
            },
        )
        .map_err(|_| CommandError::InvalidImport)?;
        prepared.push((name.to_string(), data, board.flow));
    }
    let mut guard = lock_app(&state)?;
    let (boards, stored) = guard
        .database
        .replace_all_boards(
            &prepared,
            backup.active_board_index,
            &backup.notes,
            &backup.settings,
            backup.focuses.as_deref(),
        )
        .map_err(CommandError::repository)?;
    guard.boards = boards;
    guard.active_board_id = guard.boards[backup.active_board_index].id;
    guard.archives_loaded = true;
    guard.replace_board_contents(stored);
    drop(guard);
    tauri::async_runtime::spawn(async move {
        use tauri::Manager;
        let network = app_handle.state::<crate::commands::iroh_share::IrohShareState>();
        if let Err(error) = crate::commands::iroh_share::stop_host_if_idle(&network).await {
            log::warn!(target: "iroh", "Could not stop board sharing after importing data: {error}");
        }
    });
    Ok(())
}

#[tauri::command]
pub fn import_data(
    state: State<'_, SharedAppData>,
    json_data: String,
    expected_board_id: i64,
) -> Result<String, CommandError> {
    let backup = parse_single_board_backup(&json_data)?;
    let import = backup.data;

    let (_, snapshot_path) = update_stored_with_pre_import_snapshot_for_board(
        &state,
        expected_board_id,
        backup.flow.as_ref(),
        &backup.notes,
        |data| {
            data.columns = import.columns;
            data.archives = import.archives;
            data.templates = import.templates;
            data.label_recency = import.label_recency;
            data.next_column_id = 0;
            data.next_task_id = 0;
            data.next_template_id = 0;
            data.repair_column_ids();
            data.repair_task_ids();
            data.repair_template_ids();
            Ok(())
        },
    )?;

    Ok(snapshot_path.display().to_string())
}

#[tauri::command]
pub fn import_board_as_new(
    state: State<'_, SharedAppData>,
    json_data: String,
    name: String,
) -> Result<crate::models::Board, CommandError> {
    let backup = parse_single_board_backup(&json_data)?;
    let import = backup.data;
    let name = super::boards::board_name(name).map_err(|_| CommandError::BoardNameInvalid)?;
    let mut data = StoredData {
        columns: import.columns,
        archives: import.archives,
        templates: import.templates,
        label_recency: import.label_recency,
        schema_version: CURRENT_SCHEMA_VERSION,
        ..StoredData::default()
    };
    data.repair_column_ids();
    data.repair_task_ids();
    data.repair_template_ids();

    let mut guard = lock_app(&state)?;
    let board = guard
        .database
        .create_board_with_backup(&name, &data, backup.flow.as_ref(), &backup.notes)
        .map_err(CommandError::repository)?;
    guard.boards.push(board.clone());
    guard
        .load_active_board(board.id)
        .map_err(CommandError::repository)?;
    Ok(board)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn single_board_backup_accepts_legacy_fields_and_keeps_preview_shape() {
        let legacy = r#"{"columns":[],"archives":[],"labels":["工作"]}"#;
        let mut backup = parse_single_board_backup(legacy).unwrap();
        assert!(backup.flow.is_none());
        assert!(backup.notes.is_empty());
        backup.flow = Some(Default::default());
        let json = serde_json::to_value(&backup).unwrap();
        assert!(json["columns"].is_array());
        assert!(json["archives"].is_array());
        assert!(json.get("data").is_none());
        assert!(parse_single_board_backup(&json.to_string())
            .unwrap()
            .flow
            .is_some());
        // Existing readers continue to see the board fields.
        assert_eq!(
            serde_json::from_value::<ExportData>(json).unwrap().labels,
            vec!["工作"]
        );
    }

    #[test]
    fn single_board_backup_rejects_invalid_flow_and_unrelated_notes() {
        let mut json = serde_json::json!({
            "columns": [], "archives": [],
            "flow": {"layout": {"nodes": [{"task_id": 99, "x": 0, "y": 0}], "viewport": null}, "relationships": []}
        });
        assert!(matches!(
            parse_single_board_backup(&json.to_string()),
            Err(CommandError::InvalidImport)
        ));
        json["flow"]["layout"]["nodes"] = serde_json::json!([]);
        let note = serde_json::json!({"id": 1, "title": "Private", "content": "", "pinned": false, "created_at": 0, "updated_at": 0});
        json["notes"] = serde_json::json!([note]);
        assert!(parse_single_board_backup(&json.to_string()).is_err());
        json["flow"]["layout"]["annotations"] = serde_json::json!([{"id": "note", "x": 0, "y": 0, "width": 240, "kind": "note", "note_id": 1}]);
        assert!(parse_single_board_backup(&json.to_string()).is_ok());
        json["notes"] = serde_json::json!([note, note]);
        assert!(parse_single_board_backup(&json.to_string()).is_err());
        json.as_object_mut().unwrap().remove("flow");
        assert!(parse_single_board_backup(&json.to_string()).is_err());
    }

    #[test]
    fn single_board_flow_import_rejects_ambiguous_task_ids_but_legacy_can_repair_them() {
        let mut json = serde_json::json!({
            "columns": [{"tasks": [{"id": 1}, {"id": 1}]}], "archives": [],
            "flow": {"layout": {"nodes": [], "viewport": null}, "relationships": []}
        });
        assert!(parse_single_board_backup(&json.to_string()).is_err());
        json["columns"][0]["tasks"][1]["id"] = (-1).into();
        assert!(parse_single_board_backup(&json.to_string()).is_err());
        json.as_object_mut().unwrap().remove("flow");
        assert!(parse_single_board_backup(&json.to_string()).is_ok());
    }

    fn backup(name: &str) -> String {
        serde_json::to_string(&AllBoardsExport {
            schema_version: 1,
            boards: vec![AllBoardsExportBoard {
                flow: Default::default(),
                name: name.to_string(),
                data: ExportData {
                    columns: vec![],
                    archives: vec![],
                    templates: vec![],
                    labels: vec![],
                    label_recency: vec![],
                },
            }],
            active_board_index: 0,
            settings: Default::default(),
            notes: vec![],
            focuses: None,
        })
        .unwrap()
    }

    #[test]
    fn backup_preserves_existing_unicode_and_long_names() {
        for name in [
            "\u{4e2d}".repeat(67),
            "\u{4e2d}".repeat(200),
            "a".repeat(201),
            "\u{1f600}".repeat(200),
        ] {
            let parsed = parse_all_boards_backup(&backup(&name)).unwrap();
            assert_eq!(parsed.boards[0].name, name);
            assert_eq!(
                super::super::boards::board_name(name.clone()).unwrap(),
                name
            );
        }
    }

    #[test]
    fn preflight_rejects_invalid_nonempty_boards_and_context() {
        assert!(matches!(
            parse_all_boards_backup(r#"{"schema_version":1,"boards":[{}]}"#),
            Err(CommandError::InvalidImport)
        ));
        assert!(parse_all_boards_backup(&backup(" ")).is_err());
        let mut duplicate_notes: serde_json::Value =
            serde_json::from_str(&backup("Board")).unwrap();
        let note = serde_json::json!({"id":1,"title":"","content":"","pinned":false,"created_at":0,"updated_at":0});
        duplicate_notes["notes"] = serde_json::json!([note, note]);
        assert!(matches!(
            parse_all_boards_backup(&duplicate_notes.to_string()),
            Err(CommandError::InvalidImport)
        ));
        let valid = backup("Board");
        let mut value: serde_json::Value = serde_json::from_str(&valid).unwrap();
        value["active_board_index"] = 1.into();
        assert!(matches!(
            parse_all_boards_backup(&value.to_string()),
            Err(CommandError::InvalidImport)
        ));
        value["active_board_index"] = 0.into();
        // Focus board filters refer to positions in the backup.
        value["focuses"] = serde_json::json!([{"id":"a","name":"Mine","filter":{"board_ids":[0]}}]);
        assert!(parse_all_boards_backup(&value.to_string()).is_ok());
        value["focuses"][0]["filter"]["board_ids"] = serde_json::json!([1]);
        assert!(matches!(
            parse_all_boards_backup(&value.to_string()),
            Err(CommandError::InvalidImport)
        ));
        value["focuses"] = serde_json::json!([{"id":"a","name":" "}]);
        assert!(matches!(
            parse_all_boards_backup(&value.to_string()),
            Err(CommandError::InvalidImport)
        ));
        value.as_object_mut().unwrap().remove("focuses");
        value["schema_version"] = 2.into();
        assert!(matches!(
            parse_all_boards_backup(&value.to_string()),
            Err(CommandError::UnsupportedBackupVersion)
        ));
    }
}
