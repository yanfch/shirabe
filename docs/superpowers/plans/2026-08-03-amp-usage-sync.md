# Amp Usage Synchronization Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Import usage from all personal Amp threads, including archived threads, through the Amp CLI with a privacy-preserving local-file fallback and per-device source controls.

**Architecture:** A new `importers::amp` adapter owns Amp CLI discovery, paginated thread listing, export parsing, local JSON fallback, normalized-event creation, schema-transition reconciliation, and incremental import metadata. Existing configuration, CLI dispatch, automatic sync, workspace collection, rollups, and UI source filters remain the orchestration and presentation paths.

**Tech Stack:** Rust 2024, serde/serde_json, std::process, rusqlite, walkdir, existing Shirabe normalized-event/projector pipeline and LiteLLM pricing cache.

---

## File Structure

- Create `src/importers/amp.rs`: Amp schemas, usage parser, CLI table parser and process runner, local-file fallback, import/collect entry points, focused tests.
- Modify `src/importers/mod.rs`: register the Amp adapter.
- Modify `src/config.rs`: Amp local path, device-local disabled-source resolution, environment override, tests.
- Modify `src/cli.rs`: expose `shirabe import amp`.
- Modify `src/main.rs`: explicit import dispatch and pass source settings into serve/collect orchestration.
- Modify `src/server.rs`: include enabled Amp in automatic/manual sync and report skipped disabled sources.
- Modify `src/workspace.rs`: collect enabled Amp usage while respecting device-local source controls.
- Modify `README.md` and `docs/beta-configuration.md`: source location, CLI-first behavior, privacy boundary, limitations, and disabling examples.

### Task 1: Device-local Amp configuration and source controls

**Files:**
- Modify: `src/config.rs`

- [ ] **Step 1: Write failing configuration tests**

Add tests that deserialize a local config, normalize source names, and verify environment replacement through a pure helper rather than mutating process-global environment in parallel tests:

```rust
#[test]
fn resolves_disabled_sources_from_config() {
    let config = ConfigFile {
        disabled_sources: vec![" AMP ".into(), "claude".into(), "amp".into()],
        ..ConfigFile::default()
    };
    let settings = SourceSettings::from_values(config.disabled_sources, None);
    assert!(!settings.is_enabled("amp"));
    assert!(!settings.is_enabled("claude"));
    assert!(settings.is_enabled("codex"));
}

#[test]
fn environment_disabled_sources_replace_config() {
    let settings = SourceSettings::from_values(
        vec!["amp".into()],
        Some(" pi, CLAUDE,unknown ".into()),
    );
    assert!(settings.is_enabled("amp"));
    assert!(!settings.is_enabled("pi"));
    assert!(!settings.is_enabled("claude"));
    assert_eq!(settings.unknown_sources(), &["unknown"]);
}

#[test]
fn empty_environment_override_enables_all_sources() {
    let settings = SourceSettings::from_values(vec!["amp".into()], Some(String::new()));
    assert!(settings.is_enabled("amp"));
}
```

- [ ] **Step 2: Run the tests and verify the missing types fail compilation**

Run: `cargo test config::tests::resolves_disabled_sources_from_config -- --exact`

Expected: compilation fails because `SourceSettings` and `disabled_sources` do not exist.

- [ ] **Step 3: Implement configuration resolution**

Add `source_settings` to `Paths`, add Amp-specific fallback roots to `SourcePaths`, and implement a normalized set with a fixed allowlist:

```rust
const KNOWN_SOURCES: [&str; 5] = ["codex", "pi", "claude", "kanade", "amp"];

#[derive(Debug, Clone, Default)]
pub struct SourceSettings {
    disabled: HashSet<String>,
    unknown: Vec<String>,
}

impl SourceSettings {
    pub(crate) fn from_values(configured: Vec<String>, environment: Option<String>) -> Self {
        let values = environment
            .map(|value| value.split(',').map(str::to_owned).collect())
            .unwrap_or(configured);
        let mut disabled = HashSet::new();
        let mut unknown = Vec::new();
        for value in values {
            let value = value.trim().to_ascii_lowercase();
            if value.is_empty() {
                continue;
            }
            if KNOWN_SOURCES.contains(&value.as_str()) {
                disabled.insert(value);
            } else if !unknown.contains(&value) {
                unknown.push(value);
            }
        }
        Self { disabled, unknown }
    }

    pub fn is_enabled(&self, source: &str) -> bool {
        !self.disabled.contains(&source.to_ascii_lowercase())
    }

    pub fn unknown_sources(&self) -> &[String] {
        &self.unknown
    }
}
```

