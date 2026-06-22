# shirabe（調べ）

> TraceLocal for personal AI operations.

shirabe is a local-first tracing and diagnostics console for personal AI work:
LLM calls, system prompts, tool calls, cache hits, retries, token waste, latency,
cost, and cross-tool traces.

The first target integrations are:

- `pi` agent sessions
- `kanade` workflow tasks and subagents
- `tsutae` STT/TTS/VAD and voice dispatch flows

## Direction

shirabe is not a prompt management platform, eval platform, or cloud LLMOps tool.
It focuses on collecting and analyzing local AI operation data so we can answer:

- Which tool calls fail often and waste tokens?
- Which system prompts or roles are expensive or unstable?
- Where does latency come from in a full trace?
- How often do provider caches and local caches actually hit?
- Which models, agents, and workflows are worth their cost?

## Architecture

Initial shape:

```text
tracelocald / shirabed
  Rust server
  OTLP / local event ingest
  SQLite storage
  rollup metrics
  Web API
  serves Web UI

web
  browser-first dashboard
  trace tree / waterfall
  tool success analysis
  cache and prompt analysis

optional macOS shell
  SwiftUI menubar app
  starts/stops server
  opens the same Web UI
```

The server must run independently. The macOS app is only a later convenience
shell, not part of the core data pipeline.

See [docs/01-v1-design.md](docs/01-v1-design.md).

Current design notes:

- [AI operations model](docs/02-ai-operations-model.md)
- [Projection pipeline](docs/04-projection-pipeline.md)
- [Importers](docs/06-importers.md)
- [Source shapes and import plan](docs/10-source-shapes-and-import-plan.md)
- [Agent handoff](docs/11-agent-handoff.md)
