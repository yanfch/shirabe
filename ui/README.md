# shirabe UI

Frontend app for the local shirabe web console.

Planned shape:

```text
ui/
  package.json
  src/
  dist/          # generated build output, served by Rust
```

The Rust server owns data collection, SQLite, imports, rollups, and API routes.
The UI calls the local API and is served from the same server.

Packaging plan:

```text
development
  Rust API server + frontend dev server

local web release
  Rust serves ui/dist from disk

future macOS menubar app
  SwiftUI/AppKit shell bundles:
    - shirabe Rust binary
    - ui/dist assets
  The shell starts/stops the server and opens the same UI in a window or browser.
```

Do not put product logic in the macOS shell. It is only a convenience wrapper.
