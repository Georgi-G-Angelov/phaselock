import { writable } from 'svelte/store';
import type { QueueItem } from '../types';

/** The current playback queue. */
export const queueStore = writable<QueueItem[]>([]);
