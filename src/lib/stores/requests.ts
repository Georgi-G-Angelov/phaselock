import { writable } from 'svelte/store';
import type { SongRequest } from '../types';

/** Incoming song requests (host only). */
export const songRequests = writable<SongRequest[]>([]);
