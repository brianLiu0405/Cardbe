import {
  parseCommandError,
  translateCommandError,
} from "#lib/command-errors.js";
import { m } from "#lib/paraglide/messages.js";
import type { TaskExplorerQuery } from "./utils/task-explorer";
import { logger } from "#lib/logger.js";
import { invoke } from "@tauri-apps/api/core";
import { toast } from "svelte-sonner";
import {
  deserialize_column,
  type Column,
  type ColumnSerialized,
} from "./type/column.svelte";
import {
  type Task,
  type TaskSummary,
  get_task_id,
  deserialize_task,
  deserialize_task_summary,
  type TaskSerialized,
  type TaskSummarySerialized,
  type TaskTemplate,
  type TaskTemplateSerialized,
} from "./type/task.svelte";
import type { Archive } from "./type/archive.svelte";
import { addMission } from "./utils/mission.svelte";
import { TaskExpansionState } from "./task-expansion.svelte";
import { layout_widths } from "./layout-widths.svelte";

const COLUMN_FETCH_TIMEOUT_MS = 15_000;

function with_timeout<T>(
  request: Promise<T>,
  timeout_ms: number,
  message: string,
): Promise<T> {
  let timeout: ReturnType<typeof setTimeout> | undefined;
  return new Promise<T>((resolve, reject) => {
    timeout = setTimeout(() => reject(new Error(message)), timeout_ms);
    request.then(resolve, reject).finally(() => {
      if (timeout !== undefined) {
        clearTimeout(timeout);
      }
    });
  });
}

export type BoardRole = "owner" | "editor" | "viewer";
export interface BoardSummary {
  id: number;
  name: string;
  task_count: number;
  shared_role: BoardRole;
  is_shared?: boolean;
  sync_status: string;
  sync_revision: number;
}
interface BoardsState {
  boards: BoardSummary[];
  active_board_id: number;
}

interface AllTaskPageSerialized {
  items: Array<{
    board_id: number;
    board_name: string;
    column_id: number | null;
    column_name: string | null;
    archived_at: number | null;
    done: boolean;
    waiting_count: number;
    task: TaskSummarySerialized;
    excerpt?: string;
  }>;
  next_cursor: string | null;
}

export interface AllTaskItem {
  board_id: number;
  board_name: string;
  column_id?: string;
  column_name: string | undefined;
  archived_at: Date | undefined;
  /** In its board's done column. */
  done?: boolean;
  /** Open tasks of its board that it still waits on. */
  waiting_count?: number;
  task: TaskSummary;
  /** Where a search matched outside the title. */
  excerpt?: string;
}

// Board selection, board data loading, search, archives, the task
// explorer, and Undo. Higher layers add sharing and editing operations.
export abstract class BoardStoreBase {
  // Reactive state
  columns = $state<Column[]>([]);
  labels = $state<string[]>([]);
  archives = $state<Archive[]>([]);
  archives_loaded = $state(false);
  archives_loading = $state(false);
  all_task_items = $state<AllTaskItem[]>([]);
  all_task_items_loading = $state(false);
  all_task_items_error = $state(false);
  all_task_items_has_more = $state(false);
  templates = $state<TaskTemplate[]>([]);
  column_fetch_finish = $state(false);
  column_fetch_error = $state(false);
  task_expansion = new TaskExpansionState();
  private has_undo_history = $state(false);
  private has_redo_history = $state(false);
  get can_edit() {
    const active = this.boards.find(
      (board) => board.id === this.active_board_id,
    );
    return Boolean(
      active &&
      (active.shared_role === "owner" || active.shared_role === "editor"),
    );
  }
  get can_undo() {
    return this.has_undo_history && this.can_edit;
  }
  /** Any change outside Undo and Redo also discards what could be redone. */
  set can_undo(value: boolean) {
    this.has_undo_history = value && this.can_edit;
    this.has_redo_history = false;
  }
  get can_redo() {
    return this.has_redo_history && this.can_edit;
  }
  undo_in_progress = $state(false);
  search_task_ids = $state<Set<string> | null>(null);
  search_pending = $state(false);
  search_error = $state(false);
  boards = $state<BoardSummary[]>([]);
  active_board_id = $state<number | null>(null);
  protected board_generation = 0;
  protected task_move_timers = new Map<string, ReturnType<typeof setTimeout>>();
  protected column_move_timers = new Map<
    string,
    ReturnType<typeof setTimeout>
  >();
  private archives_request: Promise<void> | undefined;
  private all_task_list_cursor: string | null = null;
  protected all_task_list_filter: TaskExplorerQuery | null = null;
  private all_task_list_request = 0;
  protected search_query = "";
  private search_request = 0;
  private search_results_generation = -1;
  private search_timer: ReturnType<typeof setTimeout> | undefined;
  protected disposed = false;

