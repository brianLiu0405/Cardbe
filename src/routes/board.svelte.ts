import { translateCommandError } from "#lib/command-errors.js";
import { m } from "#lib/paraglide/messages.js";
import {
  applyLanguagePreference,
  type LanguagePreference,
} from "#lib/i18n/index.js";
import { logger } from "#lib/logger.js";
import { invoke } from "@tauri-apps/api/core";
import { emit } from "@tauri-apps/api/event";
import { save, open } from "@tauri-apps/plugin-dialog";
import { writeTextFile, readTextFile } from "@tauri-apps/plugin-fs";
import { toast } from "svelte-sonner";
import {
  isPermissionGranted,
  requestPermission,
} from "@tauri-apps/plugin-notification";
import {
  type Task,
  serialize_task,
  deserialize_task,
  type TaskTemplateSerialized,
} from "./type/task.svelte";
import { get_column_id } from "./type/column.svelte";
import { parse_import_summary, type ImportSummary } from "./utils/import-data";
import { addMission } from "./utils/mission.svelte";
import type { BoardSummary } from "./board-base.svelte";
import { TaskBoardStore } from "./board-tasks.svelte";
import { flow_imports } from "./components/flow/import-coordinator";

export type { AllTaskItem, BoardRole, BoardSummary } from "./board-base.svelte";

export interface ImportCandidate {
  json_data: string;
  summary: ImportSummary;
  board_name: string;
}

// ============ Board Store (class-based for Svelte 5 rune compatibility) ============

export class BoardStore extends TaskBoardStore {
  notify_enabled = $state(false);
  // Device-local detail visibility per board; boards without a saved choice
  // use the default.
  default_task_expand_mode = $state(true);
  private board_task_expand_modes = $state<Record<number, boolean>>({});
  get task_expand_mode() {
    const board_id = this.active_board_id;
    return (
      (board_id === null
        ? undefined
        : this.board_task_expand_modes[board_id]) ??
      this.default_task_expand_mode
    );
  }
  // Device-local calendar layout per board that shows every task instead of
  // "+N".
  private board_calendar_expand_all = $state<Record<number, boolean>>({});
  get calendar_expand_all() {
    const board_id = this.active_board_id;
    return (
      (board_id === null
        ? undefined
        : this.board_calendar_expand_all[board_id]) ?? false
    );
  }
  // Device-local columns that send no due notifications, keyed by board ID.
  private notify_muted_columns = $state<Record<number, number[]>>({});
  is_column_notify_muted(column_id: string) {
    const board_id = this.active_board_id;
    return (
      board_id !== null &&
      (this.notify_muted_columns[board_id]?.includes(
        get_column_id(column_id),
      ) ??
        false)
    );
  }
  notify_reminder_minutes = $state(0);
  notification_setting_updating = $state(false);
  private permission_listener_added = false;
  // Permission can be revoked in system settings while the app runs.
  private readonly handle_window_focus = () => {
    void this.check_notification_permission();
  };

  async init() {
    await this.get_boards();
    this.update_labels();
    this.get_columns();
    this.get_task_templates();

    // Load settings
    await this.load_settings();
    await this.show_recovery_messages();
    this.start_iroh_background_sync();
    if (
      !this.disposed &&
      !this.permission_listener_added &&
      typeof window !== "undefined"
    ) {
      window.addEventListener("focus", this.handle_window_focus);
      this.permission_listener_added = true;
    }

    // The backend checks due tasks; it still needs notification permission.
    if (this.notify_enabled) {
      const permission_granted = await this.request_notification_permission();
      if (!permission_granted) {
        await this.set_notify_enabled(false);
        toast.warning(m.settings_notify_permission_disabled());
      }
    }
  }

  dispose() {
    this.disposed = true;
    this.stop_iroh_background_sync();
    if (this.permission_listener_added) {
      window.removeEventListener("focus", this.handle_window_focus);
      this.permission_listener_added = false;
    }
    // Let pending user writes finish; only recurring background work is stopped.
  }

