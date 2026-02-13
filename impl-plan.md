# PhaseLock — Implementation Plan

> Derived from [dev-spec.md](dev-spec.md)
> Each step has a corresponding prompt file in the `prompts/` folder.

---

## Overview

The implementation is broken into **20 steps** across **5 phases**. Each step is designed to be a self-contained chunk of work that builds on the previous steps. Steps within the same phase can sometimes be parallelized, but the phases themselves are sequential.

---

## Phase 1: Project Foundation (Steps 1–3)

| Step | Title                              | Prompt File | Dependencies | Estimated Effort |
| ---- | ---------------------------------- | ----------- | ------------ | ---------------- |
| 1    | Project Scaffolding                | `prompts/1.txt` | None         | 1–2 hours        |
| 2    | Message Protocol & Serialization   | `prompts/2.txt` | Step 1       | 2–3 hours        |
| 3    | Logging Module                     | `prompts/3.txt` | Step 1       | 1–2 hours        |

**Phase 1 outcome:** A Tauri 2 + Svelte project that builds and launches on all platforms, with all network message types defined and a working logging system.

---

## Phase 2: Networking & Discovery (Steps 4–7)

| Step | Title                              | Prompt File | Dependencies | Estimated Effort |
| ---- | ---------------------------------- | ----------- | ------------ | ---------------- |
| 4    | TCP Networking Layer               | `prompts/4.txt` | Steps 1–3   | 3–4 hours        |
| 5    | UDP Networking & Clock Sync        | `prompts/5.txt` | Steps 1–3   | 3–4 hours        |
| 6    | mDNS Session Discovery             | `prompts/6.txt` | Step 1       | 2–3 hours        |
| 7    | Session Manager                    | `prompts/7.txt` | Steps 4–6   | 3–4 hours        |

**Phase 2 outcome:** Host can create a session (advertised via mDNS), peers can discover and join. TCP connections are established with heartbeat. UDP clock sync is running and converging. Peer list is managed.

---

## Phase 3: Audio & File Transfer (Steps 8–12)

| Step | Title                              | Prompt File | Dependencies | Estimated Effort |
| ---- | ---------------------------------- | ----------- | ------------ | ---------------- |
| 8    | File Transfer                      | `prompts/8.txt` | Step 7       | 3–4 hours        |
| 9    | Audio Engine (Decode & Playback)   | `prompts/9.txt` | Step 1       | 3–4 hours        |
| 10   | Playback Scheduler & Sync          | `prompts/10.txt` | Steps 5, 7, 9 | 4–5 hours      |
| 11   | Queue Manager                      | `prompts/11.txt` | Steps 8, 10 | 2–3 hours        |
| 12   | Song Request Flow                  | `prompts/12.txt` | Steps 8, 11 | 2–3 hours        |

**Phase 3 outcome:** Full backend functionality — files transfer between host and peers, audio decodes and plays in sync, queue works with auto-advance and pre-distribution, peers can request songs.

---

## Phase 4: Frontend & Integration (Steps 13–17)

| Step | Title                              | Prompt File | Dependencies | Estimated Effort |
| ---- | ---------------------------------- | ----------- | ------------ | ---------------- |
| 13   | Tauri IPC Commands & Events        | `prompts/13.txt` | Steps 7–12  | 3–4 hours        |
| 14   | Frontend: Global Styles & Layout   | `prompts/14.txt` | Step 1       | 1–2 hours        |
| 15   | Frontend: Home & Session Browser   | `prompts/15.txt` | Steps 13, 14 | 2–3 hours       |
| 16   | Frontend: Host Session Screen      | `prompts/16.txt` | Steps 13, 14 | 3–4 hours        |
| 17   | Frontend: Peer Session Screen      | `prompts/17.txt` | Steps 13, 14 | 2–3 hours        |

**Phase 4 outcome:** The app has a complete UI. Host can create jams, peers can discover and join. Music plays in sync. Queue is visible and interactive. Song requests work end-to-end.

---

## Phase 5: Polish & Ship (Steps 18–20)

| Step | Title                              | Prompt File | Dependencies | Estimated Effort |
| ---- | ---------------------------------- | ----------- | ------------ | ---------------- |
| 18   | Error Handling & Edge Cases        | `prompts/18.txt` | Steps 1–17  | 3–4 hours        |
| 19   | Peer Reconnection                  | `prompts/19.txt` | Steps 7, 10, 11 | 2–3 hours     |
| 20   | CI/CD & Packaging                  | `prompts/20.txt` | Steps 1–19  | 2–3 hours        |

**Phase 5 outcome:** All error cases handled gracefully, peers can reconnect mid-session, CI/CD pipeline produces installers for Windows, macOS, and Linux.

---

## Dependency Graph

```
Step 1 (Scaffolding)
├── Step 2 (Messages)
├── Step 3 (Logging)
├── Step 6 (mDNS)
├── Step 9 (Audio Engine)
└── Step 14 (Global Styles)

Steps 2, 3 ──► Step 4 (TCP)
Steps 2, 3 ──► Step 5 (UDP + Clock Sync)

Steps 4, 5, 6 ──► Step 7 (Session Manager)

Step 7 ──► Step 8 (File Transfer)

Steps 5, 7, 9 ──► Step 10 (Playback Scheduler)

Steps 8, 10 ──► Step 11 (Queue Manager)
Steps 8, 11 ──► Step 12 (Song Requests)

Steps 7–12 ──► Step 13 (Tauri IPC)

Steps 13, 14 ──► Step 15 (Home / Browser UI)
Steps 13, 14 ──► Step 16 (Host Session UI)
Steps 13, 14 ──► Step 17 (Peer Session UI)

Steps 1–17 ──► Step 18 (Error Handling)
Steps 7, 10, 11 ──► Step 19 (Reconnection)
Steps 1–19 ──► Step 20 (CI/CD & Packaging)
```

---

## Total Estimated Effort

| Phase | Steps | Estimated Hours |
| ----- | ----- | --------------- |
| 1     | 1–3   | 4–7 hours       |
| 2     | 4–7   | 11–15 hours     |
| 3     | 8–12  | 14–19 hours     |
| 4     | 13–17 | 11–16 hours     |
| 5     | 18–20 | 7–10 hours      |
| **Total** |   | **47–67 hours** |
