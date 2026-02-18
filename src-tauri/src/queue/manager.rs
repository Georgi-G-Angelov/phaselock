use crate::network::messages::{QueueItem, QueueItemStatus};
use uuid::Uuid;

// ── QueueManager ────────────────────────────────────────────────────────────

/// Manages the ordered playback queue.
///
/// The host owns the authoritative queue. After every mutation the host
/// broadcasts a `QueueUpdate` to all peers (handled by the caller).
pub struct QueueManager {
    queue: Vec<QueueItem>,
    /// Index of the currently playing (or about-to-play) track.
    current_index: Option<usize>,
}

impl QueueManager {
    /// Create an empty queue.
    pub fn new() -> Self {
        Self {
            queue: Vec::new(),
            current_index: None,
        }
    }

    /// Reset the queue to its initial empty state.
    pub fn clear(&mut self) {
        self.queue.clear();
        self.current_index = None;
    }

    // ── Mutations ───────────────────────────────────────────────────────

    /// Add a track to the end of the queue. Returns the generated `Uuid`.
    pub fn add(&mut self, file_name: String, duration_secs: f64, added_by: String) -> Uuid {
        let id = Uuid::new_v4();
        self.queue.push(QueueItem {
            id,
            file_name,
            duration_secs,
            added_by,
            status: QueueItemStatus::Transferring,
        });
        log::info!("Queue: added track {} (total={})", id, self.queue.len());
        id
    }

    /// Remove a track by id. Returns `true` if found and removed.
    ///
    /// If the removed track was the current track, `current_index` is
    /// adjusted so it points to the same logical position (or `None`
    /// if the queue becomes empty).
    pub fn remove(&mut self, id: Uuid) -> bool {
        let pos = self.queue.iter().position(|q| q.id == id);
        let Some(idx) = pos else { return false };

        self.queue.remove(idx);
        log::info!("Queue: removed track {} (total={})", id, self.queue.len());

        // Adjust current_index.
        if let Some(ci) = self.current_index {
            if idx < ci {
                // Removed before current → shift current back.
                self.current_index = Some(ci - 1);
            } else if idx == ci {
                // Removed the current track.
                if self.queue.is_empty() {
                    self.current_index = None;
                } else if ci >= self.queue.len() {
                    // Was last item; clamp to new last.
                    self.current_index = Some(self.queue.len() - 1);
                }
                // else: ci still valid, now pointing at the next song.
            }
            // idx > ci: nothing to adjust.
        }

        true
    }

    /// Move a track from `from_index` to `to_index` (drag-and-drop reorder).
    /// Returns `true` on success.
    pub fn reorder(&mut self, from_index: usize, to_index: usize) -> bool {
        if from_index >= self.queue.len() || to_index >= self.queue.len() {
            return false;
        }
        if from_index == to_index {
            return true;
        }

        let item = self.queue.remove(from_index);
        self.queue.insert(to_index, item);

        // Adjust current_index so it keeps pointing at the same track.
        if let Some(ci) = self.current_index {
            if from_index == ci {
                // The current track was moved.
                self.current_index = Some(to_index);
            } else {
                let mut new_ci = ci;
                // Removal phase: if from < ci, ci shifted left.
                if from_index < ci {
                    new_ci -= 1;
                }
                // Insertion phase: if to <= new_ci, ci shifted right.
                if to_index <= new_ci {
                    new_ci += 1;
                }
                self.current_index = Some(new_ci);
            }
        }

        log::info!("Queue: reordered {} → {}", from_index, to_index);
        true
    }

    // ── Queries ─────────────────────────────────────────────────────────

    /// Get the full queue (cloned, for broadcasting to peers).
    pub fn get_queue(&self) -> Vec<QueueItem> {
        self.queue.clone()
    }

    /// Get a reference to the currently playing track.
    pub fn current(&self) -> Option<&QueueItem> {
        self.current_index.and_then(|i| self.queue.get(i))
    }