Represent `SourcePaths.amp` as `Vec<PathBuf>`. Deserialize `disabled_sources` with `#[serde(default)]`, use `SHIRABE_DISABLED_SOURCES` as the replacement value, and resolve Amp local roots in strict precedence order: configured `sources.amp`; `SHIRABE_AMP_THREADS_DIR`; every comma-separated `AMP_DATA_DIR` entry with `threads` appended; `~/.local/share/amp/threads`. Trim/drop empty roots and deduplicate paths. Add a test in which two `AMP_DATA_DIR` values each contribute a distinct root without mutating process environment by testing the pure resolver directly.

- [ ] **Step 4: Run focused and complete configuration tests**

Run: `cargo test config::tests`

Expected: all configuration tests pass.

- [ ] **Step 5: Commit**

```bash
git add src/config.rs
git commit -m "feat: add per-device source controls"
```

### Task 2: Parse current and legacy Amp usage without retaining content

**Files:**
- Create: `src/importers/amp.rs`
- Modify: `src/importers/mod.rs`

- [ ] **Step 1: Write failing parser tests**

Create inline fixtures containing sentinel private text and assert exact normalized token buckets:

```rust
#[test]
fn parses_current_message_usage_as_exclusive_buckets() -> Result<()> {
    let raw = r#"{
      "id":"T-current","title":"PRIVATE TITLE",
      "messages":[
        {"role":"user","content":[{"type":"text","text":"PRIVATE PROMPT"}]},
        {"role":"assistant","messageId":7,"content":[{"type":"text","text":"PRIVATE ANSWER"}],
         "usage":{"model":"claude-opus-4-1","timestamp":"2026-08-03T10:00:00Z",
          "inputTokens":5,"outputTokens":11,"cacheCreationInputTokens":20,
          "cacheReadInputTokens":30,"totalInputTokens":55,"maxInputTokens":200000}}
      ]
    }"#;
    let parsed = parse_thread(raw.as_bytes(), &ImportIdentity::local(), "fixture")?;
    assert_eq!(parsed.events.len(), 1);
    let event = &parsed.events[0];
    assert_eq!(event.source_event_id.as_deref(), Some("amp:T-current:message:7"));
    assert_eq!(event.usage.input_tokens, 5);
    assert_eq!(event.usage.uncached_input_tokens, 5);
    assert_eq!(event.usage.output_tokens, 11);
    assert_eq!(event.usage.cache_write_tokens, 20);
    assert_eq!(event.usage.cache_read_tokens, 30);
    assert_eq!(event.usage.model_context_window, Some(200000));
    let serialized = serde_json::to_string(event)?;
    assert!(!serialized.contains("PRIVATE"));
    Ok(())
}

#[test]
fn empty_ledger_falls_back_to_message_usage() -> Result<()> {
    let raw = br#"{"id":"T-empty","usageLedger":{"events":[]},"messages":[
      {"role":"assistant","messageId":"m1","usage":{"model":"gpt-5.6",
       "timestamp":"2026-08-03T10:00:00Z","inputTokens":3,"outputTokens":4}}
    ]}"#;
    assert_eq!(parse_thread(raw, &ImportIdentity::local(), "fixture")?.events.len(), 1);
    Ok(())
}

#[test]
fn legacy_ledger_joins_message_cache_tokens() -> Result<()> {
    let raw = br#"{"id":"T-ledger","messages":[
      {"role":"assistant","messageId":2,"usage":{"cacheCreationInputTokens":13,"cacheReadInputTokens":17}}
    ],"usageLedger":{"events":[{"id":"e1","timestamp":"2026-08-03T10:00:00Z",
      "model":"claude-opus-4-1","tokens":{"input":5,"output":7,"total":42},"toMessageId":2}]}}"#;
    let event = parse_thread(raw, &ImportIdentity::local(), "fixture")?.events.remove(0);
    assert_eq!(event.source_event_id.as_deref(), Some("amp:T-ledger:ledger:e1"));
    assert_eq!((event.usage.input_tokens, event.usage.output_tokens), (5, 7));
    assert_eq!((event.usage.cache_write_tokens, event.usage.cache_read_tokens), (13, 17));
    Ok(())
}
```