  private async load_settings() {
    try {
      const settings = await invoke<{
        notify_enabled: boolean;
        notify_reminder_minutes?: number;
        task_expand_mode?: boolean;
        board_task_expand_modes?: Record<string, boolean>;
        board_calendar_expand_all?: Record<string, boolean>;
        notify_muted_columns?: Record<string, number[]>;
        iroh_network?: { enabled: boolean };
        language?: LanguagePreference;
      }>("get_settings");
      this.notify_enabled = settings.notify_enabled;
      this.notify_reminder_minutes = settings.notify_reminder_minutes ?? 0;
      this.default_task_expand_mode = settings.task_expand_mode ?? true;
      this.board_task_expand_modes = settings.board_task_expand_modes ?? {};
      this.board_calendar_expand_all = settings.board_calendar_expand_all ?? {};
      this.notify_muted_columns = settings.notify_muted_columns ?? {};
      this.iroh_enabled = settings.iroh_network?.enabled ?? false;
      applyLanguagePreference(settings.language);
    } catch (e) {
      logger.warn("settings.load.failed", e);
    }
  }

  async set_notify_enabled(enabled: boolean) {
    if (this.notification_setting_updating) {
      return;
    }
    this.notification_setting_updating = true;

    try {
      if (enabled && !(await this.request_notification_permission())) {
        toast.error(m.settings_notify_permission_error());
        return;
      }

      const saved = await addMission(async () => {
        try {
          await invoke<void>("set_notify_enabled", { enabled });
          return true;
        } catch (e: unknown) {
          logger.error("settings.notification_save.failed", e);
          toast.error(m.settings_notify_save_error(), {
            description: translateCommandError(e),
          });
          return false;
        }
      });
      if (!saved) {
        return;
      }

      this.notify_enabled = enabled;
    } finally {
      this.notification_setting_updating = false;
    }
  }

  private async check_notification_permission() {
    if (this.disposed || !this.notify_enabled) {
      return;
    }
    let granted: boolean;
    try {
      granted = await isPermissionGranted();
    } catch (e: unknown) {
      logger.warn("notifications.permission_check.failed", e);
      return;
    }
    if (granted || this.disposed || !this.notify_enabled) {
      return;
    }
    await this.set_notify_enabled(false);
    if (!this.notify_enabled) {
      toast.warning(m.settings_notify_permission_disabled());
    }
  }

  async set_notify_reminder(minutes: number) {
    const previous = this.notify_reminder_minutes;
    this.notify_reminder_minutes = minutes;
    const saved = await addMission(async () => {
      try {
        await invoke<void>("set_notify_reminder", { minutes });
        return true;
      } catch (error) {
        logger.error("settings.notify_reminder_save.failed", error);
        toast.error(m.settings_notify_save_error(), {
          description: translateCommandError(error),
        });
        return false;
      }
    });
    if (!saved && this.notify_reminder_minutes === minutes) {
      this.notify_reminder_minutes = previous;
    }
  }

  async set_task_expand_mode(expanded: boolean) {
    const board_id = this.active_board_id;
    if (board_id === null) {
      return;
    }
    const previous = this.board_task_expand_modes[board_id];
    this.board_task_expand_modes[board_id] = expanded;
    // Switching the board-wide mode resets individually expanded tasks.
    this.task_expansion.clear(board_id);
    const saved = await addMission(async () => {
      try {
        await invoke<void>("set_task_expand_mode", {
          boardId: board_id,
          expanded,
        });
        return true;
      } catch (error) {
        logger.error("settings.task_expand_save.failed", error);
        toast.error(translateCommandError(error));
        return false;
      }
    });
    if (!saved && this.board_task_expand_modes[board_id] === expanded) {
      if (previous === undefined) {
        delete this.board_task_expand_modes[board_id];
      } else {
        this.board_task_expand_modes[board_id] = previous;
      }
    }
  }

  async set_default_task_expand_mode(expanded: boolean) {
    const previous = {
      expanded: this.default_task_expand_mode,
      boards: this.board_task_expand_modes,
    };
    this.default_task_expand_mode = expanded;
    this.board_task_expand_modes = {};
    this.task_expansion.clear_all();
    const saved = await addMission(async () => {
      try {
        await invoke<void>("set_default_task_expand_mode", { expanded });
        return true;
      } catch (error) {
        logger.error("settings.task_expand_save.failed", error);
        toast.error(translateCommandError(error));
        return false;
      }
    });
    if (!saved && this.default_task_expand_mode === expanded) {
      this.default_task_expand_mode = previous.expanded;
      this.board_task_expand_modes = previous.boards;
    }
  }