    /// Peek at the next track after the current one, without advancing.
    pub fn peek_next(&self) -> Option<&QueueItem> {
        let ci = self.current_index?;
        self.queue.get(ci + 1)
    }

    /// Check if a track's status is `Ready`.
    pub fn is_ready(&self, id: Uuid) -> bool {
        self.queue
            .iter()
            .any(|q| q.id == id && q.status == QueueItemStatus::Ready)
    }

    /// Get the current queue length.
    pub fn len(&self) -> usize {
        self.queue.len()
    }

    /// Check if the queue is empty.
    pub fn is_empty(&self) -> bool {
        self.queue.is_empty()
    }

    /// Get the current index (if any).
    pub fn current_index(&self) -> Option<usize> {
        self.current_index
    }

    // ── Status transitions ──────────────────────────────────────────────

    /// Mark a track as `Ready` (all peers have confirmed the file).
    pub fn mark_ready(&mut self, id: Uuid) {
        if let Some(item) = self.queue.iter_mut().find(|q| q.id == id) {
            item.status = QueueItemStatus::Ready;
            log::info!("Queue: track {} marked Ready", id);
        }
    }

    /// Mark a track as `Playing`.
    pub fn mark_playing(&mut self, id: Uuid) {
        if let Some(item) = self.queue.iter_mut().find(|q| q.id == id) {
            item.status = QueueItemStatus::Playing;
            log::info!("Queue: track {} marked Playing", id);
        }
    }

    /// Mark a track as `Played` (finished playback).
    pub fn mark_played(&mut self, id: Uuid) {
        if let Some(item) = self.queue.iter_mut().find(|q| q.id == id) {
            item.status = QueueItemStatus::Played;
            log::info!("Queue: track {} marked Played", id);
        }
    }

    // ── Playback progression ────────────────────────────────────────────

    /// Advance to the next track.
    ///
    /// If the current track is set, marks it `Played` and moves to the next.
    /// If there is no current track yet, starts at index 0.
    ///
    /// Returns `Some(&QueueItem)` for the new current track if one is
    /// available and its status is `Ready`. Returns `None` if there is no
    /// next track or the next track is not yet ready.
    pub fn advance(&mut self) -> Option<&QueueItem> {
        let next_index = match self.current_index {
            Some(ci) => {
                // Mark current as Played.
                if let Some(item) = self.queue.get_mut(ci) {
                    item.status = QueueItemStatus::Played;
                }
                ci + 1
            }
            None => 0,
        };

        if next_index < self.queue.len() {
            self.current_index = Some(next_index);
            let item = &self.queue[next_index];
            if item.status == QueueItemStatus::Ready {
                log::info!("Queue: advanced to track {} (index {})", item.id, next_index);
                Some(item)
            } else {
                log::info!(
                    "Queue: advanced to index {} but track {} is {:?}",
                    next_index,
                    item.id,
                    item.status
                );
                None
            }
        } else {
            self.current_index = Some(next_index); // past end = nothing playing
            log::info!("Queue: no more tracks");
            None
        }
    }

    /// Skip the current track and advance to the next one.
    ///
    /// Marks the current track as `Played` and calls `advance` logic
    /// internally. Returns the next track if available and `Ready`.
    pub fn skip(&mut self) -> Option<&QueueItem> {
        self.advance()
    }
}

impl Default for QueueManager {
    fn default() -> Self {
        Self::new()
    }
}