  // ============ Data Fetching ============

  private show_data_fetch_error(
    message: string,
    error: unknown,
    retry: () => void,
  ) {
    toast.error(message, {
      description: translateCommandError(error),
      action: {
        label: m.common_retry(),
        onClick: retry,
      },
    });
  }

  protected show_success_with_undo(message: string) {
    this.can_undo = true;
    if (!this.can_undo) {
      toast.success(message);
      return;
    }
    toast.success(message, {
      action: {
        label: m.common_undo(),
        onClick: () => void this.undo(),
      },
    });
  }

  undo(): Promise<boolean> {
    return this.step_history("undo");
  }

  redo(): Promise<boolean> {
    return this.step_history("redo");
  }

  private async step_history(direction: "undo" | "redo"): Promise<boolean> {
    if (
      (direction === "undo" ? !this.can_undo : !this.can_redo) ||
      this.undo_in_progress
    ) {
      return false;
    }
    this.undo_in_progress = true;
    try {
      const expectedBoardId = this.active_board_id;
      const generation = this.board_generation;
      if (expectedBoardId === null) {
        return false;
      }
      const remaining = await addMission(() =>
        invoke<boolean>(direction === "undo" ? "undo" : "redo", {
          expectedBoardId,
        }),
      );
      if (generation !== this.board_generation) {
        return false;
      }
      // Each step moves one entry onto the opposite history.
      if (direction === "undo") {
        this.can_undo = remaining;
        this.has_redo_history = this.can_edit;
      } else {
        this.can_undo = true;
        this.has_redo_history = remaining && this.can_edit;
      }
      this.get_columns();
      this.refresh_archives_if_loaded();
      this.get_task_templates();
      this.update_labels();
      toast.success(
        direction === "undo" ? m.common_undone() : m.common_redone(),
      );
      return true;
    } catch (e: unknown) {
      logger.error(`board.${direction}.failed`, e);
      toast.error(translateCommandError(e));
      return false;
    } finally {
      this.undo_in_progress = false;
    }
  }

  protected refresh_labels_from_columns() {
    this.sync_active_board_task_count();
    const active_labels = new Set(
      this.columns.flatMap((column) =>
        column.tasks.flatMap((task) => task.labels),
      ),
    );
    this.labels = [
      ...this.labels.filter((label) => active_labels.delete(label)),
      ...Array.from(active_labels).sort(),
    ];
  }

  update_labels() {
    const board = this;
    const expectedBoardId = this.active_board_id;
    const generation = this.board_generation;
    if (expectedBoardId === null) {
      return;
    }
    addMission(() => invoke<string[]>("get_labels", { expectedBoardId }))
      .then((data) => {
        if (generation !== board.board_generation) {
          return;
        }
        board.labels = data;
      })
      .catch((e) => {
        logger.error("board.labels_load.failed", e);
        board.show_data_fetch_error(m.board_labels_load_error(), e, () =>
          board.update_labels(),
        );
      });
  }

  get_columns() {
    const board = this;
    const expectedBoardId = this.active_board_id;
    const generation = this.board_generation;
    if (expectedBoardId === null) {
      return;
    }
    // Only replace the workspace with the loading state during the initial fetch
    // or when retrying an initial-load error.
    // Later refreshes (for example after archiving a recurring task) must keep the
    // existing view mounted so its scroll position is preserved.
    const replaces_workspace =
      !board.column_fetch_finish || board.column_fetch_error;
    if (replaces_workspace) {
      board.column_fetch_finish = false;
      board.column_fetch_error = false;
    }
    // The main workspace readiness gate must not wait behind unrelated
    // background missions (such as loading labels). If one of those IPC
    // calls never settles, queueing this read leaves the app on its
    // initial loading screen forever.
    return with_timeout(
      invoke<ColumnSerialized[]>("get_columns", { expectedBoardId }),
      COLUMN_FETCH_TIMEOUT_MS,
      "Loading board data timed out",
    )
      .then((data: ColumnSerialized[]) => {
        if (generation !== board.board_generation) {
          return;
        }
        board.columns = data.map(deserialize_column);
        for (const column of board.columns) {
          board.keep_pinned_tasks_first(column);
        }
        board.sync_active_board_task_count();
        board.task_expansion.prune(
          expectedBoardId,
          board.columns.flatMap((column) =>
            column.tasks.map((task) => task.id),
          ),
        );
        board.column_fetch_finish = true;
      })
      .catch((e) => {
        logger.error("board.columns_load.failed", e);
        if (generation !== board.board_generation) {
          return;
        }
        if (replaces_workspace) {
          board.column_fetch_error = true;
          board.column_fetch_finish = true;
        } else {
          board.show_data_fetch_error(m.board_columns_refresh_error(), e, () =>
            board.get_columns(),
          );
        }
      });
  }

