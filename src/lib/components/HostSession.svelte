<script lang="ts">
    import { createEventDispatcher, onMount, onDestroy } from 'svelte';
    import { invoke } from '@tauri-apps/api/core';
    import { listen } from '@tauri-apps/api/event';
    import type { UnlistenFn } from '@tauri-apps/api/event';
    import NowPlaying from './NowPlaying.svelte';
    import Queue from './Queue.svelte';
    import PeerList from './PeerList.svelte';
    import SongRequests from './SongRequests.svelte';
    import Toast from './Toast.svelte';
    import { sessionStore, peersStore, isHost } from '../stores/session';
    import { playbackStore } from '../stores/playback';
    import { queueStore } from '../stores/queue';
    import { songRequests } from '../stores/requests';
    import { EVENTS } from '../types';
    import type { PeerInfo, PlaybackState, PlaybackPosition, QueueItem, SongRequest, ErrorEvent } from '../types';

    const dispatch = createEventDispatcher<{ ended: void }>();

    let toast: Toast;
    let confirmingEnd = false;
    let unlisteners: UnlistenFn[] = [];

    onMount(async () => {
        unlisteners.push(
            await listen<PeerInfo>(EVENTS.PEER_JOINED, (e) => {
                peersStore.update(peers => {
                    if (peers.find(p => p.peer_id === e.payload.peer_id)) return peers;
                    return [...peers, e.payload];
                });
                toast?.show(`${e.payload.display_name} joined`, 'info');
            }),
            await listen<{ peer_id: number }>(EVENTS.PEER_LEFT, (e) => {
                peersStore.update(peers => peers.filter(p => p.peer_id !== e.payload.peer_id));
            }),
            await listen<{ queue: QueueItem[] }>(EVENTS.QUEUE_UPDATED, (e) => {
                queueStore.set(e.payload.queue);
            }),
            await listen<PlaybackState>(EVENTS.PLAYBACK_STATE_CHANGED, (e) => {
                playbackStore.set(e.payload);
            }),
            await listen<PlaybackPosition>(EVENTS.PLAYBACK_POSITION, (e) => {
                playbackStore.update(s => ({ ...s, position_ms: e.payload.position_ms, duration_ms: e.payload.duration_ms }));
            }),
            await listen(EVENTS.TRACK_FINISHED, () => {
                playbackStore.update(s => ({ ...s, state: 'stopped', position_ms: 0 }));
            }),
            await listen<SongRequest>(EVENTS.REQUEST_INCOMING, (e) => {
                songRequests.update(r => [...r, e.payload]);
            }),
            await listen<ErrorEvent>(EVENTS.ERROR_GENERAL, (e) => {
                toast?.show(e.payload.message, 'error');
            }),
        );
    });

    onDestroy(() => {
        unlisteners.forEach(fn => fn());
    });

    async function endSession() {
        if (!confirmingEnd) {
            confirmingEnd = true;
            return;
        }
        try {
            await invoke('leave_session');
        } catch { /* best-effort */ }
        sessionStore.set(null);
        isHost.set(false);
        peersStore.set([]);
        queueStore.set([]);
        songRequests.set([]);
        playbackStore.set({ state: 'stopped', file_name: '', position_ms: 0, duration_ms: 0 });
        dispatch('ended');
    }

    function cancelEnd() {
        confirmingEnd = false;
    }

    $: peerCount = $peersStore.length;
</script>

<Toast bind:this={toast} />

<div class="host-session flex-col fade-in" on:click={cancelEnd} role="presentation">
    <header class="session-header flex items-center justify-between p-4">
        <div class="flex items-center gap-2">
            <h3>🎵 {$sessionStore?.session_name ?? 'Your Jam'}</h3>
        </div>
        <span class="text-sm text-secondary">{peerCount} {peerCount === 1 ? 'peer' : 'peers'} connected</span>
        <div class="flex items-center gap-2">
            {#if confirmingEnd}
                <span class="text-sm text-error">End session?</span>
                <button class="btn btn-danger" on:click|stopPropagation={endSession}>Confirm</button>
                <button class="btn btn-secondary" on:click|stopPropagation={cancelEnd}>Cancel</button>
            {:else}
                <button class="btn btn-danger" on:click|stopPropagation={endSession}>End Session</button>
            {/if}
        </div>
    </header>

    <div class="session-body flex flex-1">
        <aside class="panel-left flex-col p-4">
            <PeerList />
        </aside>
        <div class="panel-center flex-col flex-1 gap-4 p-4 overflow-y-auto">
            <NowPlaying hostControls={true} on:toast-message={(e) => toast?.show(e.detail.message, e.detail.variant)} />
            <SongRequests />
        </div>
        <aside class="panel-right flex-col p-4 overflow-y-auto">
            <Queue editable={true} />
        </aside>
    </div>
</div>

<style>
    .host-session {
        height: 100vh;
    }

    .session-header {
        border-bottom: 1px solid var(--border-subtle);
        flex-shrink: 0;
    }

    .session-body {
        min-height: 0;
    }

    .panel-left {
        width: 220px;
        flex-shrink: 0;
        border-right: 1px solid var(--border-subtle);
        overflow-y: auto;
    }

    .panel-right {
        width: 300px;
        flex-shrink: 0;
        border-left: 1px solid var(--border-subtle);
    }

    .panel-center {
        min-width: 0;
    }
</style>
