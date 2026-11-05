> **企業版開發副本**：新增獨立的本機企業基礎原型，包含成員、板級權限、任務討論與稽核。請先閱讀[啟動方式](enterprise-server/README.md)與[實作進度及限制](docs/enterprise-progress.md)；完整目標見[企業版計畫](docs/enterprise-plan.md)。

<p align="center">
  <img src="app_icon.png" alt="Cardbe" width="128" height="128">
</p>

<h1 align="center">Cardbe</h1>

<p align="center">
  <strong>A calm, private task board that lives on your computer.</strong><br>
  Plan your work on cards, see your day at a glance, and share a board only when you want to.
</p>

<p align="center">
  <a href="https://github.com/ldslds449/Cardbe/releases/latest">Download for Windows</a> ·
  <a href="docs/sharing-and-privacy.md">Privacy</a> ·
  <a href="docs/development.md">Build from source</a>
</p>

<p align="center">
  <img src="docs/images/board.png" alt="A Cardbe board with Ideas, To do, In progress, Review, and Done columns">
</p>

---

## Why Cardbe?

- **Your data stays with you.** Everything is saved on your own computer. No account, no subscription, no cloud required.
- **Works offline.** Create, edit, and organize tasks without an internet connection.
- **Simple to start, room to grow.** Begin with a few columns of cards, then add notes, schedules, checklists, and more as you need them.

## What you can do

### Organize work your way

- Create as many boards as you like, each with your own columns.
- Drag and drop cards to reorder them or move them between columns and boards.
- Add labels, checklists, start and due dates, recurring schedules, and rich-text descriptions.
- Pin important cards, archive finished ones, and restore them anytime.
- Save frequently used cards as templates.

### See your work from different angles

| View           | What it shows                                                          |
| -------------- | ---------------------------------------------------------------------- |
| **Board**      | Your columns and cards, the classic Kanban layout.                     |
| **Focus**      | Your own saved filters across all boards, such as what is due today.   |
| **Calendar**   | Your tasks on a monthly or weekly calendar, including multi-day tasks. |
| **Flow**       | A free-form canvas to arrange cards, draw arrows, and add notes.       |
| **Statistics** | How much you have finished and how your work has progressed.           |

<table>
  <tr>
    <td width="50%"><img src="docs/images/home.png" alt="Home page with Focus and board overviews"><br><p align="center"><b>Home</b> — every Focus and board at a glance</p></td>
    <td width="50%"><img src="docs/images/calendar.png" alt="Calendar view showing multi-day tasks as bars"><br><p align="center"><b>Calendar</b> — plan your month at a glance</p></td>
  </tr>
  <tr>
    <td width="50%"><img src="docs/images/flow.png" alt="Flow canvas with tasks connected by arrows"><br><p align="center"><b>Flow</b> — map out what depends on what</p></td>
    <td width="50%"><img src="docs/images/statistics.png" alt="Statistics view with an activity heatmap"><br><p align="center"><b>Statistics</b> — see your progress over time</p></td>
  </tr>
</table>

### Plan today

- Choose tasks from any board for **Today's plan** on the home page, including tasks without a due date.
- Move items earlier or later, open or complete tasks, and remove items without deleting the tasks.
- Drag the handle to reorder tasks, or use the task menu to move them with the keyboard. Float the plan on your desktop to keep the same list in sight; changes follow across both windows.
- The ordered list stays on this device across restarts and days until you remove items. Archived, deleted, or transferred tasks are hidden. The plan is outside board synchronization, Undo, and portable backups.

### Capture ideas instantly

- Use a global keyboard shortcut to add a task or a note from anywhere, even when Cardbe is in the background.
- Keep quick Markdown notes alongside your boards.
- Write formulas with `$...$` inline or `$$...$$` as a display formula (on one line or across multiple lines), and diagrams in fenced `mermaid` blocks. KaTeX and Pele render them offline in task descriptions and notes. In the rich text editor, use `/math` or `/mermaid` to insert them, type `$$` and press Enter for a formula block, and select a formula to edit its LaTeX.
- Pasted URLs become links, including image URLs. Use Markdown image syntax (`![description](URL)`) to display an image explicitly. Desktop images use the existing safe preview loader (public HTTP/HTTPS destinations, supported raster formats, up to 512 KiB); external images require a connection.
- Pop a task out into a small floating window to keep it in sight while you work.

<p align="center">
  <img src="docs/images/notes.png" alt="Notes panel beside the board, with links to related cards">
</p>

### Find anything quickly

- Search across titles, descriptions, labels, and checklists — even if you only remember a few letters of a title.
- Browse every task across all boards in one place, then filter and sort it the way you need.

### Share when you choose to

- **Show a few cards** to people on the same Wi-Fi with a temporary, read-only link.
- **Invite others to a whole board** with a link or QR code. Invite them as viewers, or as editors who can make changes with you. Changes sync automatically, and edits made while offline are merged when everyone reconnects.
- Stop sharing at any time.

Sharing is always off until you turn it on. Learn more in [Sharing and privacy](docs/sharing-and-privacy.md).

### Make it yours

- Choose from light, dark, and a range of soft Morandi color themes.
- Use Cardbe in English or Traditional Chinese (繁體中文).
- Back up and restore your boards, and export your calendar.
- Extend Cardbe with optional plugins, such as syncing tasks with other services. Plugins run in a sandbox and must ask for your permission before connecting to the internet.

## Get Cardbe

1. Go to the [latest release](https://github.com/ldslds449/Cardbe/releases/latest).
2. Download the Windows installer, or the portable `.exe` if you prefer not to install anything.
3. Open Cardbe and create your first board.

> [!NOTE]
> Cardbe is not code-signed yet, so Windows may show a "Windows protected your PC" message the first time you open it. Select **More info**, then **Run anyway**. Only do this for files downloaded from the official [Releases page](https://github.com/ldslds449/Cardbe/releases).

Cardbe currently provides ready-to-use downloads for Windows. Developers can [build it from source](docs/development.md).

## Privacy at a glance

- Your boards, tasks, and notes are stored only on your computer.
- Cardbe does not collect analytics and does not require an account.
- Cardbe goes online only to check for updates, to preview a link you hover over, or when you turn on sharing or a plugin that needs it.

See [Sharing and privacy](docs/sharing-and-privacy.md) for the details.

## For developers

- [Development guide](docs/development.md) — setup, scripts, and project structure
- [Sharing and privacy](docs/sharing-and-privacy.md) — how LAN sharing and shared boards work
- [MCP server](docs/mcp-server.md) — connect local AI tools to your boards
- [Plugin API](https://github.com/ldslds449/cardbe-plugin-api) — build your own Cardbe plugin

## License

Cardbe is released under the [Apache License 2.0](LICENSE). See [NOTICE](NOTICE) for third-party components.
