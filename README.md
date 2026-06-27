# Shirabe

Local-first usage and tracing console for personal AI coding workflows.

Shirabe imports local agent logs into SQLite, normalizes them into common
operation records, computes usage rollups, and exposes both a full web dashboard
and a compact macOS menu bar view.

## Screenshots

| Usage Dashboard | macOS Menu Bar |
| --- | --- |
| <a href="script/usage.png"><img src="script/usage.png" alt="Usage dashboard" width="520"></a> | <a href="script/menubar.png"><img src="script/menubar.png" alt="macOS menu bar usage panel" width="260"></a> |

## Install

Download the latest beta from [GitHub Releases](https://github.com/yanfch/shirabe/releases).

- `ShirabeBar-*.dmg`: macOS menu bar app with the server and UI bundled.
- `shirabe-*.tar.gz`: CLI/server package for running the web dashboard.

The beta builds are unsigned. macOS may ask you to confirm before opening them.
See [beta setup and configuration](docs/beta-configuration.md) for details.

## Run

Use the macOS menu bar app by opening the DMG and dragging `ShirabeBar.app` into
Applications. The app starts a bundled local server when needed.

Use the CLI package without the menu bar app:

```bash
tar -xzf shirabe-v0.0.1-aarch64-apple-darwin.tar.gz
cd shirabe-v0.0.1-aarch64-apple-darwin
./bin/shirabe init
./bin/shirabe serve --ui-dir ./ui/dist
```

Then open:

```text
http://127.0.0.1:7778
```

## Sources

Supported local sources:

- Codex: `~/.codex/sessions`
- pi: `~/.pi/agent/sessions`
- Claude Code: `~/.claude/projects`
- Kanade: `~/.kanade/traces`

Override source paths with `~/.shirabe/config.json`. See
[configuration](docs/beta-configuration.md#configuration).

## Development

Prerequisites:

- Rust stable
- Node.js 22+
- macOS and SwiftPM for the menu bar app
- `just`

Useful commands:

```bash
just init
just import-all
just restart
just macos-run
just check
just package
```

## Layout

```text
src/        Rust CLI, importers, database, rollups, and local API server
ui/         Svelte dashboard and menu bar web views
app/macos/  native macOS menu bar shell
script/     build, package, and diagnostic helpers
```

Shirabe is local-only today. It does not upload agent logs or usage data.

## License

MIT. See [LICENSE](LICENSE).
