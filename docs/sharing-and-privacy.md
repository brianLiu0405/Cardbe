# Sharing and privacy

[Back to README](../README.md)

This page describes personal desktop mode. The separate, opt-in [enterprise prototype](../enterprise-server/README.md) has its own loopback service, development credentials, and isolated database; it does not upload personal boards.

## Where your data lives

Cardbe persists its application data locally in SQLite. Release builds use the system app-local-data directory. The repository does not include a sample database or personal task data.

Cardbe does not require an account, cloud sync, or an API key. It contacts the network only in these cases:

- **Update checks** contact the GitHub Releases API to check for a newer version.
- **Link previews** fetch the page title and image of a link when you hover over it in a task.
- **LAN sharing** starts a local HTTP server while a share is published.
- **Shared boards** connect to Iroh relay services for peer discovery and transport while an invitation is active. Disabling or deleting every invitation stops the owner sharing endpoint.
- **Plugins** can reach only the HTTPS hosts you approve for each plugin instance.

## Portable board backups

Single-board JSON exports include the board's tasks, archives, templates, labels, Flow tabs, layouts, relationships, and annotations. They also include the full content of notes referenced by those Flow canvases; unrelated global notes are excluded. Importing into another database creates new note IDs and remaps those references instead of linking to unrelated notes with the same old ID.

An overwrite import saves the previous board, Flow, and referenced notes in the pre-import recovery file. A full Flow import clears task-only Undo because that history cannot restore canvases and notes together; use the recovery file to restore the previous complete board. Legacy task-only backups remain readable.

## LAN sharing

LAN sharing is opt-in: publishing a board starts a local HTTP server reachable by devices on the same network and exposes the selected task content to anyone who has the unguessable share link. Treat the link as sensitive, share only content suitable for that audience, set an expiry where appropriate, and revoke it when finished. Do not use the feature on an untrusted network.

## Shared boards

Board invitations are bearer capabilities: anyone who receives an active link can use its current permission. A received board is stored separately and marked `read only` or `shared` in the sidebar. Shared boards sync automatically while Cardbe is active and online; **Sync now** remains available for an immediate refresh.

Read-only boards receive the owner's current snapshot when its revision or permission changes; unchanged polls receive an empty result. Editors exchange Loro document updates over Iroh with the board owner, so offline changes to different cards or fields merge when they reconnect to the owner. Editors cannot exchange updates directly while the owner is offline.

SQLite remains the local queryable copy; Loro is the mergeable source for shared board content. Its Tree stores column order, card order, and card moves; Map stores individual fields; Text stores descriptions for a future Loro rich-text editor in Svelte. Concurrent changes to the same scalar field use Loro's deterministic map conflict rule.

### Secrets and identity

Invitation secrets, device identity, and received-board connection metadata stay in the local SQLite database and are intentionally excluded from JSON backups. This avoids exporting access to another person's board by accident.

Each installation has a stable device identity; invitations use that identity with relay-based discovery rather than persisting peer IP addresses, so reconnecting after an IP or NAT change can use a fresh route. The sharing endpoint starts only while an invitation is active. Global app settings are excluded from shared-board snapshots. Restoring a JSON backup on another device intentionally creates a new identity and requires a new invitation.

### Revocation limits

Revoking an invitation stops future sync, but cannot erase board data already downloaded by a recipient. Loro documents can retain deleted text in their edit history, including invitation snapshots; do not treat deleting a card as secure erasure from shared copies.
