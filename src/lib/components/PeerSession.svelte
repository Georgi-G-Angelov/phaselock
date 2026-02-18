<script lang="ts">
    import { onMount, onDestroy, createEventDispatcher } from 'svelte';
    import { invoke } from '@tauri-apps/api/core';
    import { listen } from '@tauri-apps/api/event';
    import type { UnlistenFn } from '@tauri-apps/api/event';
    import NowPlaying from './NowPlaying.svelte';
    import Queue from './Queue.svelte';
    import Toast from './Toast.svelte';
    import { sessionStore, peersStore } from '../stores/session';
    import { playbackStore, volumeStore } from '../stores/playback';
    import { queueStore } from '../stores/queue';
    import {
        EVENTS,
        type QueueItem,
        type PlaybackState,
        type PlaybackPosition,
        type SyncLatency,
        type SyncState,
        type ErrorEvent,
    } from '../types';

    const dispatch = createEventDispatcher<{ disconnected: void }>();

    let toast: Toast;
    let unlisteners: UnlistenFn[] = [];

    // Connection / latency state
    let connected = true;
    let latencyMs: number | null = null;

    // Sync state
    let syncing = false;
    let syncMessage = 'Syncing...';

    // Volume control
    let muted = false;
    let premuteVolume = 1.0;

    $: sessionName = $sessionStore?.session_name ?? 'Jam Session';
    $: myPeerId = $sessionStore?.peer_id ?? null;

    $: latencyColor = latencyMs === null ? 'var(--text-secondary)'
        : latencyMs < 10 ? 'var(--accent-green)'
        : latencyMs <= 100 ? '#f0c040'
        : 'var(--error-red)';

    function onVolumeChange(e: Event) {
        const value = parseFloat((e.target as HTMLInputElement).value) / 100;
        volumeStore.set(value);
        invoke('set_volume', { volume: value });
        if (value > 0) muted = false;
    }

    function toggleMute() {
        if (muted) {
            muted = false;
            volumeStore.set(premuteVolume);
            invoke('set_volume', { volume: premuteVolume });
        } else {
            premuteVolume = $volumeStore;
            muted = true;
            volumeStore.set(0);
            invoke('set_volume', { volume: 0 });
        }
    }

    function leaveJam() {
        // Fire-and-forget backend cleanup — navigate immediately.
        invoke('leave_session').catch(e => console.error('Failed to leave session:', e));
        sessionStore.set(null);
        peersStore.set([]);
        queueStore.set([]);
        playbackStore.set({ state: 'stopped', file_name: '', position_ms: 0, duration_ms: 0 });
        dispatch('disconnected');
    }

    onMount(async () => {
        unlisteners.push(
            await listen<{ queue: QueueItem[] }>(EVENTS.QUEUE_UPDATED, (e) => {
                console.log('[PeerSession] QUEUE_UPDATED:', e.payload.queue.length, 'items', e.payload.queue);
                queueStore.set(e.payload.queue);
            }),
            await listen<PlaybackState>(EVENTS.PLAYBACK_STATE_CHANGED, (e) => {
                console.log('[PeerSession] PLAYBACK_STATE_CHANGED:', e.payload);
                playbackStore.set(e.payload);
            }),
            await listen<PlaybackPosition>(EVENTS.PLAYBACK_POSITION, (e) => {
                console.log('[PeerSession] PLAYBACK_POSITION:', e.payload.position_ms, '/', e.payload.duration_ms);
                playbackStore.update(s => ({ ...s, position_ms: e.payload.position_ms, duration_ms: e.payload.duration_ms }));
            }),
            await listen(EVENTS.SESSION_ENDED, () => {
                toast?.show('Host ended the session.', 'info');
                setTimeout(() => {
                    sessionStore.set(null);
                    peersStore.set([]);
                    queueStore.set([]);
                    playbackStore.set({ state: 'stopped', file_name: '', position_ms: 0, duration_ms: 0 });
                    dispatch('disconnected');
                }, 1500);
            }),
            await listen(EVENTS.HOST_DISCONNECTED, () => {
                connected = false;
                toast?.show('Host disconnected — session ended.', 'error');
                setTimeout(() => {
                    sessionStore.set(null);
                    peersStore.set([]);
                    queueStore.set([]);
                    playbackStore.set({ state: 'stopped', file_name: '', position_ms: 0, duration_ms: 0 });
                    dispatch('disconnected');
                }, 2000);
            }),
            await listen<SyncLatency>(EVENTS.SYNC_LATENCY_UPDATED, (e) => {
                if (myPeerId !== null && e.payload.peer_id === myPeerId) {
                    latencyMs = e.payload.latency_ms;
                }
            }),
            await listen<ErrorEvent>(EVENTS.ERROR_GENERAL, (e) => {
                toast?.show(e.payload.message, 'error');
            }),
            await listen<SyncState>(EVENTS.SYNC_STATE, (e) => {
                console.log('[PeerSession] SYNC_STATE:', e.payload);
                syncing = e.payload.syncing;
                syncMessage = e.payload.message || 'Syncing...';
            }),
        );
    });

    onDestroy(() => {
        unlisteners.forEach(u => u());
    });