  search_tasks(search_text: string) {
    const query_text = search_text.trim();
    // Refreshing the same query after board edits keeps current results
    // visible instead of flashing the searching placeholder.
    const refreshing =
      query_text !== "" &&
      query_text === this.search_query &&
      this.search_task_ids !== null &&
      this.search_results_generation === this.board_generation;
    this.search_query = query_text;
    const request = ++this.search_request;
    if (this.search_timer) {
      clearTimeout(this.search_timer);
    }
    this.search_error = false;
    if (!refreshing) {
      this.search_task_ids = null;
      this.search_pending = Boolean(this.search_query);
    }
    if (!this.search_query) {
      return;
    }

    const query = this.search_query;
    const query_length = [...query].length;
    const expectedBoardId = this.active_board_id;
    if (expectedBoardId === null) {
      this.search_pending = false;
      return;
    }
    const generation = this.board_generation;
    this.search_timer = setTimeout(async () => {
      if (
        request !== this.search_request ||
        generation !== this.board_generation
      ) {
        return;
      }
      this.search_timer = undefined;
      const started_at = performance.now();
      try {
        const ids = await invoke<number[]>("search_tasks", {
          query,
          expectedBoardId,
        });
        if (
          request !== this.search_request ||
          generation !== this.board_generation
        ) {
          return;
        }
        this.search_task_ids = new Set(ids.map((id) => `task_${id}`));
        this.search_results_generation = generation;
        logger.debug("board.search.completed", {
          board_id: expectedBoardId,
          query_length,
          result_count: ids.length,
          duration_ms: Math.round(performance.now() - started_at),
        });
      } catch (error) {
        if (
          request !== this.search_request ||
          generation !== this.board_generation
        ) {
          return;
        }
        logger.error("board.search.failed", error, {
          board_id: expectedBoardId,
          query_length,
          duration_ms: Math.round(performance.now() - started_at),
        });
        this.search_error = true;
      } finally {
        if (
          request === this.search_request &&
          generation === this.board_generation
        ) {
          this.search_pending = false;
        }
      }
    }, 160);
  }

  protected reset_archive_request() {
    const reload = this.archives_loaded || this.archives_loading;
    this.archives = [];
    this.archives_loaded = false;
    this.archives_loading = false;
    this.archives_request = undefined;
    return reload;
  }

  get_archives() {
    if (this.archives_request) {
      return this.archives_request;
    }
    const board = this;
    const expectedBoardId = this.active_board_id;
    const generation = this.board_generation;
    if (expectedBoardId === null) {
      return Promise.resolve();
    }
    board.archives_loading = true;
    const request = addMission(() =>
      invoke<{ time: number; task: TaskSerialized }[]>("get_archives", {
        expectedBoardId,
      }),
    )
      .then((data) => {
        if (generation !== board.board_generation) {
          return;
        }
        board.archives = data.reduce<Archive[]>((list, d) => {
          list.push({
            time: new Date(d.time),
            task: deserialize_task(d.task),
          });
          return list;
        }, []);
        board.archives_loaded = true;
      })
      .catch((e) => {
        logger.error("board.archives_load.failed", e);
        if (generation === board.board_generation) {
          board.show_data_fetch_error(m.task_archives_load_error(), e, () =>
            board.get_archives(),
          );
        }
      })
      .finally(() => {
        if (generation === board.board_generation) {
          board.archives_loading = false;
          board.archives_request = undefined;
        }
      });
    this.archives_request = request;
    return request;
  }

