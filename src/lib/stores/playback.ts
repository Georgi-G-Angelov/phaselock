import { writable } from 'svelte/store';
import type { PlaybackState } from '../types';

/** Current playback state. */
export const playbackStore = writable<PlaybackState>({
    state: 'stopped',
    file_name: '',
    position_ms: 0,
    duration_ms: 0,
});

/** Local volume (0.0 – 1.0). */
export const volumeStore = writable<number>(1.0);