// ── Tests ───────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    /// Helper: add N tracks and return their IDs.
    fn add_tracks(qm: &mut QueueManager, n: usize) -> Vec<Uuid> {
        (0..n)
            .map(|i| qm.add(format!("song{}.mp3", i + 1), 180.0 + i as f64, "host".into()))
            .collect()
    }

    // ── Basic add / query ───────────────────────────────────────────────

    #[test]
    fn test_add_three_songs_order_preserved() {
        let mut qm = QueueManager::new();
        let ids = add_tracks(&mut qm, 3);

        let queue = qm.get_queue();
        assert_eq!(queue.len(), 3);
        assert_eq!(queue[0].id, ids[0]);
        assert_eq!(queue[1].id, ids[1]);
        assert_eq!(queue[2].id, ids[2]);

        // All start as Transferring.
        for item in &queue {
            assert_eq!(item.status, QueueItemStatus::Transferring);
        }
    }

    #[test]
    fn test_add_sets_metadata() {
        let mut qm = QueueManager::new();
        let id = qm.add("cool.mp3".into(), 200.5, "alice".into());

        let item = &qm.get_queue()[0];
        assert_eq!(item.id, id);
        assert_eq!(item.file_name, "cool.mp3");
        assert_eq!(item.duration_secs, 200.5);
        assert_eq!(item.added_by, "alice");
    }

    // ── Remove ──────────────────────────────────────────────────────────

    #[test]
    fn test_remove_middle_song() {
        let mut qm = QueueManager::new();
        let ids = add_tracks(&mut qm, 3);

        assert!(qm.remove(ids[1]));
        let queue = qm.get_queue();
        assert_eq!(queue.len(), 2);
        assert_eq!(queue[0].id, ids[0]);
        assert_eq!(queue[1].id, ids[2]);
    }

    #[test]
    fn test_remove_nonexistent_returns_false() {
        let mut qm = QueueManager::new();
        add_tracks(&mut qm, 2);
        assert!(!qm.remove(Uuid::new_v4()));
    }

    #[test]
    fn test_remove_currently_playing_adjusts_index() {
        let mut qm = QueueManager::new();
        let ids = add_tracks(&mut qm, 3);

        // Mark all ready, advance to index 0, then index 1.
        for &id in &ids {
            qm.mark_ready(id);
        }
        qm.advance(); // current = 0
        qm.advance(); // current = 1

        assert_eq!(qm.current().unwrap().id, ids[1]);

        // Remove the current (index 1 = ids[1]).
        qm.remove(ids[1]);

        // current_index should still be 1, now pointing at ids[2].
        assert_eq!(qm.current().unwrap().id, ids[2]);
    }

    #[test]
    fn test_remove_before_current_adjusts_index() {
        let mut qm = QueueManager::new();
        let ids = add_tracks(&mut qm, 3);

        for &id in &ids {
            qm.mark_ready(id);
        }
        qm.advance(); // current = 0
        qm.advance(); // current = 1

        // Remove ids[0] (before current).
        qm.remove(ids[0]);
        // current_index should shift from 1 → 0, still pointing at ids[1].
        assert_eq!(qm.current().unwrap().id, ids[1]);
    }

    // ── Reorder ─────────────────────────────────────────────────────────

    #[test]
    fn test_reorder_move_last_to_first() {
        let mut qm = QueueManager::new();
        let ids = add_tracks(&mut qm, 3);

        assert!(qm.reorder(2, 0));
        let queue = qm.get_queue();
        assert_eq!(queue[0].id, ids[2]);
        assert_eq!(queue[1].id, ids[0]);
        assert_eq!(queue[2].id, ids[1]);
    }

    #[test]
    fn test_reorder_move_first_to_last() {
        let mut qm = QueueManager::new();
        let ids = add_tracks(&mut qm, 3);

        assert!(qm.reorder(0, 2));
        let queue = qm.get_queue();
        assert_eq!(queue[0].id, ids[1]);
        assert_eq!(queue[1].id, ids[2]);
        assert_eq!(queue[2].id, ids[0]);
    }

    #[test]
    fn test_reorder_out_of_bounds() {
        let mut qm = QueueManager::new();
        add_tracks(&mut qm, 2);
        assert!(!qm.reorder(0, 5));
        assert!(!qm.reorder(5, 0));
    }

    #[test]
    fn test_reorder_same_position() {
        let mut qm = QueueManager::new();
        let ids = add_tracks(&mut qm, 3);
        assert!(qm.reorder(1, 1));
        assert_eq!(qm.get_queue()[1].id, ids[1]);
    }

    #[test]
    fn test_reorder_tracks_current_index() {
        let mut qm = QueueManager::new();
        let ids = add_tracks(&mut qm, 4);

        for &id in &ids {
            qm.mark_ready(id);
        }
        qm.advance(); // current = 0
        qm.advance(); // current = 1, playing ids[1]

        // Move ids[3] (index 3) to index 0.
        qm.reorder(3, 0);
        // Current was at 1. After removing from 3 (>1, no shift), inserting at 0 (<=1, shift +1).
        // So current should be at 2, still ids[1].
        assert_eq!(qm.current().unwrap().id, ids[1]);
    }

    #[test]
    fn test_reorder_moves_current_track() {
        let mut qm = QueueManager::new();
        let ids = add_tracks(&mut qm, 3);

        for &id in &ids {
            qm.mark_ready(id);
        }
        qm.advance(); // current = 0
        qm.advance(); // current = 1 (ids[1])

        // Move the current track (index 1) to index 0.
        qm.reorder(1, 0);
        // current_index should follow the moved track → 0.
        assert_eq!(qm.current_index(), Some(0));
        assert_eq!(qm.current().unwrap().id, ids[1]);
    }

    // ── Status transitions ──────────────────────────────────────────────

    #[test]
    fn test_mark_ready() {
        let mut qm = QueueManager::new();
        let id = qm.add("a.mp3".into(), 100.0, "host".into());
        assert_eq!(qm.get_queue()[0].status, QueueItemStatus::Transferring);

        qm.mark_ready(id);
        assert_eq!(qm.get_queue()[0].status, QueueItemStatus::Ready);
        assert!(qm.is_ready(id));
    }

    #[test]
    fn test_mark_playing() {
        let mut qm = QueueManager::new();
        let id = qm.add("a.mp3".into(), 100.0, "host".into());
        qm.mark_ready(id);
        qm.mark_playing(id);
        assert_eq!(qm.get_queue()[0].status, QueueItemStatus::Playing);
    }

    #[test]
    fn test_mark_played() {
        let mut qm = QueueManager::new();
        let id = qm.add("a.mp3".into(), 100.0, "host".into());
        qm.mark_ready(id);
        qm.mark_playing(id);
        qm.mark_played(id);
        assert_eq!(qm.get_queue()[0].status, QueueItemStatus::Played);
    }

    // ── Advance / progression ───────────────────────────────────────────

    #[test]
    fn test_advance_from_empty_queue() {
        let mut qm = QueueManager::new();
        assert!(qm.advance().is_none());
    }

    #[test]
    fn test_advance_first_track() {
        let mut qm = QueueManager::new();
        let ids = add_tracks(&mut qm, 2);
        qm.mark_ready(ids[0]);

        let next = qm.advance();
        assert!(next.is_some());
        assert_eq!(next.unwrap().id, ids[0]);
        assert_eq!(qm.current_index(), Some(0));
    }

    #[test]
    fn test_advance_to_second_track() {
        let mut qm = QueueManager::new();
        let ids = add_tracks(&mut qm, 3);
        for &id in &ids {
            qm.mark_ready(id);
        }

        qm.advance(); // → ids[0]
        let next = qm.advance(); // → ids[1]
        assert_eq!(next.unwrap().id, ids[1]);
        // ids[0] should now be Played.
        assert_eq!(qm.get_queue()[0].status, QueueItemStatus::Played);
    }

    #[test]
    fn test_advance_past_end_returns_none() {
        let mut qm = QueueManager::new();
        let ids = add_tracks(&mut qm, 1);
        qm.mark_ready(ids[0]);

        qm.advance(); // → ids[0]
        assert!(qm.advance().is_none()); // past end
    }

    #[test]
    fn test_advance_not_ready_returns_none() {
        let mut qm = QueueManager::new();
        add_tracks(&mut qm, 2);
        // Don't mark anything ready.

        // Advance to index 0 — track is Transferring, so returns None.
        assert!(qm.advance().is_none());
        // current_index is still set to 0 though.
        assert_eq!(qm.current_index(), Some(0));
    }

    // ── Skip ────────────────────────────────────────────────────────────

    #[test]
    fn test_skip_advances_to_next() {
        let mut qm = QueueManager::new();
        let ids = add_tracks(&mut qm, 3);
        for &id in &ids {
            qm.mark_ready(id);
        }

        qm.advance(); // current = ids[0]
        let skipped_to = qm.skip();
        assert_eq!(skipped_to.unwrap().id, ids[1]);
        // ids[0] should be Played.
        assert_eq!(qm.get_queue()[0].status, QueueItemStatus::Played);
    }

    #[test]
    fn test_skip_at_end_returns_none() {
        let mut qm = QueueManager::new();
        let ids = add_tracks(&mut qm, 1);
        qm.mark_ready(ids[0]);

        qm.advance(); // current = ids[0]
        assert!(qm.skip().is_none());
    }

    // ── Peek next ───────────────────────────────────────────────────────

    #[test]
    fn test_peek_next_returns_next_without_advancing() {
        let mut qm = QueueManager::new();
        let ids = add_tracks(&mut qm, 3);
        for &id in &ids {
            qm.mark_ready(id);
        }

        qm.advance(); // current = ids[0]

        let peeked = qm.peek_next();
        assert_eq!(peeked.unwrap().id, ids[1]);
        // Current should still be ids[0].
        assert_eq!(qm.current().unwrap().id, ids[0]);
    }

    #[test]
    fn test_peek_next_at_end_returns_none() {
        let mut qm = QueueManager::new();
        let ids = add_tracks(&mut qm, 1);
        qm.mark_ready(ids[0]);
        qm.advance();

        assert!(qm.peek_next().is_none());
    }

    #[test]
    fn test_peek_next_no_current_returns_none() {
        let mut qm = QueueManager::new();
        add_tracks(&mut qm, 2);
        // No current set.
        assert!(qm.peek_next().is_none());
    }

    // ── Edge cases ──────────────────────────────────────────────────────

    #[test]
    fn test_empty_queue_current_is_none() {
        let qm = QueueManager::new();
        assert!(qm.current().is_none());
        assert!(qm.is_empty());
        assert_eq!(qm.len(), 0);
    }

    #[test]
    fn test_single_item_queue_full_lifecycle() {
        let mut qm = QueueManager::new();
        let id = qm.add("only.mp3".into(), 60.0, "host".into());

        qm.mark_ready(id);
        let track = qm.advance().unwrap();
        assert_eq!(track.id, id);

        qm.mark_playing(id);
        assert_eq!(qm.current().unwrap().status, QueueItemStatus::Playing);

        qm.mark_played(id);
        assert_eq!(qm.current().unwrap().status, QueueItemStatus::Played);

        // No next track.
        assert!(qm.advance().is_none());
    }

    #[test]
    fn test_is_ready_false_for_transferring() {
        let mut qm = QueueManager::new();
        let id = qm.add("x.mp3".into(), 100.0, "host".into());
        assert!(!qm.is_ready(id));
    }

    #[test]
    fn test_is_ready_false_for_unknown_id() {
        let qm = QueueManager::new();
        assert!(!qm.is_ready(Uuid::new_v4()));
    }

    #[test]
    fn test_remove_last_item_when_current() {
        let mut qm = QueueManager::new();
        let ids = add_tracks(&mut qm, 1);
        qm.mark_ready(ids[0]);
        qm.advance();

        qm.remove(ids[0]);
        assert!(qm.is_empty());
        assert!(qm.current().is_none());
    }
}
