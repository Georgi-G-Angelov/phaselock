//! In-memory YouTube download task queue.
//!
//! Tasks are enqueued instantly when the user submits a URL.  Two background
//! workers process them:
//!   1. **Metadata worker** — fetches title/artist from yt-dlp (fast).
//!   2. **Download worker** — downloads audio, decodes, and adds to the playback queue.

use serde::{Deserialize, Serialize};
use std::collections::VecDeque;
use std::sync::Arc;
use tokio::sync::Mutex;

/// The status of a single download task.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub enum DownloadStatus {
    /// Search query submitted — waiting for URL resolution.
    Searching,
    /// Just added — metadata not yet fetched.
    Pending,
    /// Metadata is being fetched.
    FetchingMeta,
    /// Metadata resolved — waiting for download worker.
    Queued,
    /// Audio is being downloaded / decoded.
    Downloading,
    /// Something went wrong.
    Failed,
}

/// A single YouTube download task.
#[derive(Debug, Clone, Serialize)]
pub struct DownloadTask {
    /// Unique id for this task (sequential, for ordering).
    pub id: u32,
    /// The original YouTube URL.
    pub url: String,
    /// Parsed song title (from "Artist - Title" pattern).
    pub title: String,
    /// Parsed artist name.
    pub artist: String,
    /// Current status.
    pub status: DownloadStatus,
    /// Error message (if failed).
    pub error: Option<String>,
}

/// Thread-safe download queue shared between the command layer and the workers.
#[derive(Debug, Clone)]
pub struct DownloadQueue {
    inner: Arc<Mutex<DownloadQueueInner>>,
}

#[derive(Debug)]
struct DownloadQueueInner {
    tasks: VecDeque<DownloadTask>,
    next_id: u32,
}

/// Snapshot of the queue sent to the frontend.
#[derive(Debug, Clone, Serialize)]
pub struct DownloadQueueSnapshot {
    pub tasks: Vec<DownloadTask>,
}

impl DownloadQueue {
    pub fn new() -> Self {
        Self {
            inner: Arc::new(Mutex::new(DownloadQueueInner {
                tasks: VecDeque::new(),
                next_id: 1,
            })),
        }
    }

    /// Add a new task with unknown metadata. Returns its id.
    pub async fn enqueue(&self, url: String) -> u32 {
        let mut inner = self.inner.lock().await;
        let id = inner.next_id;
        inner.next_id += 1;
        inner.tasks.push_back(DownloadTask {
            id,
            url,
            title: "Unknown".into(),
            artist: "Unknown".into(),
            status: DownloadStatus::Pending,
            error: None,
        });
        id
    }

    /// Add a new task in Searching status (URL unknown yet). Returns its id.
    pub async fn enqueue_search(&self, query: String) -> u32 {
        let mut inner = self.inner.lock().await;
        let id = inner.next_id;
        inner.next_id += 1;
        inner.tasks.push_back(DownloadTask {
            id,
            url: String::new(),
            title: query,
            artist: "YouTube Search".into(),
            status: DownloadStatus::Searching,
            error: None,
        });
        id
    }

    /// Resolve a search task: set its URL and move to Pending so metadata worker picks it up.
    pub async fn resolve_search(&self, id: u32, url: String) {
        let mut inner = self.inner.lock().await;
        if let Some(task) = inner.tasks.iter_mut().find(|t| t.id == id) {
            if task.status == DownloadStatus::Searching {
                task.url = url;
                task.status = DownloadStatus::Pending;
            }
        }
    }

    /// Take the next task that needs metadata (Pending → FetchingMeta).
    pub async fn take_next_pending(&self) -> Option<DownloadTask> {
        let mut inner = self.inner.lock().await;
        for task in inner.tasks.iter_mut() {
            if task.status == DownloadStatus::Pending {
                task.status = DownloadStatus::FetchingMeta;
                return Some(task.clone());
            }
        }
        None
    }

    /// Update a task's metadata after successful fetch and mark it Queued.
    pub async fn resolve_metadata(&self, id: u32, title: String, artist: String) {
        let mut inner = self.inner.lock().await;
        if let Some(task) = inner.tasks.iter_mut().find(|t| t.id == id) {
            task.title = title;
            task.artist = artist;
            task.status = DownloadStatus::Queued;
        }
    }

    /// Take the next task ready for download (Queued → Downloading).
    pub async fn take_next_queued(&self) -> Option<DownloadTask> {
        let mut inner = self.inner.lock().await;
        for task in inner.tasks.iter_mut() {
            if task.status == DownloadStatus::Queued {
                task.status = DownloadStatus::Downloading;
                return Some(task.clone());
            }
        }
        None
    }

    /// Mark a task as failed with an error message.
    pub async fn mark_failed(&self, id: u32, error: String) {
        let mut inner = self.inner.lock().await;
        if let Some(task) = inner.tasks.iter_mut().find(|t| t.id == id) {
            task.status = DownloadStatus::Failed;
            task.error = Some(error);
        }
    }

    /// Remove a completed or failed task from the queue.
    pub async fn remove(&self, id: u32) {
        let mut inner = self.inner.lock().await;
        inner.tasks.retain(|t| t.id != id);
    }

    /// Get a snapshot of all tasks (for emitting to the frontend).
    pub async fn snapshot(&self) -> DownloadQueueSnapshot {
        let inner = self.inner.lock().await;
        DownloadQueueSnapshot {
            tasks: inner.tasks.iter().cloned().collect(),
        }
    }

    /// Clear all tasks (used on session end).
    pub async fn clear(&self) {
        let mut inner = self.inner.lock().await;
        inner.tasks.clear();
    }
}

// ── Search Queue ────────────────────────────────────────────────────────────

/// A pending YouTube search task.
#[derive(Debug, Clone)]
pub struct SearchTask {
    pub id: u32,
    pub query: String,
    /// The download task id that was created for this search (to resolve later).
    pub download_task_id: u32,
}

/// Thread-safe search queue.  The search worker pops tasks, runs
/// `ytsearch1:<query>`, and enqueues the resulting URL into the DownloadQueue.
#[derive(Debug, Clone)]
pub struct SearchQueue {
    inner: Arc<Mutex<SearchQueueInner>>,
}

#[derive(Debug)]
struct SearchQueueInner {
    tasks: VecDeque<SearchTask>,
    next_id: u32,
}

impl SearchQueue {
    pub fn new() -> Self {
        Self {
            inner: Arc::new(Mutex::new(SearchQueueInner {
                tasks: VecDeque::new(),
                next_id: 1,
            })),
        }
    }

    /// Add a search query. Returns its id.
    pub async fn enqueue(&self, query: String, download_task_id: u32) -> u32 {
        let mut inner = self.inner.lock().await;
        let id = inner.next_id;
        inner.next_id += 1;
        inner.tasks.push_back(SearchTask { id, query, download_task_id });
        id
    }

    /// Take the next search task.
    pub async fn take_next(&self) -> Option<SearchTask> {
        let mut inner = self.inner.lock().await;
        inner.tasks.pop_front()
    }

    /// Clear all pending searches.
    pub async fn clear(&self) {
        let mut inner = self.inner.lock().await;
        inner.tasks.clear();
    }
}