- [ ] **Step 2: Run parser tests and verify failure**

Run: `cargo test importers::amp::tests`

Expected: compilation fails because the Amp module and parser do not exist.

- [ ] **Step 3: Implement privacy-bounded schema and event conversion**

Use typed structs that name only required fields and rely on Serde's unknown-field skipping:

```rust
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct AmpThread {
    id: String,
    #[serde(default)]
    messages: Vec<AmpMessage>,
    #[serde(default, deserialize_with = "deserialize_lenient_ledger")]
    usage_ledger: Option<UsageLedger>,
    updated_at: Option<ScalarTimestamp>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct AmpMessage {
    role: Option<String>,
    message_id: Option<ScalarId>,
    timestamp: Option<ScalarTimestamp>,
    model: Option<String>,
    usage: Option<MessageUsage>,
}

#[derive(Deserialize, Default)]
#[serde(rename_all = "camelCase")]
struct MessageUsage {
    model: Option<String>,
    timestamp: Option<ScalarTimestamp>,
    input_tokens: Option<i64>,
    output_tokens: Option<i64>,
    cache_creation_input_tokens: Option<i64>,
    cache_read_input_tokens: Option<i64>,
    total_input_tokens: Option<i64>,
    total_tokens: Option<i64>,
    max_input_tokens: Option<i64>,
}
```

`ScalarId` accepts only strings and non-negative integers and canonicalizes numeric/string forms. `ScalarTimestamp` accepts only ISO strings or integer milliseconds and immediately converts to nanoseconds. The lenient ledger deserializer consumes malformed ledger values without failing the enclosing thread and records only a boolean diagnostic. Validate `T-...` thread IDs, reject negative token counts, and never format malformed values into IDs, metadata, warnings, or logs.

Build one `NormalizedEvent` per usable current message or ledger event. Parsing is mutually exclusive: if at least one usable ledger event exists, emit only ledger events and return the current-message source IDs superseded by the ledger; otherwise emit current message events. Set session external ID to the Amp thread ID, operation type to `LlmCall`, pricing status to `estimated_from_model_prices`, cost confidence to `estimated`, and metadata only to count/boolean schema diagnostics.

- [ ] **Step 4: Add mismatch, aggregate fallback, malformed ledger, and deterministic fallback-ID tests**

Assert that split buckets win on aggregate mismatch, aggregate-only records become uncached input at lower event confidence, malformed ledger uses message records, a usable ledger suppresses message events, numeric/string IDs canonicalize identically, negative/all-zero usage is skipped, missing message IDs produce the same hash across repeated parses, and nested sentinel values in identity/timestamp fields never appear in serialized output.

- [ ] **Step 5: Run parser tests**

Run: `cargo test importers::amp::tests`

Expected: all Amp parser tests pass.

- [ ] **Step 6: Commit**

```bash
git add src/importers/amp.rs src/importers/mod.rs
git commit -m "feat: parse Amp thread usage"
```

### Task 3: Local-file import, projection, and update semantics

**Files:**
- Modify: `src/importers/amp.rs`

- [ ] **Step 1: Write failing local importer integration tests**

Write two JSON thread files, import them into a temporary database, and assert sessions/calls/tokens. Rewrite one message with larger finalized counters and assert the existing call is updated rather than duplicated. Then replace a message-only payload with a usable ledger and assert the superseded message call is removed before the ledger call is projected:

```rust
import_local_with_identity(&db, root.clone(), &ImportIdentity::local())?;
assert_eq!(count(&db, "sessions")?, 2);
assert_eq!(count(&db, "llm_calls")?, 2);

write_thread(&first, "T-one", 7, 50, 20)?;
import_local_with_identity(&db, root.clone(), &ImportIdentity::local())?;
assert_eq!(count(&db, "llm_calls")?, 2);
let finalized: (i64, i64) = db.connection().query_row(
    "SELECT input_tokens, output_tokens FROM llm_calls WHERE source = 'amp' AND session_id IS NOT NULL ORDER BY input_tokens DESC LIMIT 1",
    [],
    |row| Ok((row.get(0)?, row.get(1)?)),
)?;
assert_eq!(finalized, (50, 20));

write_ledger_thread(&first, "T-one", "ledger-7", 50, 20)?;
import_local_with_identity(&db, first.clone(), &ImportIdentity::local())?;
assert_eq!(count_source_calls(&db, "amp")?, 2);
assert_eq!(count_source_identity(&db, "amp:T-one:message:7")?, 0);
assert_eq!(count_source_identity(&db, "amp:T-one:ledger:ledger-7")?, 1);
```

