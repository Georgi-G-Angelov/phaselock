import { writable } from 'svelte/store';
import type { PeerInfo, SessionInfo, ListenersUpdated } from '../types';

/** Current session info, null when not in a session. */
export const sessionStore = writable<SessionInfo | null>(null);

/** List of peers in the current session. */
export const peersStore = writable<PeerInfo[]>([]);

/** Whether the local user is the host. */
export const isHost = writable<boolean>(false);

/** Session listeners info received via UDP (peer side only). */
export const listenersStore = writable<ListenersUpdated | null>(null);
