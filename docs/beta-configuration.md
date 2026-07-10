# Beta Setup And Configuration

## Downloads

GitHub beta releases ship two artifacts:

- `shirabe-v<version>-<target>.tar.gz`: CLI/server plus the built web UI.
- `ShirabeBar-v<version>-<target>.dmg`: macOS menu bar app with the server and UI bundled.

The DMG app starts its bundled Shirabe server when no server is already running
on `127.0.0.1:7778`.

## Unsigned Builds

The beta builds are unsigned. macOS may warn that the developer cannot be
verified.

For the menu bar app, open it with Control-click, then Open.

CLI downloads may inherit the same quarantine flag. Remove it after extracting
if macOS blocks `bin/shirabe`:

```bash
xattr -dr com.apple.quarantine shirabe-v<version>-aarch64-apple-darwin
```

Wider distribution should use Developer ID signing and Apple notarization:

```bash
SIGN_IDENTITY="Developer ID Application: Example (TEAMID)" \
NOTARIZE=1 \
NOTARY_PROFILE=shirabe-notary \
just package-macos
```

## Configuration

By default Shirabe stores data in `~/.shirabe`:

```text
~/.shirabe/catalog.sqlite
~/.shirabe/config.json
```

Override the data directory with `SHIRABE_DIR`:

```bash
SHIRABE_DIR=/path/to/data shirabe init
```

Importer source paths default to local agent data under your home directory:

- Codex: `~/.codex/sessions`
- pi: `~/.pi/agent/sessions`
- Claude Code: `~/.claude/projects`
- Kanade: `~/.kanade/traces`

Configure source paths in `$SHIRABE_DIR/config.json`. With the default data
directory, that file is `~/.shirabe/config.json`:

```json
{
  "sources": {
    "codex": "~/.codex/sessions",
    "pi": "~/.pi/agent/sessions",
    "claude": "~/.claude/projects",
    "kanade": "~/.kanade/traces"
  }
}
```

Override a one-off import with `--path`:

```bash
shirabe import codex --path /path/to/codex/sessions
```

Environment variables are also supported. Exact session/project paths take
precedence over root directories:

```bash
SHIRABE_CODEX_SESSIONS_DIR=/path/to/sessions
SHIRABE_PI_DIR=/path/to/.pi
SHIRABE_CLAUDE_DIR=/path/to/.claude
SHIRABE_KANADE_DIR=/path/to/.kanade
```

## Pricing Refresh

The server checks pricing on startup, during each automatic five-minute sync,
and on each manual sync. It downloads the public LiteLLM pricing snapshot when
the cache is missing or more than 24 hours old. A pricing download failure does
not block local usage imports; the next sync retries it.