- [ ] **Step 2: Run the update test and verify failure**

Run: `cargo test importers::amp::tests::local_reimport_updates_usage_without_duplication -- --exact`

Expected: fails because local scanning and projection entry points are missing.

- [ ] **Step 3: Implement local scanning and projection**

Implement explicit local entry points so a user-supplied path can never invoke the network:

```rust
pub fn import_local_with_identity(db: &Database, path: PathBuf, identity: &ImportIdentity) -> Result<ImportReport>;
pub fn import_local_recent_with_identity(db: &Database, roots: &[PathBuf], modified_since_ns: i64, identity: &ImportIdentity) -> Result<ImportReport>;
pub fn collect_local_recent(roots: &[PathBuf], identity: &ImportIdentity, writer: &mut dyn Write, modified_since_ns: i64) -> Result<ImportReport>;
```

Scan `.json` files recursively without following links, use file size/mtime through `prepare_import_file`, parse from `BufReader`, project with `ProjectionCache`, refresh touched run/session summaries, finish import-file status, and produce count-only warnings. Use the same source event IDs in import and collector paths. After a complete parse succeeds, transactionally delete only the returned superseded Amp message `run_steps`, `llm_calls`, and event identities before projecting ledger events; never reconcile after a partial/failed parse or because a thread disappeared. Assert model, timestamp, context window, and all token buckets are replaced on re-import; update the projector conflict clause if the timestamp assertion exposes its current omission.

- [ ] **Step 4: Run local importer tests**

Run: `cargo test importers::amp::tests`

Expected: parser and local projection tests pass.

- [ ] **Step 5: Commit**

```bash
git add src/importers/amp.rs
git commit -m "feat: import local Amp usage"
```

### Task 4: CLI-first enumeration, incremental export, and fallback

**Files:**
- Modify: `src/importers/amp.rs`
- Modify: `src/db.rs`

- [ ] **Step 1: Write failing table-parser and fake-CLI tests**

Use a fake executable that validates archived pagination arguments and emits Unicode titles with stable trailing columns:

```rust
#[test]
fn parses_amp_thread_table_by_trailing_fields() -> Result<()> {
    let output = "Title  Last Updated  Visibility  Messages  Thread ID\n\
                  ─────  ────────────  ──────────  ────────  ─────────\n\
                  中文 标题  2m ago  Private  9  T-019fc844-d527-709c-98a1-631bc89946ff\n";
    let rows = parse_thread_list(output)?;
    assert_eq!(rows[0].message_count, 9);
    assert_eq!(rows[0].id, "T-019fc844-d527-709c-98a1-631bc89946ff");
    Ok(())
}
```

The fake CLI tests must assert: a 101-thread listing requests offsets 0 and 100 and includes all archived fixtures; second sync exports no stale unchanged fixture; a recently active fixture is refreshed; failed and crash-like pending exports retry; one failed export does not block another; a malformed candidate row fails the complete listing instead of truncating pagination; and list failure imports fixtures from two local fallback roots.

- [ ] **Step 2: Run CLI tests and verify failure**

Run: `cargo test importers::amp::tests::parses_amp_thread_table_by_trailing_fields -- --exact`

Expected: fails because CLI list parsing is not implemented.

- [ ] **Step 3: Add import-file metadata accessors**

In `src/db.rs`, add a typed view over Amp virtual thread records without adding a new table:

```rust
pub struct AmpVirtualImportState {
    pub status: String,
    pub message_count: usize,
    pub updated_at_ns: Option<i64>,
    pub payload_fingerprint: Option<String>,
    pub event_count: usize,
}

pub fn amp_virtual_import_state(&self, source_id: &str, path: &Path) -> Result<Option<AmpVirtualImportState>>;
pub fn upsert_virtual_import_file(
    &self,
    source_id: &str,
    path: &Path,
    fingerprint: &str,
    modified_ns: i64,
    status: &str,
    event_count: usize,
    metadata_json: &str,
) -> Result<String>;
```