  async get_archive_detail(task_id: string): Promise<Task | null> {
    const expectedBoardId = this.active_board_id;
    if (expectedBoardId === null) {
      return null;
    }
    const cached = this.archives.find((archive) => archive.task.id === task_id);
    if (cached) {
      return cached.task;
    }
    try {
      const data = await invoke<TaskSerialized>("get_archive_task", {
        expectedBoardId,
        taskId: get_task_id(task_id),
      });
      return deserialize_task(data);
    } catch (error) {
      logger.error("board.archive_detail_load.failed", error);
      return null;
    }
  }

  ensure_archives_loaded() {
    if (this.archives_loaded) {
      return Promise.resolve();
    }
    return this.get_archives();
  }

  protected refresh_archives_if_loaded() {
    if (this.all_task_list_filter) {
      void this.fetch_all_task_page(true);
    }
    if (this.archives_loaded) {
      void this.get_archives();
    }
  }

  search_all_tasks(filter: TaskExplorerQuery) {
    this.all_task_list_filter = filter;
    return this.fetch_all_task_page(true);
  }

  load_more_all_tasks() {
    if (!this.all_task_items_loading && this.all_task_items_has_more) {
      this.fetch_all_task_page(false);
    }
  }

  protected fetch_all_task_page(reset: boolean) {
    if (!this.all_task_list_filter) {
      return Promise.resolve();
    }
    const request = ++this.all_task_list_request;
    if (reset) {
      this.all_task_items = [];
      this.all_task_items_loading = false;
      this.all_task_list_cursor = null;
      this.all_task_items_has_more = true;
    }
    this.all_task_items_loading = true;
    this.all_task_items_error = false;
    return invoke<AllTaskPageSerialized>("list_all_tasks", {
      cursor: reset ? null : this.all_task_list_cursor,
      filter: this.all_task_list_filter,
      limit: 50,
    })
      .then((data) => {
        if (request !== this.all_task_list_request) {
          return;
        }
        const items = data.items.map((item) => ({
          board_id: item.board_id,
          board_name: item.board_name,
          column_id:
            item.column_id === null || item.column_id === undefined
              ? undefined
              : `column_${item.column_id}`,
          column_name: item.column_name ?? undefined,
          archived_at:
            item.archived_at === null ? undefined : new Date(item.archived_at),
          done: item.done,
          waiting_count: item.waiting_count,
          task: deserialize_task_summary(item.task),
          excerpt: item.excerpt,
        }));
        this.all_task_items = reset
          ? items
          : [
              ...this.all_task_items,
              ...items.filter(
                (item) =>
                  !this.all_task_items.some(
                    (existing) =>
                      existing.board_id === item.board_id &&
                      existing.archived_at?.getTime() ===
                        item.archived_at?.getTime() &&
                      existing.task.id === item.task.id,
                  ),
              ),
            ];
        this.all_task_list_cursor = data.next_cursor;
        this.all_task_items_has_more = data.next_cursor !== null;
      })
      .catch((error) => {
        logger.error("board.all_task_page_load.failed", error);
        if (request === this.all_task_list_request) {
          this.all_task_items_error = true;
          this.show_data_fetch_error(m.task_explorer_load_error(), error, () =>
            this.fetch_all_task_page(reset),
          );
        }
      })
      .finally(() => {
        if (request === this.all_task_list_request) {
          this.all_task_items_loading = false;
        }
      });
  }

  async get_expired_task_detail(task_id: string): Promise<Task | null> {
    const expectedBoardId = this.active_board_id;
    if (expectedBoardId === null) {
      return null;
    }
    const active = this.columns
      .flatMap((column) => column.tasks)
      .find((task) => task.id === task_id);
    if (active) {
      return active;
    }
    try {
      const data = await invoke<TaskSerialized>("get_task_detail", {
        expectedBoardId,
        taskId: get_task_id(task_id),
      });
      return deserialize_task(data);
    } catch (error) {
      // Missing tasks are expected, e.g. when a link targets an archived task.
      if (parseCommandError(error)?.code !== "TASK_NOT_FOUND") {
        logger.error("board.expired_task_detail_load.failed", error);
      }
      return null;
    }
  }

  private sync_active_board_task_count() {
    const active_board_id = this.active_board_id;
    if (active_board_id === null) {
      return;
    }
    const task_count = this.columns.reduce(
      (total, column) => total + column.tasks.length,
      0,
    );
    this.boards = this.boards.map((item) =>
      item.id === active_board_id && item.task_count !== task_count
        ? { ...item, task_count }
        : item,
    );
  }

