# PhaseLock

[![CI](https://github.com/user/phaselock/actions/workflows/ci.yml/badge.svg)](https://github.com/user/phaselock/actions/workflows/ci.yml)
[![Build & Release](https://github.com/user/phaselock/actions/workflows/release.yml/badge.svg)](https://github.com/user/phaselock/actions/workflows/release.yml)

Synchronized music playback for groups. Like Spotify Jam, but with perfect sync.

## Download Pre-built Releases

Head to the [Releases](https://github.com/user/phaselock/releases) page and grab the installer for your platform:

| Platform | Download |
|----------|----------|
| Windows  | `.exe` (NSIS) or `.msi` (WiX) |
| macOS    | `.dmg` (Apple Silicon & Intel) |
| Linux    | `.AppImage` or `.deb` |

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

## Architecture

PhaseLock uses a **host/peer** networking model:

1. **Host** creates a session and advertises it via mDNS on the local network.
2. **Peers** discover sessions and connect over TCP (control messages) and UDP (sync & audio).
3. **NTP-style clock synchronisation** keeps all peers within ~1 ms of the host clock.
4. Hosts manage a shared **queue** and stream audio files to peers via **chunked file transfer**.
5. Playback uses **symphonia** for MP3 decoding and **cpal** for low-latency audio output.

## Contributing

1. Fork the repository.
2. Create a feature branch (`git checkout -b feat/my-feature`).
3. Run `cd src-tauri && cargo fmt && cargo clippy -- -D warnings && cargo test` before committing.
4. Open a Pull Request — CI must be green.

## License

MIT