Use the same path hashing and profile/device lookup as physical `import_files`. Store only message count, parsed `updatedAt` nanoseconds, payload hash, import mode, and count diagnostics. Mark a selected thread pending while preserving prior successful metadata; mark imported/partial only after projection commits; mark failed without replacing the successful fingerprint/timestamp/event count. Both pending and failed are mandatory retry states.

- [ ] **Step 4: Implement the CLI runner**

Define an injected `AmpCommandRunner` so tests do not mutate process-global environment. Production resolution checks `AMP_CLI`, PATH, then `~/.amp/bin/amp`. Execute list pages with `--include-archived --limit 100 --offset N`, stop on an empty page or a page with fewer than 100 candidate data rows, and advance by candidate-row count. Parse every candidate from the final thread ID and preceding numeric message count; if any candidate row is malformed, fail the complete CLI listing and use local fallback rather than silently omitting later pages.

Expose distinct CLI-first entry points used by explicit import, server sync, and workspace collection:

```rust
pub fn sync_cli_first_with_identity(db: &Database, fallback_roots: &[PathBuf], identity: &ImportIdentity) -> Result<ImportReport>;
pub fn sync_cli_first_recent_with_identity(db: &Database, fallback_roots: &[PathBuf], modified_since_ns: i64, identity: &ImportIdentity) -> Result<ImportReport>;
pub fn collect_cli_first_recent(fallback_roots: &[PathBuf], state: &mut AmpCollectorState, identity: &ImportIdentity, writer: &mut dyn Write) -> Result<ImportReport>;
```

For each selected thread, spawn `amp threads export <id>` with a bounded deadline and output limit. Drain stderr concurrently to `io::sink()`; parse stdout through a capped, hashing reader; on deadline or 64 MiB overflow kill and reap the child. Return errors containing only command kind, exit status, and validated thread ID. Inject deadlines/limits for timeout, overflow, and stderr-flood tests. Export new threads, changed message counts, pending/failed records, and records whose parsed `updatedAt` is less than 15 minutes old.

- [ ] **Step 5: Implement fallback and mode-safe deduplication**

When CLI resolution/listing/auth/network fails, invoke the physical local scanner. Resolve fallback roots from configured `sources.amp`, otherwise `SHIRABE_AMP_THREADS_DIR`, otherwise every comma-separated `AMP_DATA_DIR` root plus `threads`, otherwise `~/.local/share/amp/threads`. Preserve CLI-imported records and use identical canonical event identities across modes. Do not fall back merely because one export fails.

- [ ] **Step 6: Run Amp and database tests**

Run: `cargo test importers::amp::tests && cargo test db::tests`

Expected: CLI parsing, incremental export, fallback, update, and database tests pass.

- [ ] **Step 7: Commit**

```bash
git add src/importers/amp.rs src/db.rs
git commit -m "feat: sync Amp usage through CLI"
```

### Task 5: Register Amp across CLI, server sync, and workspace collection

**Files:**
- Modify: `src/cli.rs`
- Modify: `src/main.rs`
- Modify: `src/server.rs`
- Modify: `src/workspace.rs`

- [ ] **Step 1: Write failing orchestration tests**

Add tests around pure source selection helpers:

```rust
#[test]
fn enabled_sync_sources_skip_device_disabled_amp() {
    let settings = SourceSettings::from_values(vec!["amp".into()], None);
    assert_eq!(enabled_sync_sources(&settings), ["codex", "pi", "claude", "kanade"]);
}

#[test]
fn amp_is_enabled_by_default() {
    assert!(enabled_sync_sources(&SourceSettings::default()).contains(&"amp"));
}
```

Extend the workspace collector test to pass settings with Amp disabled and assert no Amp event is written even when a local fixture exists.

- [ ] **Step 2: Run focused tests and verify failure**

Run: `cargo test enabled_sync_sources -- --nocapture`

Expected: fails because orchestration helpers do not include settings.

- [ ] **Step 3: Wire explicit import and automatic sync**

Add `Amp` to `ImportSource` and make dispatch explicit:

- `shirabe import amp --path <path>` calls local-only import and bypasses disablement;
- `shirabe import amp` calls CLI-first sync with configured roots as fallback and bypasses disablement;
- automatic/global sync calls CLI-first unless Amp is disabled;
- one failed thread export does not change the complete run to fallback mode.