  async set_calendar_expand_all(expanded: boolean) {
    const board_id = this.active_board_id;
    if (board_id === null) {
      return;
    }
    const previous = this.board_calendar_expand_all[board_id];
    this.board_calendar_expand_all[board_id] = expanded;
    const saved = await addMission(async () => {
      try {
        await invoke<void>("set_calendar_expand_all", {
          boardId: board_id,
          expanded,
        });
        return true;
      } catch (error) {
        logger.error("settings.calendar_expand_save.failed", error);
        toast.error(translateCommandError(error));
        return false;
      }
    });
    if (!saved && this.board_calendar_expand_all[board_id] === expanded) {
      if (previous === undefined) {
        delete this.board_calendar_expand_all[board_id];
      } else {
        this.board_calendar_expand_all[board_id] = previous;
      }
    }
  }

  async set_column_notify_muted(column_id: string, muted: boolean) {
    const board_id = this.active_board_id;
    if (board_id === null) {
      return;
    }
    const id = get_column_id(column_id);
    const previous = this.notify_muted_columns[board_id];
    const others = (previous ?? []).filter((muted_id) => muted_id !== id);
    this.notify_muted_columns[board_id] = muted ? [...others, id] : others;
    const saved = await addMission(async () => {
      try {
        await invoke<void>("set_column_notify_muted", {
          boardId: board_id,
          columnId: id,
          muted,
        });
        return true;
      } catch (error) {
        logger.error("settings.column_notify_save.failed", error);
        toast.error(translateCommandError(error));
        return false;
      }
    });
    if (!saved && this.is_column_notify_muted(column_id) === muted) {
      if (previous === undefined) {
        delete this.notify_muted_columns[board_id];
      } else {
        this.notify_muted_columns[board_id] = previous;
      }
    }
  }

  async set_language(preference: LanguagePreference): Promise<boolean> {
    const saved = await addMission(async () => {
      try {
        await invoke("set_language", { language: preference });
        return true;
      } catch (error) {
        logger.error("settings.language_save.failed", error);
        toast.error(translateCommandError(error));
        return false;
      }
    });
    if (saved) {
      applyLanguagePreference(preference);
      try {
        await emit("cardbe:language-changed", preference);
      } catch (error) {
        logger.warn("settings.language_broadcast.failed", error);
      }
    }
    return saved;
  }

  private async request_notification_permission(): Promise<boolean> {
    try {
      let permission_granted = await isPermissionGranted();
      if (!permission_granted) {
        const permission = await requestPermission();
        permission_granted = permission === "granted";
      }
      return permission_granted;
    } catch (e: unknown) {
      logger.warn("notifications.permission_request.failed", e);
      return false;
    }
  }

  // ============ Task Templates ============

  save_task_template(task_id: string, name: string): Promise<boolean> {
    const expectedBoardId = this.active_board_id;
    const generation = this.board_generation;
    if (expectedBoardId === null) {
      return Promise.resolve(false);
    }
    const task_id_resolution = this.capture_task_id(task_id);
    return addMission(async () =>
      invoke<TaskTemplateSerialized>("save_task_template", {
        taskId: await task_id_resolution.id,
        name,
        expectedBoardId,
      }),
    )
      .then((template) => {
        if (generation !== this.board_generation) {
          return false;
        }
        const task = deserialize_task(template.task);
        task.id = "";
        this.templates = [
          ...this.templates,
          { id: template.id, name: template.name, task },
        ];
        this.show_success_with_undo(m.task_template_saved());
        return true;
      })
      .catch((e: unknown) => {
        logger.error("template.create.failed", e);
        if (generation === this.board_generation) {
          toast.error(translateCommandError(e));
        }
        return false;
      });
  }

