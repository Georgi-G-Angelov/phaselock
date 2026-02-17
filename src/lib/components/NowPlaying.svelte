<script lang="ts">
    import { playbackStore } from '../stores/playback';
</script>

<div class="now-playing card flex-col gap-3">
    <h4>Now Playing</h4>

    {#if $playbackStore.state === 'stopped' && !$playbackStore.file_name}
        <p class="text-secondary text-sm">No track playing</p>
    {:else}
        <div class="track-info flex-col gap-1">
            <span class="text-ellipsis">{$playbackStore.file_name || 'Unknown'}</span>
            <span class="text-sm text-secondary">{$playbackStore.state}</span>
        </div>

        <div class="progress-bar">
            <div
                class="progress-fill"
                style:width="{$playbackStore.duration_ms > 0
                    ? ($playbackStore.position_ms / $playbackStore.duration_ms) * 100
                    : 0}%"
            />
        </div>

        <div class="transport-controls flex-center gap-3">
            <button class="btn-icon" aria-label="Previous">⏮</button>
            <button class="btn-icon large" aria-label="Play/Pause">
                {$playbackStore.state === 'playing' ? '⏸' : '▶'}
            </button>
            <button class="btn-icon" aria-label="Next">⏭</button>
        </div>
    {/if}
</div>

<style>
    .progress-bar {
        width: 100%;
        height: 4px;
        background: var(--bg-elevated);
        border-radius: 2px;
        overflow: hidden;
    }

    .progress-fill {
        height: 100%;
        background: var(--accent-green);
        border-radius: 2px;
        transition: width 0.5s linear;
    }
</style>
