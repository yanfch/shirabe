# Amp Usage Synchronization Design

## Goal

Add Amp as a Shirabe usage source while preserving Shirabe's local-first privacy boundary and its existing token-based cost model.

The importer must:

- synchronize personal Amp threads, including archived threads;
- prefer the signed-in Amp CLI so threads not present in the local cache can be included;
- fall back to local Amp thread files when CLI synchronization is unavailable;
- store only usage metadata, never conversation or tool content;
- use source-reported token counts and Shirabe's LiteLLM pricing cache for estimated cost;
- feed the existing normalized-event, SQLite, rollup, workspace collector, and dashboard paths.

This is a best-effort personal usage view, not an audit-grade reproduction of Amp billing. Amp's reported credit charges and customer-managed provider billing are outside this feature's cost model.

## Source Discovery

### CLI-first mode

Resolve the Amp executable in this order:

1. `AMP_CLI` environment variable;
2. `amp` on `PATH`;
3. `~/.amp/bin/amp`.

Enumerate threads with paginated invocations of:

```text
amp threads list --include-archived --limit <page-size> --offset <offset>
```

The current command emits a table rather than JSON. Parse rows from the stable trailing fields: numeric message count and `T-...` thread ID. Titles and relative `Last Updated` text must not be persisted or used as stable cursors. A non-empty, unparseable page is an error rather than a successful empty result.

For a new or changed thread, invoke:

```text
amp threads export <thread-id>
```

The export payload contains the complete thread. Shirabe must stream it directly from the child process and deserialize only structural and usage fields. Message content, thinking, tool payloads, environment data, titles, and creator identity must be skipped during deserialization and must never be written to a temporary file, normalized event, log, warning, or database column.

### Local fallback mode

If the Amp executable cannot be found, the user is signed out, thread listing fails, the network is unavailable, or the list format cannot be safely parsed, scan:

```text
${AMP_DATA_DIR:-~/.local/share/amp}/threads/**/*.json
```

`AMP_DATA_DIR` may contain comma-separated roots for compatibility with Amp-oriented tooling. A configured Shirabe Amp source path takes precedence when present.

Local files are an incomplete machine-specific cache. Fallback mode must be identified in the sync report and must not remove previously imported CLI data. CLI and local parsing use the same event identities so the same usage is not counted twice.

## Incremental Synchronization

Use the existing `import_sources` and `import_files` infrastructure rather than creating a second synchronization subsystem.

- Register source `amp` with source kinds `amp_cli_export` or `amp_local_thread_json`.
- Represent a CLI thread as the virtual import path `amp://thread/<thread-id>`.
- Persist message count, exported `updatedAt`, payload fingerprint, event count, and last import status in import-file metadata.
- Export a thread when it is new, its listed message count changed, its previous import failed, or its last exported `updatedAt` is within the recent-activity overlap window.
- Use a 15-minute recent-activity window so usage finalized shortly after a response can replace partial counters.
- Treat a changed payload with an unchanged message count as an update when the thread is in that recent window.
- Retain imported history when a thread disappears from the remote list or local cache. Shirabe does not interpret disappearance as deletion of usage.

The existing five-minute automatic sync and manual `/api/sync` action invoke Amp alongside the other sources. Shared-workspace collection uses the same Amp collector and normalized events.

## Supported Usage Schemas

### Current message usage

Read assistant `messages[].usage` records with these fields:

| Amp field | Shirabe mapping |
| --- | --- |
| `model` | `usage.model` |
| `timestamp`, falling back to message timestamp | event occurrence time |
| `inputTokens` | `input_tokens` and `uncached_input_tokens` |
| `outputTokens` | `output_tokens` |
| `cacheCreationInputTokens` | `cache_write_tokens` |
| `cacheReadInputTokens` | `cache_read_tokens` |
| `totalInputTokens` | validation and missing-split fallback only |
| `maxInputTokens` | `model_context_window` |

Current Amp data has been verified to report:

```text
totalInputTokens = inputTokens + cacheCreationInputTokens + cacheReadInputTokens
```

Therefore cache tokens are exclusive buckets and must not be added to `inputTokens` before storage or pricing. If all split fields are present but disagree with `totalInputTokens`, trust the split fields and emit a count-only warning without thread content. If split fields are absent, use the aggregate as uncached input and mark the event metadata as aggregate-fallback with lower confidence.

Skip non-assistant messages, usage records without a usable timestamp or model, and records whose token buckets are all zero.

### Legacy usage ledger

When `usageLedger.events` contains usable events, map each event's timestamp, model, token fields, and `toMessageId`. Join `toMessageId` to assistant message usage for cache creation and cache read counts.

Amp credits are intentionally ignored. Cost is estimated from tokens through Shirabe's pricing cache.

