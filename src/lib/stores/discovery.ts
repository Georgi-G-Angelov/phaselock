import { writable } from 'svelte/store';
import type { DiscoveredSession } from '../types';

/** Sessions discovered on the local network via mDNS. */
export const discoveredSessions = writable<DiscoveredSession[]>([]);
