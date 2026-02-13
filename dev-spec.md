# PhaseLock — Developer Specification

> **Version:** 1.0 (MVP)
> **Date:** February 13, 2026
> **Status:** Ready for implementation

---

## Table of Contents

1. [Overview](#1-overview)
2. [Goals & Non-Goals](#2-goals--non-goals)
3. [Technology Stack](#3-technology-stack)
4. [Architecture](#4-architecture)
5. [Networking Protocol](#5-networking-protocol)
6. [Clock Synchronization](#6-clock-synchronization)
7. [File Transfer](#7-file-transfer)
8. [Audio Playback Engine](#8-audio-playback-engine)
9. [Session Lifecycle](#9-session-lifecycle)
10. [Playback Queue](#10-playback-queue)
11. [User Interface](#11-user-interface)
12. [Error Handling](#12-error-handling)
13. [Logging](#13-logging)
14. [Packaging & Distribution](#14-packaging--distribution)
15. [Project Structure](#15-project-structure)
16. [Message Protocol Reference](#16-message-protocol-reference)
17. [Testing Plan](#17-testing-plan)
18. [Future Enhancements](#18-future-enhancements)

---

## 1. Overview

**PhaseLock** is a cross-platform desktop application that allows up to 6 devices (1 host + 5 peers) to connect in a "jam" session and play MP3 music in perfect sync, with a target synchronization tolerance of **~1 ms**.

It solves the latency and drift issues present in services like Spotify Jam by using NTP-style clock synchronization, latency-compensated command dispatch, and direct peer-to-host networking with no central server.

**Primary use case:** LAN (house party, shared space with multiple speakers).
**Secondary use case:** WAN (remote friends listening together over the internet).

---

## 2. Goals & Non-Goals

### MVP Goals

- Host creates a named jam session; peers discover and join via LAN (mDNS) or manual IP:port (WAN).
- Host selects MP3 files from local disk; files are transferred to all peers before playback.
- All devices play audio in sync with ≤1 ms tolerance on LAN.
- Host has exclusive playback control (play, pause, stop, seek, skip).
- Peers can request songs; host approves/rejects.
- Playback queue with auto-advance, reorder, and remove.
- Cross-platform: Windows, macOS, Linux.

### Non-Goals (MVP)

- Streaming service integration (Spotify, Apple Music, etc.).
- Audio formats other than MP3.
- Active drift correction during playback.
- Password-protected sessions.
- Host migration on disconnect.
- Peer counts larger than 5.
- Mobile platforms (iOS, Android).
- User accounts or persistent profiles.

---

## 3. Technology Stack

| Layer             | Technology                     | Rationale                                                                 |
| ----------------- | ------------------------------ | ------------------------------------------------------------------------- |
| App framework     | **Tauri 2**                    | Lightweight, cross-platform desktop app with Rust backend + web frontend. |
| Backend language  | **Rust**                       | Low-level control over networking, audio, and high-resolution timers.     |
| Frontend framework| **Svelte**                     | Minimal boilerplate, fast, small bundle — ideal for Tauri.                |
| Audio output      | **cpal**                       | Cross-platform audio I/O (WASAPI / CoreAudio / ALSA+PulseAudio).         |
| MP3 decoding      | **symphonia**                  | Pure-Rust media decoder, high quality, MP3 support.                       |
| LAN discovery     | **mdns-sd** (or similar crate) | mDNS/DNS-SD for zero-config LAN session discovery.                       |
| Networking        | **std::net** (TCP + UDP)       | Rust standard library sockets; no heavy framework needed.                 |
| Serialization     | **serde + bincode** (or MessagePack) | Fast, compact binary serialization for network messages.            |
| Build / bundle    | **Tauri CLI + GitHub Actions** | CI/CD for cross-platform builds and installer generation.                 |

### Key Rust Crates

```toml
[dependencies]
tauri = "2"
cpal = "0.15"
symphonia = { version = "0.5", features = ["mp3"] }
serde = { version = "1", features = ["derive"] }
bincode = "1"
mdns-sd = "0.11"
tokio = { version = "1", features = ["full"] }
chrono = "0.4"
log = "0.4"
parking_lot = "0.12"
```

> Exact versions should be pinned at project setup time.

---

## 4. Architecture

### 4.1 High-Level Architecture

```
┌─────────────────────────────────────┐
│           PhaseLock App             │
│  ┌───────────┐   ┌───────────────┐ │
│  │  Svelte   │◄─►│  Tauri IPC    │ │
│  │  Frontend │   │  (Commands +  │ │
│  │           │   │   Events)     │ │
│  └───────────┘   └──────┬────────┘ │
│                         │          │
│  ┌──────────────────────▼────────┐ │
│  │         Rust Backend          │ │
│  │  ┌──────────┐ ┌────────────┐  │ │
│  │  │ Session  │ │   Audio    │  │ │
│  │  │ Manager  │ │   Engine   │  │ │
│  │  └────┬─────┘ └─────┬──────┘  │ │
│  │       │              │         │ │
│  │  ┌────▼─────┐ ┌──────▼──────┐ │ │
│  │  │ Network  │ │  Playback   │ │ │
│  │  │ Module   │ │  Scheduler  │ │ │
│  │  └────┬─────┘ └─────────────┘ │ │
│  │       │                        │ │
│  │  ┌────▼─────┐                  │ │
│  │  │  Clock   │                  │ │
│  │  │  Sync    │                  │ │
│  │  └──────────┘                  │ │
│  └────────────────────────────────┘ │
└─────────────────────────────────────┘
```

### 4.2 Module Breakdown

| Module               | Responsibility                                                                                      |
| -------------------- | --------------------------------------------------------------------------------------------------- |
| **Session Manager**  | Create/join sessions, manage peer list, handle connect/disconnect, mDNS broadcast/discovery.        |
| **Network Module**   | TCP connections (file transfer + commands), UDP sockets (clock sync), message serialization.         |
| **Clock Sync**       | NTP-style ping/pong over UDP, RTT measurement, offset calculation, maintains per-peer latency map.  |
| **Audio Engine**     | MP3 decoding via symphonia, PCM buffer management, cpal output stream management.                   |
| **Playback Scheduler** | Receives "play at time T" commands, schedules sample feeding to cpal at the precise moment.       |
| **Queue Manager**    | Manages the ordered song queue, add/remove/reorder, pre-distribution of next track.                 |
| **File Transfer**    | Chunked TCP transfer of MP3 files, integrity verification (SHA-256 hash), retry logic.              |
| **Logger**           | Buffered file logging with 20 MB rotation.                                                          |

### 4.3 Roles

Every instance of PhaseLock runs the same binary. The role (host vs. peer) is determined at runtime:

- **Host:** Runs a TCP listener + UDP socket. Manages the session, queue, and dispatches commands.
- **Peer:** Connects to the host via TCP + UDP. Receives files and commands, reports readiness.

---

## 5. Networking Protocol

### 5.1 Transports

| Purpose                  | Transport | Port            | Notes                                                   |
| ------------------------ | --------- | --------------- | ------------------------------------------------------- |
| Session control & commands | TCP     | Configurable (default: `17401`) | Persistent connection per peer. Reliable delivery.  |
| File transfer            | TCP       | Same connection | Multiplexed on the same TCP stream, or a secondary TCP connection per transfer. |
| Clock synchronization    | UDP       | Configurable (default: `17402`) | Lightweight ping/pong. Stateless. Tolerates loss.   |
| mDNS discovery           | UDP       | `5353` (standard mDNS) | Broadcast/listen for `_phaselock._tcp.local.`        |

### 5.2 Connection Flow

```
Peer                                    Host
 │                                        │
 │  ── TCP connect ──────────────────────►│
 │                                        │
 │  ◄── SessionInfo (name, peer list) ───│
 │                                        │
 │  ── JoinRequest (display_name) ──────►│
 │                                        │
 │  ◄── JoinAccepted (peer_id) ─────────│
 │                                        │
 │  ◄─► UDP clock sync begins ──────────►│
 │       (continuous, every 200ms)        │
 │                                        │
 │  ◄── FileTransferStart (metadata) ───│
 │  ◄── FileChunk[] ─────────────────────│
 │  ── FileReceived (hash_ok) ──────────►│
 │                                        │
 │  ◄── PlayCommand (target_time, pos) ─│
 │                                        │
 │       ... playback in sync ...         │
```

### 5.3 Message Format

All messages are serialized with **bincode** (compact binary) and prefixed with a 4-byte little-endian length header:

```
[length: u32][message_type: u8][payload: bincode bytes]
```

---

## 6. Clock Synchronization

### 6.1 Algorithm

NTP-style offset calculation using UDP ping/pong:

1. Peer sends `ClockPing { peer_send_time: u64 }` (nanosecond monotonic clock) to host.
2. Host receives it, records `host_recv_time`, and immediately replies with `ClockPong { peer_send_time, host_recv_time, host_send_time }`.
3. Peer receives the pong at `peer_recv_time` and computes:

$$RTT = (peer\_recv\_time - peer\_send\_time) - (host\_send\_time - host\_recv\_time)$$

$$offset = \frac{(host\_recv\_time - peer\_send\_time) + (host\_send\_time - peer\_recv\_time)}{2}$$

4. The peer maintains a **sliding window** (last 10 measurements) and uses the **median offset** to filter out outliers.

### 6.2 Parameters

| Parameter           | Value    | Notes                                        |
| ------------------- | -------- | -------------------------------------------- |
| Ping interval       | 200 ms   | 5 pings/second per peer.                     |
| Sliding window size | 10       | Last 10 RTT/offset measurements.             |
| Aggregation         | Median   | Robust against occasional outlier packets.    |
| Stale threshold     | 2 s      | Discard measurements older than 2 seconds.    |

### 6.3 Host-Side Latency Map

The host also computes and stores each peer's **estimated one-way latency** ($RTT / 2$) from the same ping/pong exchange (the host timestamps are already in the pong). This map is used for staggered command dispatch.

---

## 7. File Transfer

### 7.1 Flow

1. Host (or peer via song request) initiates a transfer.
2. Host sends `FileTransferStart { file_id, file_name, file_size, sha256_hash }` to each peer.
3. Host sends file data in chunks of **64 KB** over TCP.
4. Peer reassembles, computes SHA-256 hash, and sends `FileReceived { file_id, hash_ok: bool }`.
5. If hash mismatch → automatic retry (up to 3 attempts).
6. Once **all peers** confirm receipt → file is marked as "ready" in the queue.

### 7.2 Song Request Flow (Peer → Host)

1. Peer sends `SongRequest { file_name, file_size }` to host.
2. Host UI shows the request (peer name + file name). Host accepts or rejects.
3. If accepted: host sends `SongRequestAccepted { request_id }` → peer streams the MP3 to host over TCP.
4. Host receives the file, then distributes it to all other peers (standard file transfer flow).
5. Track is appended to the queue.

### 7.3 Pre-Distribution

While a song is playing, the host should **pre-distribute the next track** in the queue to all peers so there is no gap between songs. The next track should begin transferring as soon as the current track starts playing (or when it's added to the queue if the queue was previously empty).

---

## 8. Audio Playback Engine

### 8.1 Pipeline

```
MP3 file → symphonia decoder → PCM sample buffer (Vec<f32>) → cpal output stream
```

1. **Decode on receipt:** When a peer receives an MP3 file and confirms hash integrity, it immediately decodes the full file into an interleaved `Vec<f32>` PCM buffer (f32 samples, native sample rate from the MP3, stereo).
2. **cpal output stream:** A persistent cpal output stream runs at the system's preferred sample rate. If the MP3 sample rate differs from the output device rate, resample using a simple linear interpolator (or the `rubato` crate for higher quality).
3. **Playback scheduling:** The audio callback reads from a shared `PlaybackState` struct:

```rust
struct PlaybackState {
    buffer: Arc<Vec<f32>>,       // decoded PCM samples
    sample_rate: u32,
    channels: u16,
    play_at: Option<Instant>,    // when to start feeding samples
    position: AtomicUsize,       // current sample index
    state: AtomicU8,             // Stopped / Waiting / Playing / Paused
}
```

4. **Precision start:** The cpal audio callback checks `play_at` on every buffer fill. Once `Instant::now() >= play_at`, it begins copying samples from `position` into the output buffer. Before that moment, it writes silence. This gives sample-accurate start time (precision = audio buffer size, typically 256–512 samples = 5–10 ms at 48 kHz).

### 8.2 Minimizing Audio Latency

- Request the **smallest buffer size** that cpal/the OS will allow (e.g., 128 or 256 samples).
- Use a **dedicated high-priority audio thread** (cpal handles this internally).
- Pre-decode the full file — no decoding happens in the audio callback.

### 8.3 Seek

On seek, the host broadcasts a `SeekCommand { position_samples, target_time }`. All peers:
1. Update `position` to the new sample offset.
2. Schedule resume at `target_time` (same staggered dispatch as play).

---

## 9. Session Lifecycle

### 9.1 Creating a Session (Host)

1. User enters **session name** and **display name**.
2. App binds TCP listener on port `17401` and UDP socket on port `17402`.
3. App registers mDNS service: `_phaselock._tcp.local.` with TXT records: `name=<session_name>`, `host=<display_name>`, `peers=0`, `max_peers=5`.
4. Session screen is shown. Host is ready to accept connections.

### 9.2 Joining a Session (Peer)

1. User enters **display name**.
2. App browses for `_phaselock._tcp.local.` services on the network.
3. Discovered sessions are listed: `<session_name> — hosted by <host_name> — <n>/5 peers`.
4. User clicks a session (or enters IP:port manually for WAN).
5. TCP connection established → `JoinRequest` sent → `JoinAccepted` received with `peer_id`.
6. UDP clock sync begins immediately.
7. Peer receives current queue state and any already-distributed files.

### 9.3 Session End

- **Host closes session:** Sends `SessionEnd` to all peers. Peers show "Session ended" and return to home screen.
- **Host crashes:** Peers detect TCP disconnect (or heartbeat timeout of 5 seconds). Peers show "Host disconnected" and return to home screen.
- **Peer leaves voluntarily:** Sends `LeaveSession`. Host removes peer from list. Playback continues.
- **Peer crashes:** Host detects TCP disconnect. Removes peer from list. Playback continues.

### 9.4 Peer Reconnection

1. Peer rejoins by connecting to the same host (standard join flow).
2. Host sends the current `SessionState`:
   - Current track (file + playback position).
   - Queue (with file IDs — peer can check which files it already has cached locally).
3. Host transfers any files the peer is missing.
4. Once the peer has the current track decoded and ready, it syncs to the current playback position and begins playing.

---

## 10. Playback Queue

### 10.1 Data Model

```rust
struct QueueItem {
    id: Uuid,
    file_name: String,
    duration_secs: f64,
    added_by: String,           // display name of who added it
    status: QueueItemStatus,    // Transferring | Ready | Playing | Played
}
```

### 10.2 Behavior

| Action                | Who       | Effect                                                          |
| --------------------- | --------- | --------------------------------------------------------------- |
| Add song              | Host      | File picker → file added to queue → transfer to all peers.      |
| Request song          | Peer      | Request sent to host → host accepts/rejects → on accept, added. |
| Reorder queue         | Host only | Drag-and-drop reorder. Updated queue broadcast to all peers.    |
| Remove from queue     | Host only | Track removed. Updated queue broadcast to all peers.            |
| Skip (next track)     | Host only | Current track stops, next track begins (if ready).              |
| Auto-advance          | Automatic | When current track ends, next "Ready" track plays immediately.  |
| View queue            | All       | All peers see the queue in read-only mode.                      |

### 10.3 Queue Sync

Any change to the queue (add, remove, reorder, status change) triggers a `QueueUpdate` message broadcast to all peers so all clients stay in sync.

---

## 11. User Interface

### 11.1 Color Palette (Spotify-Inspired Dark Theme)

| Token            | Hex       | Usage                        |
| ---------------- | --------- | ---------------------------- |
| Background       | `#121212` | Main app background          |
| Surface          | `#181818` | Cards, panels                |
| Surface elevated | `#282828` | Hover states, active items   |
| Primary (green)  | `#1DB954` | Buttons, accents, active     |
| Text primary     | `#FFFFFF` | Headings, primary text       |
| Text secondary   | `#B3B3B3` | Subtitles, metadata          |
| Error / reject   | `#E22134` | Error states, reject buttons |

### 11.2 Screens

#### 11.2.1 Home Screen (Both Roles)

```
┌─────────────────────────────────────────┐
│              🎵 PhaseLock               │
│                                         │
│    ┌─────────────────────────────┐      │
│    │      [ Create Jam ]         │      │
│    └─────────────────────────────┘      │
│                                         │
│    ┌─────────────────────────────┐      │
│    │      [ Join Jam ]           │      │
│    └─────────────────────────────┘      │
│                                         │
└─────────────────────────────────────────┘
```

- **Create Jam:** Prompts for session name + display name → opens host session screen.
- **Join Jam:** Prompts for display name → opens session browser.

#### 11.2.2 Session Browser (Peer)

```
┌─────────────────────────────────────────┐
│  Join a Jam                             │
│                                         │
│  Discovered sessions:                   │
│  ┌─────────────────────────────────┐    │
│  │ 🟢 Friday Night Vibes          │    │
│  │    hosted by Alex — 2/5 peers   │    │
│  ├─────────────────────────────────┤    │
│  │ 🟢 Chill Lounge                │    │
│  │    hosted by Sam — 0/5 peers    │    │
│  └─────────────────────────────────┘    │
│                                         │
│  ── or connect manually ──              │
│  [ IP:Port ___________] [ Connect ]     │
│                                         │
└─────────────────────────────────────────┘
```

#### 11.2.3 Host Session Screen

```
┌─────────────────────────────────────────────────────────────┐
│  Friday Night Vibes                    192.168.1.42:17401   │
│  3 peers connected                                          │
├──────────────┬──────────────────────────┬───────────────────┤
│  Peers       │   Now Playing            │  Queue            │
│              │                          │                   │
│  🟢 Alex    │   Song Title.mp3         │  1. Song A ✕      │
│     2ms      │   ───●──────────── 2:34  │  2. Song B ✕      │
│  🟡 Sam     │                          │  3. Song C ✕      │
│     45ms     │   [◄◄] [⏸] [⏹] [►►]     │                   │
│  🟢 Jordan  │                          │  [+ Add Song]     │
│     3ms      │                          │                   │
│              │                          │                   │
├──────────────┴──────────────────────────┴───────────────────┤
│  Song Requests                                              │
│  Sam wants to add "NewTrack.mp3" (4.2 MB)    [ ✓ ] [ ✗ ]   │
└─────────────────────────────────────────────────────────────┘
```

**Peer latency indicators:**
- 🟢 Green: < 10 ms
- 🟡 Yellow: 10–100 ms
- 🔴 Red: > 100 ms

#### 11.2.4 Peer Session Screen

```
┌─────────────────────────────────────────────────────────────┐
│  Friday Night Vibes                 Connected — latency: 3ms│
├─────────────────────────────────────┬───────────────────────┤
│   Now Playing                       │  Queue                │
│                                     │                       │
│   Song Title.mp3                    │  1. Song A            │
│   ───●──────────── 2:34             │  2. Song B            │
│                                     │  3. Song C            │
│                                     │                       │
│                                     │                       │
│   🔊 ────●────────                  │  [🎵 Request Song]   │
│                                     │                       │
└─────────────────────────────────────┴───────────────────────┘
```

---

## 12. Error Handling

### 12.1 Error Matrix

| Scenario                           | Behavior                                                                                             |
| ---------------------------------- | ---------------------------------------------------------------------------------------------------- |
| Peer hasn't received file yet      | Play button disabled on host. Tooltip: "Waiting for all peers to receive the file."                  |
| File transfer fails                | Auto-retry up to 3 times. After 3 failures: host notified, can skip peer or retry manually.          |
| Peer has no audio output device    | Peer warned: "No audio output detected." Peer stays in session, flagged in host's peer list.         |
| Peer disconnects mid-playback      | Host removes peer from list. Playback continues for all others.                                      |
| Host disconnects/crashes           | All peers see "Host disconnected — session ended." Return to home screen.                            |
| Invalid MP3 file selected          | Show error toast: "Invalid or corrupted MP3 file." File not added to queue.                          |
| MP3 decode failure                 | Peer notifies host. Host notified. Track can be skipped.                                             |
| Hash mismatch after transfer       | Automatic retry (counted toward the 3-retry limit).                                                  |
| Session full (5 peers)             | New peer connection rejected with `SessionFull` message. Peer sees: "Session is full."               |
| UDP clock sync not converging      | Peer flagged as 🔴 in host peer list. Warn host before playing.                                     |
| Port already in use                | Show error: "Port 17401 is in use. Please close the other application or change the port."           |

---

## 13. Logging

### 13.1 Configuration

| Parameter         | Value                                           |
| ----------------- | ----------------------------------------------- |
| Log file path     | `<app_data_dir>/phaselock/phaselock.log`        |
| Max file size     | 20 MB                                           |
| Buffer strategy   | In-memory queue, flush to disk every 50 entries |
| Rotation strategy | Truncate from start of file when exceeding 20 MB (ring-buffer style) |
| Log levels        | `ERROR`, `WARN`, `INFO`, `DEBUG`, `TRACE`       |
| Default level     | `INFO` (configurable via environment variable `PHASELOCK_LOG`) |

### 13.2 What to Log

| Category       | Level   | Examples                                                              |
| -------------- | ------- | --------------------------------------------------------------------- |
| Session events | INFO    | Session created, peer joined, peer left, session ended.               |
| File transfer  | INFO    | Transfer started, completed, failed, retry.                           |
| Clock sync     | DEBUG   | Per-ping RTT, computed offset, median offset update.                  |
| Playback       | INFO    | Play/pause/stop/seek commands dispatched, target timestamps.          |
| Errors         | ERROR   | Decode failure, hash mismatch, connection errors, bind failures.      |
| Network        | DEBUG   | TCP connect/disconnect, bytes sent/received, UDP packet stats.        |

### 13.3 Implementation

Use the `log` crate facade with a custom sink that:
1. Writes log entries to a `VecDeque<String>` (in-memory buffer).
2. Every 50 entries (or on `ERROR`), flushes the buffer to the log file.
3. Before flushing, checks file size. If > 20 MB, truncates the first ~25% of the file to make room.

---

## 14. Packaging & Distribution

### 14.1 Installers

| Platform | Format      | Tool                  |
| -------- | ----------- | --------------------- |
| Windows  | `.msi`      | Tauri bundler (WiX)   |
| macOS    | `.dmg`      | Tauri bundler          |
| Linux    | `.AppImage` | Tauri bundler          |

### 14.2 CI/CD

Use **GitHub Actions** with a matrix build:

```yaml
strategy:
  matrix:
    os: [windows-latest, macos-latest, ubuntu-latest]
```

Each push to `main` / tagged release triggers:
1. Rust + Node.js setup.
2. `npm install` (Svelte dependencies).
3. `cargo tauri build` → produces platform-specific installer.
4. Upload artifacts / create GitHub Release.

### 14.3 App Metadata

| Field        | Value                                      |
| ------------ | ------------------------------------------ |
| App name     | PhaseLock                                  |
| Bundle ID    | `com.phaselock.app`                        |
| Version      | `0.1.0` (MVP)                              |
| Description  | Synchronized music playback for groups.    |

---

## 15. Project Structure

```
phaselock/
├── src-tauri/
│   ├── Cargo.toml
│   ├── tauri.conf.json
│   ├── src/
│   │   ├── main.rs                  # Tauri entry point
│   │   ├── lib.rs                   # Module declarations
│   │   ├── session/
│   │   │   ├── mod.rs               # Session manager (create, join, leave)
│   │   │   ├── host.rs              # Host-specific session logic
│   │   │   └── peer.rs              # Peer-specific session logic
│   │   ├── network/
│   │   │   ├── mod.rs               # Network module entry
│   │   │   ├── tcp.rs               # TCP listener, connections, message framing
│   │   │   ├── udp.rs               # UDP socket for clock sync
│   │   │   ├── mdns.rs              # mDNS broadcast & discovery
│   │   │   └── messages.rs          # All message types (serde structs)
│   │   ├── sync/
│   │   │   ├── mod.rs
│   │   │   └── clock.rs             # NTP-style clock offset calculation
│   │   ├── audio/
│   │   │   ├── mod.rs
│   │   │   ├── decoder.rs           # symphonia MP3 → PCM decoding
│   │   │   ├── playback.rs          # cpal output stream + playback state
│   │   │   └── scheduler.rs         # "Play at time T" scheduling logic
│   │   ├── queue/
│   │   │   ├── mod.rs
│   │   │   └── manager.rs           # Queue data model, add/remove/reorder
│   │   ├── transfer/
│   │   │   ├── mod.rs
│   │   │   └── file_transfer.rs     # Chunked file send/receive, hash verify
│   │   ├── logging/
│   │   │   ├── mod.rs
│   │   │   └── logger.rs            # Buffered file logger with rotation
│   │   └── commands.rs              # Tauri IPC command handlers
│   └── icons/                       # App icons
├── src/                             # Svelte frontend
│   ├── App.svelte                   # Root component, router
│   ├── main.ts                      # Svelte entry point
│   ├── lib/
│   │   ├── stores/                  # Svelte stores (session, queue, playback)
│   │   ├── components/
│   │   │   ├── HomeScreen.svelte
│   │   │   ├── CreateJam.svelte
│   │   │   ├── SessionBrowser.svelte
│   │   │   ├── HostSession.svelte
│   │   │   ├── PeerSession.svelte
│   │   │   ├── NowPlaying.svelte
│   │   │   ├── Queue.svelte
│   │   │   ├── PeerList.svelte
│   │   │   └── SongRequests.svelte
│   │   └── types.ts                 # TypeScript type definitions
│   └── styles/
│       └── global.css               # Spotify-inspired dark theme
├── package.json
├── svelte.config.js
├── vite.config.ts
├── tsconfig.json
└── README.md
```

---

## 16. Message Protocol Reference

### 16.1 TCP Messages (Control Channel)

| Message                | Direction   | Payload                                                       |
| ---------------------- | ----------- | ------------------------------------------------------------- |
| `JoinRequest`          | Peer → Host | `{ display_name: String }`                                    |
| `JoinAccepted`         | Host → Peer | `{ peer_id: u32, session_state: SessionState }`               |
| `JoinRejected`         | Host → Peer | `{ reason: String }` (e.g., "Session full")                   |
| `LeaveSession`         | Peer → Host | `{ peer_id: u32 }`                                            |
| `SessionEnd`           | Host → All  | `{}`                                                          |
| `Heartbeat`            | Bidirectional | `{}` — sent every 2 seconds, timeout at 5 seconds.          |
| `FileTransferStart`    | Host → Peer | `{ file_id: Uuid, file_name: String, size: u64, sha256: [u8; 32] }` |
| `FileChunk`            | Host → Peer | `{ file_id: Uuid, offset: u64, data: Vec<u8> }`              |
| `FileReceived`         | Peer → Host | `{ file_id: Uuid, hash_ok: bool }`                            |
| `SongRequest`          | Peer → Host | `{ file_name: String, file_size: u64 }`                       |
| `SongRequestAccepted`  | Host → Peer | `{ request_id: Uuid }`                                        |
| `SongRequestRejected`  | Host → Peer | `{ request_id: Uuid }`                                        |
| `SongUploadChunk`      | Peer → Host | `{ request_id: Uuid, offset: u64, data: Vec<u8> }`           |
| `SongUploadComplete`   | Peer → Host | `{ request_id: Uuid, sha256: [u8; 32] }`                     |
| `PlayCommand`          | Host → Peer | `{ file_id: Uuid, position_samples: u64, target_time_ns: u64 }` |
| `PauseCommand`         | Host → Peer | `{ position_samples: u64 }`                                   |
| `ResumeCommand`        | Host → Peer | `{ position_samples: u64, target_time_ns: u64 }`             |
| `StopCommand`          | Host → Peer | `{}`                                                          |
| `SeekCommand`          | Host → Peer | `{ position_samples: u64, target_time_ns: u64 }`             |
| `QueueUpdate`          | Host → All  | `{ queue: Vec<QueueItem> }`                                   |

### 16.2 UDP Messages (Clock Sync)

| Message      | Direction   | Payload                                                                  |
| ------------ | ----------- | ------------------------------------------------------------------------ |
| `ClockPing`  | Peer → Host | `{ peer_id: u32, peer_send_time_ns: u64 }`                              |
| `ClockPong`  | Host → Peer | `{ peer_id: u32, peer_send_time_ns: u64, host_recv_time_ns: u64, host_send_time_ns: u64 }` |

---

## 17. Testing Plan

### 17.1 Unit Tests

| Module           | Tests                                                                       |
| ---------------- | --------------------------------------------------------------------------- |
| Clock Sync       | Offset calculation with known RTT values. Median filter with outliers. Edge cases (zero RTT, huge RTT). |
| Message Serde    | Round-trip serialize/deserialize for every message type. Verify length prefixing. |
| Queue Manager    | Add, remove, reorder, auto-advance. Edge cases (empty queue, single item). |
| Audio Decoder    | Decode a known MP3 → verify PCM sample count, sample rate, channels.       |
| File Transfer    | SHA-256 hash computation and verification. Chunk reassembly.               |
| Logger           | Buffer flush at 50 entries. File size truncation at 20 MB boundary.        |

### 17.2 Integration Tests

| Test                          | Description                                                                     |
| ----------------------------- | ------------------------------------------------------------------------------- |
| Host ↔ Peer connection        | Start a host, connect a peer over loopback TCP. Verify handshake completes.     |
| Clock sync convergence        | Run clock sync between host and peer on loopback. Verify offset converges to < 1 ms within 2 seconds. |
| File transfer end-to-end      | Transfer a known MP3 from host to peer. Verify hash match and decoded PCM equality. |
| Play command sync              | Host sends PlayCommand to 2 peers with different simulated latencies. Verify both schedule playback at the same wall-clock time (within tolerance). |
| Queue operations              | Host adds/removes/reorders songs. Verify all peers receive correct `QueueUpdate`. |
| Peer disconnect / reconnect   | Peer disconnects mid-session, reconnects, receives current state and resumes.   |
| Session full rejection        | Connect 5 peers, attempt 6th. Verify `JoinRejected` with "Session full."       |

### 17.3 Manual / Exploratory Tests

| Test                          | Description                                                                     |
| ----------------------------- | ------------------------------------------------------------------------------- |
| LAN discovery                 | Two machines on the same network. Verify mDNS discovery shows the session.      |
| Audible sync test             | Place two devices side by side. Play a click track. Listen for echo/flamming (indicates > ~2 ms drift). |
| WAN test                      | Connect two machines over the internet (with port forwarding). Verify playback starts and stays in sync. |
| Stress: large file            | Load a 10 MB MP3. Verify transfer completes and playback works.                |
| Stress: 5 peers               | Connect 5 peers on LAN. Verify all receive files and play in sync.              |
| Error: bad MP3                | Select a non-MP3 or corrupted file. Verify graceful error message.              |
| Error: pull network cable     | Disconnect a peer mid-playback. Verify host continues and peer shows disconnect message. |

### 17.4 Performance Benchmarks

| Metric                     | Target                          |
| -------------------------- | ------------------------------- |
| Clock sync accuracy (LAN)  | < 1 ms offset after convergence |
| Clock sync accuracy (WAN)  | < 10 ms offset                  |
| File transfer speed (LAN)  | > 5 MB/s per peer               |
| Playback start sync (LAN)  | < 1 ms difference between peers |
| Memory usage (idle)         | < 50 MB                         |
| Memory usage (playing)      | < 150 MB (with decoded PCM)     |
| CPU usage (playing)         | < 5% on modern hardware         |

---

## 18. Future Enhancements

These are explicitly **out of scope** for the MVP but documented for future planning:

| Enhancement                 | Description                                                                 |
| --------------------------- | --------------------------------------------------------------------------- |
| Active drift correction     | Periodic checkpoint sync + micro-speed adjustments during playback.         |
| More audio formats          | FLAC, WAV, OGG, AAC support via additional symphonia features.              |
| Streaming service integration | Coordinate playback of Spotify/Apple Music tracks (command sync only).    |
| Password-protected sessions | Optional password when creating a jam.                                      |
| Host migration              | If the host drops, promote a peer to host automatically.                    |
| Larger groups               | Support 20+ peers with optimized distribution (tree/mesh file relay).       |
| Mobile apps                 | iOS/Android companion apps.                                                 |
| Chat                        | Text chat between peers during a session.                                   |
| Persistent profiles         | Save display name and preferences locally.                                  |
| Visualizer                  | Audio visualizer displayed on the now-playing screen.                       |
| Playlist import             | Import .m3u or folder of MP3s as a batch queue.                             |