Pass `SourceSettings` into `server::serve`, store it in `AppState`, iterate only enabled sources, and add the Amp match arm:

```rust
"amp" => importers::amp::sync_cli_first_recent_with_identity(
    db,
    &source_paths.amp,
    modified_since_ns,
    &import_identity,
),
```

Log unknown disabled source names once at startup without failing.

- [ ] **Step 4: Wire workspace collection**

Pass `SourceSettings` into `collect_to_inbox`, build the report vector only from enabled collectors, and invoke `importers::amp::collect_cli_first_recent` for Amp. Extend serde-defaulted `CollectorState` with an Amp per-thread map containing message count, parsed `updatedAt`, fingerprint, event count, and status. Update Amp state only after successful parsing and inbox publication; pending/failed entries retry. Disabled Amp must not invoke the runner or modify Amp state/watermarks. Add tests for CLI-first collection, incremental retry, disabled runner non-invocation, and `--path` local-only dispatch.

Add optional content-free fields to `ImportReport` and carry them through `SyncSourceReport`: `mode` (`cli`, `local_fallback`, or `local_explicit`), `threads_enumerated`, `threads_exported`, and `unchanged_threads_skipped`. Assert fallback mode is visible in direct import, sync status, and collector reports.

- [ ] **Step 5: Run orchestration and complete Rust tests**

Run: `cargo test`

Expected: all tests pass, including existing 39 baseline tests and new Amp/configuration tests.

- [ ] **Step 6: Commit**

```bash
git add src/cli.rs src/main.rs src/server.rs src/workspace.rs
git commit -m "feat: integrate Amp usage synchronization"
```

### Task 6: Document operation and privacy behavior

**Files:**
- Modify: `README.md`
- Modify: `docs/beta-configuration.md`

- [ ] **Step 1: Update supported sources and configuration examples**

Add Amp to the README source list with `~/.local/share/amp/threads` as fallback, and extend the configuration example:

```json
{
  "disabled_sources": ["amp"],
  "sources": {
    "amp": "~/.local/share/amp/threads"
  }
}
```

Document `SHIRABE_DISABLED_SOURCES=amp,claude`, environment replacement semantics, per-device behavior in shared workspaces, and that explicit `shirabe import amp` remains available.

- [ ] **Step 2: Document CLI-first privacy and accuracy boundaries**

State that CLI sync includes archived personal threads, a complete export transiently crosses a local process pipe, only usage fields are retained, local fallback is incomplete, source-reported tokens drive LiteLLM estimates, and Amp credits/BYOK charges are not imported.

- [ ] **Step 3: Check documentation and formatting**

Run: `git diff --check`

Expected: no whitespace errors.

- [ ] **Step 4: Commit**

```bash
git add README.md docs/beta-configuration.md
git commit -m "docs: explain Amp usage synchronization"
```

### Task 7: Final verification and privacy audit

**Files:**
- Verify all modified files.

- [ ] **Step 1: Format and run full repository checks**

Run: `cargo fmt --check && cargo test && just check`

Expected: formatter, Rust tests, UI checks, and repository checks pass.

- [ ] **Step 2: Run the fake-CLI end-to-end test in isolation**

Run the integration-style test that creates a temporary fake Amp executable, imports its list/export output into SQLite, and inspects projected calls:

```bash
cargo test importers::amp::tests::fake_cli_syncs_archived_threads_incrementally -- --exact --nocapture
```

Expected: the test passes; its report has source `amp` and CLI mode counts, `llm_calls` contains exclusive token buckets, the second sync skips stale unchanged threads, and no private fixture sentinel is persisted.

- [ ] **Step 3: Audit persistence and logs for content fields**

Search Amp normalized-event construction, import metadata, warning strings, and test database values for `content`, `thinking`, `tool`, `title`, `creatorUserID`, and fixture sentinel strings. Expected: these names appear only in ignored input fixtures/documentation, never persisted output paths.

- [ ] **Step 4: Review diff against design scope**

Run: `git diff main...HEAD --stat && git log --oneline main..HEAD`

Expected: changes are limited to the Amp adapter, source configuration/orchestration, database virtual import metadata support, tests, and documentation.

- [ ] **Step 5: Commit any verification-only corrections**

```bash
git add -A
git commit -m "test: verify Amp usage synchronization"
```

Skip this commit when verification required no file changes.