</script>

<Toast bind:this={toast} />

{#if syncing}
<div class="sync-overlay">
    <div class="sync-content">
        <div class="sync-spinner"></div>
        <p class="sync-text">{syncMessage}</p>
    </div>
</div>
{/if}

<div class="peer-session flex-col fade-in">
    <header class="session-header flex items-center justify-between p-4">
        <h3>🎵 {sessionName}</h3>
        <div class="flex items-center gap-4">
            <span class="connection-status text-sm">
                {#if connected}
                    <span class="text-secondary">Connected</span>
                    {#if latencyMs !== null}
                        <span class="text-secondary"> — latency: </span>
                        <span style:color={latencyColor}>{latencyMs}ms</span>
                    {/if}
                {:else}
                    <span style:color="var(--error-red)">Disconnected</span>
                {/if}
            </span>
            <button class="btn btn-secondary" on:click={leaveJam}>
                Leave Jam
            </button>
        </div>
    </header>

    <div class="session-body flex flex-1 overflow-y-auto">
        <!-- Left panel: Now Playing + Volume -->
        <div class="main-area flex-col flex-1 gap-4 p-4">
            <NowPlaying hostControls={false} />

            <!-- Volume control -->
            <div class="volume-control card flex items-center gap-3">
                <button class="btn-icon volume-icon" on:click={toggleMute} aria-label={muted ? 'Unmute' : 'Mute'}>
                    {muted || $volumeStore === 0 ? '🔇' : '🔊'}
                </button>
                <input
                    type="range"
                    min="0"
                    max="100"
                    value={muted ? 0 : Math.round($volumeStore * 100)}
                    on:input={onVolumeChange}
                    class="volume-slider"
                    aria-label="Volume"
                />
                <span class="text-xs text-secondary volume-label">
                    {muted ? 0 : Math.round($volumeStore * 100)}%
                </span>
            </div>
        </div>

        <!-- Right panel: Queue (read-only) -->
        <aside class="queue-sidebar flex-col p-4">
            <Queue editable={false} showRequestButton={true} on:toast-message={(e) => toast?.show(e.detail.message, e.detail.variant)} />
        </aside>
    </div>
</div>

<style>
    .peer-session {
        height: 100vh;
    }

    .session-header {
        border-bottom: 1px solid var(--border-subtle);
    }

    .session-body {
        min-height: 0;
    }

    .main-area {
        min-width: 0;
    }

    .queue-sidebar {
        width: 300px;
        flex-shrink: 0;
        border-left: 1px solid var(--border-subtle);
        overflow-y: auto;
    }

    .volume-control {
        padding: 0.75rem 1rem;
    }

    .volume-icon {
        font-size: 1.25rem;
        width: 2rem;
        height: 2rem;
        flex-shrink: 0;
    }

    .volume-slider {
        flex: 1;
        height: 4px;
        -webkit-appearance: none;
        appearance: none;
        background: var(--bg-elevated);
        border-radius: 2px;
        outline: none;
        cursor: pointer;
    }

    .volume-slider::-webkit-slider-thumb {
        -webkit-appearance: none;
        appearance: none;
        width: 14px;
        height: 14px;
        border-radius: 50%;
        background: var(--accent-green);
        cursor: pointer;
    }

    .volume-slider::-moz-range-thumb {
        width: 14px;
        height: 14px;
        border-radius: 50%;
        background: var(--accent-green);
        cursor: pointer;
        border: none;
    }

    .volume-label {
        width: 2.5rem;
        text-align: right;
        flex-shrink: 0;
    }

    .connection-status {
        white-space: nowrap;
    }

    .sync-overlay {
        position: fixed;
        top: 0;
        left: 0;
        right: 0;
        bottom: 0;
        background: rgba(0, 0, 0, 0.75);
        display: flex;
        align-items: center;
        justify-content: center;
        z-index: 100;
    }

    .sync-content {
        display: flex;
        flex-direction: column;
        align-items: center;
        gap: 1rem;
    }

    .sync-spinner {
        width: 48px;
        height: 48px;
        border: 4px solid var(--bg-elevated);
        border-top-color: var(--accent-green);
        border-radius: 50%;
        animation: spin 0.8s linear infinite;
    }

    @keyframes spin {
        to { transform: rotate(360deg); }
    }

    .sync-text {
        color: var(--text-primary);
        font-size: 1.1rem;
    }
</style>
