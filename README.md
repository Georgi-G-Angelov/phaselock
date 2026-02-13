# PhaseLock

Synchronized music playback for groups. Like Spotify Jam, but with perfect sync.

## Prerequisites

- [Node.js](https://nodejs.org/) (v18+)
- [Rust](https://www.rust-lang.org/tools/install) (stable)
- [Tauri 2 prerequisites](https://v2.tauri.app/start/prerequisites/) for your platform

## Setup

```bash
npm install
```

## Development

Start the app in dev mode with hot-reloading:

```bash
npm run tauri dev
```

This launches both the Vite dev server (frontend on port 5173) and the Rust backend. The PhaseLock window opens automatically. Code changes to Svelte files reload instantly; Rust changes trigger an incremental recompile.

## Release Build

Build platform-specific installers:

```bash
npm run tauri build
```

Output is placed in `src-tauri/target/release/bundle/`:

| Platform | Format | Path |
|----------|--------|------|
| Windows  | NSIS installer | `bundle/nsis/PhaseLock_0.1.0_x64-setup.exe` |
| Windows  | MSI installer | `bundle/msi/PhaseLock_0.1.0_x64_en-US.msi` |
| macOS    | DMG | `bundle/dmg/PhaseLock_0.1.0_aarch64.dmg` |
| macOS    | App bundle | `bundle/macos/PhaseLock.app` |
| Linux    | AppImage | `bundle/appimage/PhaseLock_0.1.0_amd64.AppImage` |
| Linux    | Deb package | `bundle/deb/PhaseLock_0.1.0_amd64.deb` |

You can also run the compiled binary directly without installing:

```bash
./src-tauri/target/release/PhaseLock.exe   # Windows
./src-tauri/target/release/phase-lock      # macOS / Linux
```

## Project Structure

```
phaselock/
├── src/                  # Svelte 5 frontend
│   ├── main.ts
│   └── App.svelte
├── src-tauri/            # Rust backend (Tauri 2)
│   └── src/
│       ├── main.rs       # Entry point
│       ├── lib.rs        # Module declarations & Tauri builder
│       ├── commands.rs   # IPC command handlers
│       ├── session/      # Host & peer session management
│       ├── network/      # TCP, UDP, mDNS networking
│       ├── sync/         # NTP-style clock synchronization
│       ├── audio/        # MP3 decoding & playback (symphonia + cpal)
│       ├── queue/        # Playlist queue management
│       ├── transfer/     # Chunked file transfer
│       └── logging/      # Buffered file logging
├── package.json
├── vite.config.ts
└── svelte.config.js
```
