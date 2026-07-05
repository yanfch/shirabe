# Shared Workspace Multi-Account Plan

## Goal

Support multiple macOS accounts on the same Mac with one shared Shirabe dataset.

V1 is local-only:

- One Mac.
- Multiple macOS accounts.
- One shared workspace database.
- One server writes the database at a time.
- Each macOS account collects only its own local AI data.
- UI and menu bar can show all accounts or filter to one account.

Cloud sync is out of scope for V1, but the data model must keep a stable
`device_id` and `profile_id` so cloud storage can replace local SQLite later.

## Directory Layout

Single-account mode stays unchanged:

```text
~/.shirabe/catalog.sqlite
~/.shirabe/config.json
```

Shared mode uses:

```text
/Users/Shared/Shirabe/
  catalog.sqlite
  workspace.json
  server.lock
  inbox/
  profiles/
  collector-state/
  logs/
```

Do not introduce a public `shards/` concept. If collectors need an offline
buffer, use `inbox/`.

Per-user source paths and profile identity stay in each account's own config:

```text
~/.shirabe/config.json
```

Do not store one account's `profile_id` in shared workspace config. Each
collector writes a small `profiles/<profile_id>.json` sidecar with account and
device display labels so the server can register filter labels without reading
another user's home directory.

## Process Model

Use three roles:

```text
UI / menu bar
  Shows data and triggers sync.

Server
  Owns the API, rollups, aggregation, and SQLite writes.
  Only one server may write the shared database at a time.

Collector / importer
  Runs under the current macOS account.
  Reads only that account's home directory.
  Writes metadata-only normalized events to inbox files.
```

Two accounts online at the same time:

```text
Account A opens Shirabe
-> starts or connects to the shared server
-> if it gets the lock, it owns DB writes

Account B opens Shirabe
-> detects the server
-> does not start a second writer
-> runs its own collector
-> writes Account B events with Account B profile_id to inbox
-> asks the shared server to drain inbox
```

If the server owner exits, another account may take `server.lock` and continue
writing the same shared database.

## Identity Model

Add stable IDs and display labels:

```text
devices
  device_id
  device_label
  hostname
  created_at_ns
  last_seen_ns

profiles
  profile_id
  device_id
  profile_label
  macos_uid
  macos_username
  created_at_ns
  last_seen_ns
```

Rules:

- `device_id` is stable per Mac. Prefer a hash of the macOS platform UUID when
  available; do not derive it from a per-user path.
- `profile_id` is stable per macOS account and not derived from imported file
  paths.
- Use `profile_label` in filters and UI.
- Default `profile_label` is the macOS full name, falling back to short
  username.
- Default `device_label` is macOS ComputerName, falling back to hostname.
- Data tables and rollups carry both `profile_id` and `device_id`.

Global imported IDs must include the profile dimension. This prevents two
macOS accounts with similar source IDs from colliding:

```text
profile_id + source + source_event_id
```

## Schema Scope

Add `profile_id` and `device_id` to:

- `import_sources`
- `import_files`
- `sessions`
- `runs`
- `turns`
- `run_steps`
- `llm_calls`
- `tool_calls`
- `run_signals`
- `skill_events`
- usage rollups

Existing single-account installs should migrate into a default local profile.

## Onboarding

Default first run remains single-account mode.

Shared mode is explicit:

```text
Enable Shared Workspace
-> create /Users/Shared/Shirabe
-> create or reuse workspace.json
-> create or migrate catalog.sqlite
-> register current device
-> register current macOS account as a profile
-> start shared server
-> run current account collector
```

CLI entrypoint:

```bash
shirabe workspace init
SHIRABE_DIR=/Users/Shared/Shirabe shirabe serve
```

Current-account collection:

```bash
shirabe collect --workspace /Users/Shared/Shirabe
```

Other macOS accounts use:

```text
Join Shared Workspace
-> open /Users/Shared/Shirabe/workspace.json
-> register this macOS account as another profile
-> connect to shared server or start it if offline
-> run this account's collector
```

CLI entrypoint:

```bash
shirabe workspace repair
shirabe collect --workspace /Users/Shared/Shirabe
SHIRABE_DIR=/Users/Shared/Shirabe shirabe serve
```

## Permissions

Normal flow should not require admin privileges.

`/Users/Shared` is intended for cross-account data. The app should create
`/Users/Shared/Shirabe` with permissions that allow all local accounts to:

- connect to shared config
- write inbox files
- write logs
- take over server ownership when no server is running

The shared database is still protected by the single-writer server lock. Do not
let multiple servers write it directly.

Use an advisory lock for `server.lock`; the PID file content is informational.
The lock file may remain after a crash, but the OS lock is released when the
process exits.

Collectors do not write `catalog.sqlite`; they only write `.jsonl` event inbox
files. The server drains those files and then deletes them after successful
projection.

Collectors also write their own `profiles/` sidecar. It contains IDs and display
labels only, not prompts, responses, or tool output.

Keep the workspace root sticky-writable, but keep writable child directories
such as `inbox/`, `profiles/`, `collector-state/`, and `logs/` non-sticky so
the active server can drain files created by another local account.

Collector watermarks live under `collector-state/` by profile and source. They
avoid full rescans after the first collection while keeping a short overlap for
late file updates.

If shared directory creation or permission repair fails:

- show a clear error
- offer a "Repair Shared Workspace Permissions" action
- only then ask for administrator credentials

The non-admin account should not need admin privileges after the workspace has
been created correctly.

Implementation note: use a small privileged repair path only for creating or
fixing `/Users/Shared/Shirabe`. Do not run the main server or collector with
elevated privileges.

## API And UI

API filters add:

```text
profile_id
device_id
```

UI defaults:

- Usage page: `All accounts`.
- Menu bar: current account first, with an `All accounts` option.
- Session and run detail: show account label in metadata.
- Filters display labels, not IDs.

For V1, account filtering is required. Device filtering can be present in the
API and data model, but can stay secondary in the UI until cross-device sync is
implemented.

## Implementation Steps

1. Add shared workspace config and identity resolution.
2. Add `devices` and `profiles` tables.
3. Add `profile_id` and `device_id` columns to core data tables.
4. Backfill existing local data into a default profile and device.
5. Include profile/device in normalized events and projected IDs.
6. Update importer state to track files per profile.
7. Update rollups to group by profile/device.
8. Add API profile/device filter support.
9. Add account options to API responses.
10. Add account filter controls to Usage and menu bar UI.
11. Add shared workspace enable/join commands or settings entrypoints.
12. Add server lock behavior for shared workspaces.
13. Add permission creation/repair handling for `/Users/Shared/Shirabe`.

## Tests

Add coverage for:

- Migrating an existing single-account database into a default profile.
- Importing the same source path under two profiles without ID collisions.
- Rollups with all accounts.
- Rollups filtered by one profile.
- Session and run lists filtered by one profile.
- Shared workspace path resolution.
- Permission error reporting when shared workspace setup cannot write.
- Server lock behavior allows one writer and rejects a second writer.

Manual verification:

- Account A enables shared workspace and imports local data.
- Account B joins shared workspace as a non-admin account.
- Account B imports its local data.
- Account A UI shows `All accounts` totals and Account B filter.
- Account B UI shows `All accounts` totals and Account A filter.
- Quitting Account A server allows Account B to become server owner.

## Not In V1

- Cloud database.
- Cross-device sync.
- Background launch daemon.
- Privileged long-running helper.
- User-facing shard management.