  update_task_template(
    template_id: number,
    name: string,
    task: Task,
  ): Promise<boolean> {
    const expectedBoardId = this.active_board_id;
    const generation = this.board_generation;
    if (expectedBoardId === null) {
      return Promise.resolve(false);
    }
    return addMission(() =>
      invoke<TaskTemplateSerialized>("update_task_template", {
        templateId: template_id,
        name,
        task: serialize_task(task),
        expectedBoardId,
      }),
    )
      .then((template) => {
        if (generation !== this.board_generation) {
          return false;
        }
        const updated_task = deserialize_task(template.task);
        updated_task.id = "";
        this.templates = this.templates.map((candidate) =>
          candidate.id === template.id
            ? { id: template.id, name: template.name, task: updated_task }
            : candidate,
        );
        this.show_success_with_undo(m.task_template_updated());
        return true;
      })
      .catch((e: unknown) => {
        logger.error("template.update.failed", e);
        if (generation === this.board_generation) {
          toast.error(translateCommandError(e));
        }
        return false;
      });
  }

  async copy_task_template_to_board(
    template_id: number,
    target_board_id: number,
  ): Promise<boolean> {
    const expectedBoardId = this.active_board_id;
    try {
      await addMission(() =>
        invoke<void>("copy_task_template_to_board", {
          expectedBoardId,
          templateId: template_id,
          targetBoardId: target_board_id,
        }),
      );
      await this.get_boards();
      toast.success(m.template_copied_board());
      return true;
    } catch (e) {
      logger.error("template.copy_to_board.failed", e);
      toast.error(translateCommandError(e));
      return false;
    }
  }

  delete_task_template(template_id: number): Promise<boolean> {
    const expectedBoardId = this.active_board_id;
    const generation = this.board_generation;
    if (expectedBoardId === null) {
      return Promise.resolve(false);
    }
    return addMission(() =>
      invoke<void>("delete_task_template", {
        templateId: template_id,
        expectedBoardId,
      }),
    )
      .then(() => {
        if (generation !== this.board_generation) {
          return false;
        }
        this.templates = this.templates.filter(
          (template) => template.id !== template_id,
        );
        this.show_success_with_undo(m.task_template_deleted());
        return true;
      })
      .catch((e: unknown) => {
        logger.error("template.delete.failed", e);
        if (generation === this.board_generation) {
          toast.error(translateCommandError(e));
        }
        return false;
      });
  }

  private async show_recovery_messages() {
    try {
      const messages = await invoke<{ code: "STORAGE_RECOVERED" }[]>(
        "take_recovery_messages",
      );
      if (messages.length > 0) {
        toast.warning(m.backup_recovery(), {
          description: m.backup_recovery_description(),
          duration: 15000,
        });
      }
    } catch (e) {
      logger.warn("storage.recovery_messages_load.failed", e);
    }
  }

  // ============ Import/Export ============

  private timestamped_filename(prefix: string): string {
    const now = new Date();
    const part = (value: number) => String(value).padStart(2, "0");
    return `${prefix}-${now.getFullYear()}-${part(now.getMonth() + 1)}-${part(now.getDate())}_${part(now.getHours())}-${part(now.getMinutes())}-${part(now.getSeconds())}.json`;
  }

  async export_to_file() {
    try {
      const file_path = await save({
        filters: [{ name: "JSON", extensions: ["json"] }],
        defaultPath: this.timestamped_filename("cardbe-board-export"),
      });

      if (!file_path) {
        return;
      }

      const expectedBoardId = this.active_board_id;
      if (expectedBoardId === null) {
        return;
      }
      const data = await invoke<string>("export_data", { expectedBoardId });
      await writeTextFile(file_path, data);
      toast.success(m.board_exported());
    } catch (e) {
      logger.error("board.export.failed", e);
      toast.error(translateCommandError(e));
    }
  }

  async export_all_boards_to_file() {
    try {
      const file_path = await save({
        filters: [{ name: m.backup_file_type(), extensions: ["json"] }],
        defaultPath: this.timestamped_filename("cardbe-everything-backup"),
      });
      if (!file_path) {
        return;
      }
      await writeTextFile(file_path, await invoke<string>("export_all_boards"));
      toast.success(m.backup_created());
    } catch (e) {
      logger.error("backup.export.failed", e);
      toast.error(translateCommandError(e));
    }
  }

