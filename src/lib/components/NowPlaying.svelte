<script lang="ts">
    import { invoke } from '@tauri-apps/api/core';
    import { playbackStore } from '../stores/playback';
    import { queueStore } from '../stores/queue';

    /** Whether to show host transport controls. */
    export let hostControls: boolean = false;

    $: hasTrack = $playbackStore.file_name !== '';
    $: isPlaying = $playbackStore.state === 'playing';
    $: isPaused = $playbackStore.state === 'paused';
    $: progress = $playbackStore.duration_ms > 0
        ? ($playbackStore.position_ms / $playbackStore.duration_ms) * 100
        : 0;
    $: anyTransferring = $queueStore.some(q => q.status === 'Transferring');

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
        if (isPlaying) {
            await invoke('pause');
        } else {
            await invoke('play');
        }
    }
</script>

<div class="now-playing card flex-col gap-4">
    <h4>Now Playing</h4>

    {#if !hasTrack && $playbackStore.state === 'stopped'}
        <p class="text-secondary text-sm">{hostControls ? 'No track playing' : 'Waiting for host to play a track...'}</p>
    {:else}
        <div class="track-info flex-col gap-1">
            <span class="track-name text-ellipsis">{$playbackStore.file_name || 'Unknown'}</span>
        </div>

        <!-- Progress bar -->
        <div class="progress-section flex-col gap-1">
            <button
                class="progress-bar"
                class:seekable={hostControls && hasTrack}
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

        <!-- Transport controls (host only) -->
        {#if hostControls}
            <div class="transport-controls flex-center gap-3">
                <button
                    class="btn-icon"
                    aria-label="Stop"
                    on:click={() => invoke('stop')}
                    disabled={!hasTrack}
                >⏹</button>

                <button
                    class="btn-icon large"
                    aria-label={isPlaying ? 'Pause' : 'Play'}
                    on:click={togglePlay}
                    disabled={!hasTrack || anyTransferring}
                    title={anyTransferring ? 'Waiting for all peers to receive the file' : ''}
                >
                    {isPlaying ? '⏸' : '▶'}
                </button>

                <button
                    class="btn-icon"
                    aria-label="Skip"
                    on:click={() => invoke('skip')}
                    disabled={!hasTrack}
                >⏭</button>
            </div>
        {/if}
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
