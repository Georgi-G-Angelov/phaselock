# PhaseLock – Synchronized Music Playback

## 1. Idea
A desktop application that allows multiple devices to connect in a "jam" session and play music in perfect sync — solving the latency and drift issues present in services like Spotify Jam.

---

## 2. Open Questions & Answers

**Q1: Is this for people in the same physical space (LAN), remote friends over the internet (WAN), or both?**
A1: Both. The app should support same-room / local network sync (e.g., house party with multiple speakers) as well as remote sync over the internet (friends listening together from different locations).

**Q2: Where does the music come from?**
A2: Local files only. One user (the host) uploads/selects an audio file, which is then transmitted to all other connected peers. All devices must begin playback at exactly the same time, with a target sync tolerance of ~1 ms.

**Q3: What audio formats and file sizes should be supported?**
A3: MVP supports MP3 only. Files are expected to be small (typical song-sized, ~3–10 MB). Since files are small, the full file will be transferred to all peers before playback begins — no streaming/chunked playback needed.

**Q4: What sync strategy should be used to achieve ~1 ms tolerance?**
A4: NTP-style clock synchronization. All peers measure round-trip time to the host and compute their clock offset relative to the host. The host then broadcasts a "play at timestamp T" command (using the host's clock as reference). Each peer converts T to its own local clock and schedules playback for that precise moment. This approach is the most reliable way to hit the ~1 ms sync target across both LAN and WAN.

**Q5: What network topology should be used for peer discovery and connection?**
A5: Host-as-server (star topology), no central server. The host creates a session and listens on a port. Peers connect directly to the host's IP and port. For LAN, the host can broadcast/advertise via mDNS for easy discovery. For WAN, peers will need to manually enter the host's IP/port (the host is responsible for ensuring reachability — e.g., port forwarding). No cloud infrastructure required.

**Q6: What technology stack should be used?**
A6: Tauri 2 + Rust backend with a web-based frontend (HTML/CSS/JS or a lightweight framework like Svelte/React). Rust gives low-level control over networking (TCP/UDP sockets), high-resolution timers, and audio decoding/playback — all critical for hitting the 1 ms sync target. Tauri keeps the app lightweight and cross-platform (Windows, macOS, Linux).

**Q7: Which frontend UI framework?**
A7: Svelte. Lightweight, minimal boilerplate, and fast — a natural fit for Tauri's small-footprint philosophy.

**Q8: What is the session/jam UX flow?**
A8: The flow is as follows, with LAN being the primary priority and WAN as extended functionality:

1. **Host** opens the app → clicks "Create Jam" → app starts listening on a port and auto-broadcasts the session via mDNS on the local network.
2. **Peer** opens the app → clicks "Join Jam" → sees a list of auto-discovered LAN sessions and can click to join. (WAN/extended: peer can manually enter host IP:port.)
3. Once connected, the host sees a **lobby** with all connected peers and their clock-sync status.
4. Host selects an MP3 file → file is transferred to all peers → once all peers confirm receipt, host can hit **Play**.
5. Only the host controls playback (play, pause, stop, seek). The host is the sole DJ for MVP.

**Q9: What playback controls exist, and how do peers interact?**
A9:
- **Host controls:** Play, Pause, Stop, Seek. The host has full and exclusive control over the currently playing track. All control commands are broadcast to peers who execute them in sync.
- **Volume:** Per-device only. Each user controls their own local volume. No global volume from the host.
- **Peer "now playing" view:** Peers see the current track info and a live progress bar, but all transport controls are disabled/read-only.
- **Queue requests:** Peers can submit an MP3 from their own machine as a "song request." The host sees the request in a queue/inbox and can accept or reject it. If accepted, the host receives the file from the peer and then distributes it to all other peers. The accepted track is added to the playback queue.
- **Local mute:** A peer can mute/unmute their own device without affecting the session.

**Q10: How does the playback queue work?**
A10:
- **Host adds songs:** The host can add MP3 files from their own machine directly to the queue at any time.
- **Auto-play:** When a song finishes, the next song in the queue plays automatically. The file must already be distributed to all peers before it starts (pre-distribute the next track while the current one is playing).
- **Host queue management:** The host can reorder and remove songs from the queue.
- **Peer visibility:** All peers can see the full queue (track names and order) in a read-only view, but cannot edit it.
- **Pause/resume:** The host can pause at any time, which pauses playback on all devices. Resume continues from the same position in sync.

**Q11: What networking protocol and sync strategy should be used?**
A11: Hybrid TCP + UDP approach:

- **File transfer (MP3 data):** TCP — reliable, every byte must arrive intact.
- **Clock sync (latency measurement):** UDP — the host and each peer continuously exchange lightweight ping/pong packets to measure round-trip time and compute one-way latency. Since these run frequently (e.g., every 100–500 ms), occasional packet loss is acceptable — stale measurements are simply discarded. Both the host and each peer maintain an up-to-date latency estimate at all times.
- **Playback control commands (play, pause, seek, stop):** TCP — reliable delivery guaranteed. The host uses the known per-peer latencies to **stagger command delivery**: it sends the command to the highest-latency peer first and the lowest-latency peer last, timed so that all peers receive the command at approximately the same moment. The host then schedules its own local playback to start at the same target moment, accounting for its zero-latency-to-self. This compensated dispatch is the key to synchronized playback across peers with varying latencies.

**Q12: Should there be active drift correction during playback?**
A12: Not for the MVP. We trust the initial clock sync and the accuracy of each machine's audio clock. For typical song lengths (3–5 min), drift on LAN should be well under 1 ms. Active drift correction (periodic checkpoints + micro-adjustments to playback speed) is a future enhancement for WAN and long sessions.

**Q13: What Rust audio library should be used for MP3 decoding and playback?**
A13: **cpal + symphonia**. `cpal` is the de facto cross-platform audio output layer in Rust (WASAPI on Windows, CoreAudio on macOS, ALSA/PulseAudio on Linux). `symphonia` is a pure-Rust, high-quality media decoder that supports MP3. Using these two together (rather than a higher-level wrapper like `rodio`) gives us direct control over the audio output callback, which is essential for scheduling playback to start at a precise sample boundary. We can pre-decode the MP3 into a PCM sample buffer, then begin feeding samples to `cpal`'s output stream at exactly the right moment.

**Q14: What should the UI look like?**
A14: Clean, modern UI using a Spotify-inspired color scheme:

**Color palette:**
- Background: #121212 (near-black)
- Surface/cards: #181818 / #282828
- Primary accent (green): #1DB954
- Text primary: #FFFFFF
- Text secondary: #B3B3B3
- Error/reject: #E22134
- Hover states: lighten surfaces slightly

**Host view screens:**
1. **Home screen:** "Create Jam" button (prominent, green accent).
2. **Session screen:**
   - Header: session status (local IP/port, peer count).
   - Left panel: connected peers list (name, sync latency indicator — green/yellow/red dot).
   - Center: now-playing area — track name, progress bar, Play / Pause / Stop / Skip / Seek controls.
   - Right panel: playback queue (drag-to-reorder, remove button per track, "Add Song" button at top).
   - Bottom drawer / notification area: incoming song requests from peers (accept ✓ / reject ✗ buttons, shows peer name + file name).

**Peer view screens:**
1. **Home screen:** "Join Jam" button → shows auto-discovered LAN sessions as a list (session name, host name, peer count). Manual IP:port entry field below for WAN.
2. **Session screen:**
   - Header: connection status, latency to host.
   - Center: now-playing area (read-only) — track name, progress bar (no transport controls).
   - Side panel: queue (read-only view).
   - "Request Song" button — opens file picker, sends MP3 to host for approval.
   - Local volume / mute control.

**Q15: How do peers identify themselves?**
A15: Simple display name. When joining a jam, the peer enters a text display name — no accounts, no persistence, no auth. The name is shown in the host's peer list and on song request notifications. If a name is already taken in the session, the app appends a number (e.g., "Alex (2)").

**Q16: What happens when a peer or host disconnects?**
A16:
- **Peer disconnects:** The host removes them from the peer list and playback continues uninterrupted for everyone else.
- **Peer reconnects:** They can rejoin the same session (enter name, connect to host). The host sends them the currently playing track (if they don't already have it), the current playback position, and the queue. The peer syncs up and begins playing from the current position as quickly as possible — no need to restart from the beginning.
- **Host disconnects/crashes:** The session ends for all peers. Peers see a "Host disconnected — session ended" message and return to the home screen. Host migration is a future enhancement.

**Q17: What is the maximum number of peers per session?**
A17: Small group — up to 5 peers (plus the host, so 6 devices total). This keeps bandwidth and CPU requirements on the host manageable (the host must upload the MP3 to each peer individually and maintain clock-sync with all of them). Larger group support is a future enhancement.

**Q18: Can the host name the session?**
A18: Yes. When creating a jam, the host enters a session name (e.g., "Friday Night Vibes"). This name is broadcast via mDNS and shown in the LAN discovery list for peers. The discovery list shows: session name, host display name, and current peer count (e.g., "Friday Night Vibes — hosted by Alex — 2/5 peers").

**Q19: Should sessions be password-protected?**
A19: No. Any device that can reach the host can join, up to the 5-peer limit. No authentication or access control for MVP. Password protection is a possible future enhancement for WAN use cases.

**Q20: Which platforms should the MVP target?**
A20: All three desktop platforms from day one: Windows, macOS, and Linux. Tauri and the chosen Rust crates (cpal, symphonia) all support cross-platform builds. CI/CD should produce installers/binaries for all three.

**Q21: How should error cases and edge cases be handled?**
A21:
- **Peer hasn't finished receiving the file:** Playback is blocked. The host cannot hit Play (or the button is disabled) until all connected peers have confirmed they have the full file and it's decoded and ready.
- **Peer has no audio device:** The peer is warned with a visible notification ("No audio output detected — you won't hear playback"). They can still stay in the session (to see the queue, request songs, etc.) but are flagged in the host's peer list.
- **File transfer fails mid-transfer:** Automatic retry (up to 3 attempts). If all retries fail, the host is notified which peer failed, and can choose to skip that peer or retry manually. The failed peer is not removed from the session.

**Q22: Logging, branding, distribution, and testing?**
A22:
- **Logging:** The app logs to a local file on each machine with a max size of 20 MB. Logs are buffered in an in-memory queue and flushed to disk every 50 entries. When the file exceeds 20 MB, the oldest entries are truncated from the start (ring-buffer style).
- **Branding:** The app is called **PhaseLock**.
- **Distribution:** The spec should include packaging — .msi for Windows, .dmg for macOS, .AppImage for Linux.
- **Testing:** No strong preference — spec should include a sensible testing plan.

---

## 3. Detailed Specification

*(To be developed.)*
