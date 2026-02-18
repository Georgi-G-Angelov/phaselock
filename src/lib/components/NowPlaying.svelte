<script lang="ts">
    import { invoke } from '@tauri-apps/api/core';
    import { createEventDispatcher } from 'svelte';
    import { playbackStore } from '../stores/playback';
    import { queueStore } from '../stores/queue';

    /** Whether to show host transport controls. */
    export let hostControls: boolean = false;

    const dispatch = createEventDispatcher<{ 'toast-message': { message: string; variant: 'success' | 'error' | 'info' } }>();

    $: hasTrack = $playbackStore.file_name !== '';
    $: isPlaying = $playbackStore.state === 'playing';
    $: isPaused = $playbackStore.state === 'paused';
    $: progress = $playbackStore.duration_ms > 0
        ? ($playbackStore.position_ms / $playbackStore.duration_ms) * 100
        : 0;
    $: anyTransferring = $queueStore.some(q => q.status === 'Transferring');
    $: hasReadyTracks = $queueStore.some(q => q.status === 'Ready');
    $: canPlay = hasTrack || hasReadyTracks;

    function formatTime(ms: number): string {
        const totalSec = Math.floor(ms / 1000);
        const m = Math.floor(totalSec / 60);
        const s = totalSec % 60;
        return `${m}:${String(s).padStart(2, '0')}`;
    }

    function handleSeek(e: MouseEvent) {
        if (!hostControls || !hasTrack) return;
        const bar = e.currentTarget as HTMLElement;
        const rect = bar.getBoundingClientRect();
        const ratio = Math.max(0, Math.min(1, (e.clientX - rect.left) / rect.width));
        const posMs = Math.floor(ratio * $playbackStore.duration_ms);
        invoke('seek', { positionMs: posMs });
    }

    async function togglePlay() {
        try {
            if (isPlaying) {
                await invoke('pause');
            } else {
                await invoke('play');
            }
        } catch (e) {
            dispatch('toast-message', { message: `${e}`, variant: 'error' });
        }
    }

    async function handleStop() {
        try { await invoke('stop'); } catch (e) { dispatch('toast-message', { message: `${e}`, variant: 'error' }); }
    }

    async function handleSkip() {
        try { await invoke('skip'); } catch (e) { dispatch('toast-message', { message: `${e}`, variant: 'error' }); }
    }
</script>

<div class="now-playing card flex-col gap-4">
    <h4>Now Playing</h4>

    {#if hasTrack}
        <div class="track-info flex-col gap-1">
            <span class="track-name text-ellipsis">{$playbackStore.file_name}</span>
        </div>

        <!-- Progress bar -->
        <div class="progress-section flex-col gap-1">
            <button
                class="progress-bar"
                class:seekable={hostControls}
                on:click={handleSeek}
                aria-label="Seek"
            >
                <div class="progress-fill" style:width="{progress}%"></div>
            </button>
            <div class="flex justify-between">
                <span class="text-xs text-secondary">{formatTime($playbackStore.position_ms)}</span>
                <span class="text-xs text-secondary">{formatTime($playbackStore.duration_ms)}</span>
            </div>
        </div>
    {:else}
        <p class="text-secondary text-sm">{hostControls ? 'Add songs to the queue, then press play.' : 'Waiting for host to play a track...'}</p>
    {/if}

    <!-- Transport controls (host only) — always visible -->
    {#if hostControls}
        <div class="transport-controls flex-center gap-3">
            <button
                class="btn-icon"
                aria-label="Stop"
                on:click={handleStop}
                disabled={!hasTrack}
            >⏹</button>

            <button
                class="btn-icon large"
                aria-label={isPlaying ? 'Pause' : 'Play'}
                on:click={togglePlay}
                disabled={!canPlay || anyTransferring}
                title={anyTransferring ? 'Waiting for all peers to receive the file' : (!canPlay ? 'Add songs to the queue first' : '')}
            >
                {isPlaying ? '⏸' : '▶'}
            </button>

            <button
                class="btn-icon"
                aria-label="Skip"
                on:click={handleSkip}
                disabled={!hasTrack}
            >⏭</button>
        </div>
    {/if}
</div>

<style>
    .track-name {
        font-size: 1.125rem;
        font-weight: 600;
    }

    .progress-bar {
        width: 100%;
        height: 6px;
        background: var(--bg-elevated);
        border-radius: 3px;
        overflow: hidden;
        border: none;
        padding: 0;
        cursor: default;
        position: relative;
    }

    .progress-bar.seekable {
        cursor: pointer;
    }

    .progress-bar.seekable:hover {
        height: 8px;
    }

    .progress-fill {
        height: 100%;
        background: var(--accent-green);
        border-radius: 3px;
        transition: width 0.5s linear;
    }

    .transport-controls {
        margin-top: 0.25rem;
    }
</style>