An absent, malformed, or empty ledger falls back to current message usage. This deliberately differs from ccusage's current empty-ledger precedence behavior, which can suppress valid message usage.

## Identity, Updates, and Cost

Use stable source event identities:

```text
amp:<thread-id>:message:<message-id>
amp:<thread-id>:ledger:<event-id>
```

If a message or ledger ID is missing, derive a deterministic fallback from the thread ID, timestamp, model, and message position. Record reduced identity confidence in metadata.

Map one Amp thread to one Shirabe session. Each usage record becomes an LLM operation/run within that session. Re-importing the same stable event reuses Shirabe's event identity and existing `llm_calls` upsert path, replacing token, model, timestamp, context-window, and estimated-cost inputs rather than adding another call.

Set source to `amp`. Infer provider only when the model identifier maps unambiguously; model pricing lookup remains model-driven when provider is unknown.

Do not call `amp threads usage`. Shirabe computes estimated cost from the exclusive input, output, cache-read, and cache-write buckets using the same LiteLLM price cache and reported/estimated precedence rules as other sources. This keeps cross-source model and time breakdowns comparable, although it will not equal Amp credit billing in every case.

## Privacy and Process Boundaries

The CLI child process necessarily returns a complete export on stdout because Amp does not expose a usage-only export. The privacy guarantee is therefore:

- full content may transiently pass through the local child-process pipe;
- the parser skips content fields without constructing or retaining their values;
- no export payload is written to disk by Shirabe;
- no content is stored in SQLite, normalized collector JSONL, logs, warnings, metrics, or sync reports;
- Shirabe continues to upload no agent logs or usage data.

Documentation must state this distinction clearly rather than claiming that Shirabe never receives full export bytes.

## Failure Handling and Reporting

Failures are isolated as follows:

- Missing CLI, authentication failure, network failure, or list failure: try local fallback.
- Non-empty list output with no safely parsed rows: mark CLI mode failed, warn, then try local fallback.
- One thread export failure: record a content-free warning, retain its previous imported data, and continue other threads.
- Malformed or unknown-schema thread: record a skipped/partial import with field-shape diagnostics only.
- Missing local fallback directory: report Amp as skipped, without failing the complete Shirabe sync.
- Child-process timeout or oversized output: terminate that import, retain previous data, and continue safely.

Extend the existing source sync report with Amp-compatible counts and metadata:

- threads enumerated;
- threads exported;
- unchanged threads skipped;
- files examined in fallback mode;
- usage events inserted or updated;
- mode: `cli` or `local_fallback`;
- content-free warnings and errors.

No new Amp-specific dashboard is required. Amp appears in the existing source filters, summaries, model tables, trends, and session detail views.

## Configuration and Documentation

Add Amp to:

- source-path configuration and environment overrides;
- CLI `import` source selection;
- automatic server synchronization;
- shared-workspace collection;
- README supported-source list;
- beta configuration documentation.

The default local path is `~/.local/share/amp/threads`, not `~/.amp`. `~/.amp` currently contains the executable and file-change cache and is not the usage source.

Document these limitations:

- CLI output and thread JSON are not stable public data contracts;
- local fallback can omit web, orb, other-machine, and otherwise non-materialized threads;
- the implementation synchronizes threads visible to the signed-in CLI account, not workspace-wide enterprise analytics;
- token counts are source-reported, while USD cost is estimated by Shirabe;
- archived threads are included; deleted or inaccessible historical data cannot be recovered unless previously imported.

## Verification

Add focused tests for:

1. current `messages[].usage` parsing;
2. legacy `usageLedger.events` parsing and cache-token joining;
3. absent, malformed, and empty ledger fallback;
4. exclusive `totalInputTokens` validation without cache double-counting;
5. aggregate-only token fallback and reduced confidence;
6. stable message and ledger identities;
7. repeated import without duplicate usage;
8. replacement of token counters when an event is updated;
9. archived-thread CLI listing, pagination, Unicode/space-containing titles, and malformed tables;
10. recent-activity refresh with unchanged message count;
11. per-thread export failure isolation;
12. CLI-to-local fallback without cross-mode duplication;
13. absence of conversation, thinking, tool, environment, title, and creator data in normalized events and persistence;
14. automatic sync and shared-workspace collector registration;
15. Amp source/model/daily rollups and LiteLLM cost estimation.

Use a fake Amp executable in integration tests. It should emit deterministic list and export fixtures so tests do not require an Amp login, network access, or real user thread content.

## Out of Scope

- Storing or displaying thread conversation content.
- Reproducing Amp credits or subscription balance.
- Calling `amp threads usage` per thread.
- Workspace-wide enterprise analytics.
- Uploading usage data to Shirabe or another service.
- An Amp-specific dashboard or billing reconciliation page.
