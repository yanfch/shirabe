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
- Amp fallback: `~/.local/share/amp/threads`

Configure source paths in `$SHIRABE_DIR/config.json`. With the default data
directory, that file is `~/.shirabe/config.json`:

```json
{
  "disabled_sources": ["amp"],
  "sources": {
    "codex": "~/.codex/sessions",
    "pi": "~/.pi/agent/sessions",
    "claude": "~/.claude/projects",
    "kanade": "~/.kanade/traces",
    "amp": "~/.local/share/amp/threads"
  }
}
```

`disabled_sources` controls automatic sync, global Sync, and shared-workspace
collection on this device. This makes it possible, for example, to collect Amp
on one computer but not another. In a shared workspace, each device reads this
setting from its own `~/.shirabe/config.json`; disabling a source does not delete
usage imported previously.

The environment variable `SHIRABE_DISABLED_SOURCES` replaces the configured
list for the current process. Values are comma-separated; an explicitly empty
value enables every source:

```bash
SHIRABE_DISABLED_SOURCES=amp,claude shirabe serve
SHIRABE_DISABLED_SOURCES= shirabe serve
```

An explicit source import remains available even when that source is disabled:

```bash
shirabe import amp
shirabe import amp --path /path/to/amp/threads
```

Without `--path`, Amp import uses the Amp CLI first. With `--path`, it reads only
that local file or directory and does not invoke the CLI.

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

For Amp local fallback, `SHIRABE_AMP_THREADS_DIR` sets the threads directory.
`AMP_DATA_DIR` is also supported and may contain comma-separated Amp data roots;
Shirabe reads the `threads` directory below each root. A configured `sources.amp`
value takes precedence over these environment variables.

## Amp Privacy And Accuracy

Amp sync is CLI-first because its local thread cache can be incomplete. Shirabe
lists all personal threads, including archived threads, and transiently streams
each complete `amp threads export` result through a local process pipe. The
parser retains only usage metadata such as model, timestamp, token counts, and
stable event identity. It does not persist thread titles, message content,
thinking, tool payloads, environment details, or Amp user identity.

This integration depends on an unstable Amp CLI contract. Amp may change its
thread-list or export commands and schemas without notice, temporarily forcing
Shirabe onto its local fallback until compatibility is restored. CLI results
cover the personal account currently signed in to Amp; they are not enterprise,
organization-wide, or administrator analytics.

If the CLI cannot be discovered, authenticated, or parsed, Shirabe falls back
to configured local thread paths. This is best-effort usage reporting rather
than billing-grade auditing. The local cache can omit web and orb activity,
threads created on other devices, other remote-only activity, and any thread or
usage details that Amp has not materialized locally. Deleted or inaccessible
threads cannot be recovered by either path unless Shirabe imported their usage
before they became unavailable.

Shirabe uses Amp-reported token counts and applies the same LiteLLM price
estimates as other sources. Amp credits and provider-specific BYOK charges are
not imported.

## Pricing Refresh

The server checks pricing on startup, during each automatic five-minute sync,
and on each manual sync. It downloads the public LiteLLM pricing snapshot when
the cache is missing or more than 24 hours old. A pricing download failure does
not block local usage imports; the next sync retries it.