  async get_boards() {
    const generation = this.board_generation;
    try {
      const state = await invoke<BoardsState>("get_boards");
      if (generation !== this.board_generation) {
        return;
      }
      this.boards = state.boards;
      this.active_board_id = state.active_board_id;
      layout_widths.prune_boards(state.boards.map((board) => board.id));
      if (!this.can_edit) {
        this.can_undo = false;
      }
    } catch (e) {
      logger.error("board.list_load.failed", e);
    }
  }

  async create_board(name: string) {
    const trimmed = name.trim();
    if (!trimmed) {
      return false;
    }
    try {
      const created = await invoke<BoardSummary>("create_board", {
        name: trimmed,
      });
      this.boards = [...this.boards, created];
      await this.switch_board(created.id);
      return true;
    } catch (e) {
      logger.error("board.create.failed", e);
      toast.error(translateCommandError(e));
      return false;
    }
  }

  async rename_board(id: number, name: string) {
    const trimmed = name.trim();
    if (!trimmed) {
      return false;
    }
    try {
      await invoke("rename_board", { boardId: id, name: trimmed });
      this.boards = this.boards.map((board) =>
        board.id === id ? { ...board, name: trimmed } : board,
      );
      return true;
    } catch (e) {
      logger.error("board.rename.failed", e);
      toast.error(translateCommandError(e));
      return false;
    }
  }

  async switch_board(id: number): Promise<boolean> {
    if (id === this.active_board_id) {
      return true;
    }
    try {
      this.board_generation++;
      for (const timer of this.task_move_timers.values()) {
        clearTimeout(timer);
      }
      for (const timer of this.column_move_timers.values()) {
        clearTimeout(timer);
      }
      this.task_move_timers.clear();
      this.column_move_timers.clear();
      await invoke("switch_board", { boardId: id });
      this.column_fetch_finish = false;
      this.column_fetch_error = false;
      this.active_board_id = id;
      this.archives = [];
      this.archives_loaded = false;
      this.archives_loading = false;
      this.archives_request = undefined;
      this.templates = [];
      this.labels = [];
      this.can_undo = false;
      this.get_columns();
      this.update_labels();
      this.get_task_templates();
      return true;
    } catch (e) {
      logger.error("board.switch.failed", e);
      toast.error(translateCommandError(e));
      return false;
    }
  }

  async get_archived_count(id: number): Promise<number | null> {
    try {
      return await invoke<number>("get_board_archived_count", { boardId: id });
    } catch (e) {
      logger.error("board.archived_count.failed", e);
      return null;
    }
  }

  async delete_board(id: number) {
    try {
      const next = await invoke<number>("delete_board", { boardId: id });
      this.boards = this.boards.filter((board) => board.id !== id);
      if (id === this.active_board_id) {
        this.active_board_id = null;
        await this.switch_board(next);
      }
      return true;
    } catch (e) {
      logger.error("board.delete.failed", e);
      toast.error(translateCommandError(e));
      return false;
    }
  }

  /**
   * `column.tasks` is the manual order; sort rules apply only when displayed
   * (see `sorted_column_tasks`). Matches `keep_pinned_tasks_first` in Rust.
   */
  keep_pinned_tasks_first(column: Column) {
    column.tasks.sort(
      (a, b) => Number(b.pinned ?? false) - Number(a.pinned ?? false),
    );
  }

  get_task_templates() {
    const expectedBoardId = this.active_board_id;
    const generation = this.board_generation;
    if (expectedBoardId === null) {
      return;
    }
    addMission(() =>
      invoke<TaskTemplateSerialized[]>("get_task_templates", {
        expectedBoardId,
      }),
    )
      .then((templates) => {
        if (generation !== this.board_generation) {
          return;
        }
        this.templates = templates.map((template) => {
          const task = deserialize_task(template.task);
          task.id = "";
          return { id: template.id, name: template.name, task };
        });
      })
      .catch((e: unknown) => {
        logger.error("template.load.failed", e);
        if (generation === this.board_generation) {
          this.show_data_fetch_error(m.task_template_load_error(), e, () =>
            this.get_task_templates(),
          );
        }
      });
  }

  protected reload_active_board_data() {
    this.get_columns();
    this.update_labels();
    this.get_task_templates();
    if (this.archives_loaded) {
      void this.get_archives();
    }
  }
}