  async import_all_boards_from_file(
    confirm_restore = true,
    before_restore?: () => Promise<void>,
  ): Promise<boolean> {
    let share_links_revoked = false;
    try {
      const file_path = await open({
        filters: [{ name: "JSON", extensions: ["json"] }],
        multiple: false,
      });
      if (!file_path) {
        return false;
      }
      const jsonData = await readTextFile(file_path as string);
      await invoke("validate_all_boards_backup", { jsonData });
      if (confirm_restore && !window.confirm(m.backup_restore_confirm())) {
        return false;
      }
      await before_restore?.();
      share_links_revoked = before_restore !== undefined;
      await invoke("import_all_boards", { jsonData });
      await this.get_boards();
      this.board_generation++;
      this.column_fetch_finish = false;
      this.archives_loaded = false;
      this.get_columns();
      this.update_labels();
      this.get_task_templates();
      await this.load_settings();
      toast.success(m.backup_restored());
      return true;
    } catch (e) {
      logger.error("backup.restore.failed", e);
      toast.error(
        share_links_revoked
          ? m.backup_restore_revoked_error()
          : translateCommandError(e),
        {
          description: share_links_revoked
            ? translateCommandError(e)
            : undefined,
        },
      );
      return false;
    }
  }

  async prepare_import_from_file(): Promise<ImportCandidate | null> {
    try {
      const file_path = await open({
        filters: [{ name: "JSON", extensions: ["json"] }],
        multiple: false,
      });

      if (!file_path) {
        return null;
      }

      const data = await readTextFile(file_path as string);
      const filename =
        String(file_path).split(/[\\/]/).pop() ?? m.ui_imported_board();
      const board_name =
        filename.replace(/\.json$/i, "").trim() || m.ui_imported_board();
      return {
        json_data: data,
        summary: parse_import_summary(data),
        board_name,
      };
    } catch (e) {
      logger.error("board.import_file_read.failed", e);
      toast.error(m.backup_import_read_error());
      return null;
    }
  }

  async import_board_as_new(candidate: ImportCandidate): Promise<boolean> {
    try {
      const created = await invoke<BoardSummary>("import_board_as_new", {
        jsonData: candidate.json_data,
        name: candidate.board_name,
      });
      await this.get_boards();
      this.active_board_id = created.id;
      this.board_generation++;
      this.column_fetch_finish = false;
      this.archives_loaded = false;
      this.get_columns();
      this.update_labels();
      this.get_task_templates();
      toast.success(m.board_imported_named({ name: created.name }));
      return true;
    } catch (e) {
      logger.error("board.import_as_new.failed", e);
      toast.error(translateCommandError(e));
      return false;
    }
  }

  import_data(json_data: string): Promise<boolean> {
    const expectedBoardId = this.active_board_id;
    let generation = this.board_generation;
    let reload_archives = false;
    if (expectedBoardId === null) {
      return Promise.resolve(false);
    }
    return addMission(() =>
      flow_imports.replace(expectedBoardId, async () => {
        const snapshot_path = await invoke<string>("import_data", {
          jsonData: json_data,
          expectedBoardId,
        });
        if (generation === this.board_generation) {
          // Discard reads started against the replaced tasks before loading Flow.
          generation = ++this.board_generation;
          reload_archives = this.reset_archive_request();
          // The write gate is already closed, so unmount cannot flush the old
          // canvas. A failed read uses the existing initial-load error screen.
          this.column_fetch_finish = false;
          await this.get_columns();
        }
        return snapshot_path;
      }),
    )
      .then((snapshot_path) => {
        if (generation !== this.board_generation) {
          return false;
        }
        this.update_labels();
        if (reload_archives) {
          this.get_archives();
        }
        this.get_task_templates();
        // The backend clears task-only Undo when a portable Flow backup is
        // restored. Omitted/null Flow retains the legacy import behavior.
        this.can_undo = JSON.parse(json_data).flow == null;
        toast.success(m.backup_imported(), {
          id: "data-import-success",
          description: m.backup_pre_import_path({ path: snapshot_path }),
          duration: 10000,
          action: this.can_undo
            ? {
                label: m.common_undo(),
                onClick: () => void this.undo(),
              }
            : undefined,
        });
        return true;
      })
      .catch((e: unknown) => {
        logger.error("board.import_data.failed", e);
        if (generation === this.board_generation) {
          toast.error(translateCommandError(e));
        }
        return false;
      });
  }
}

// Singleton instance
export const board = new BoardStore();

if (import.meta.hot) {
  import.meta.hot.dispose(() => board.dispose());
}
