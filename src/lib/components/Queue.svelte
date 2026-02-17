<script lang="ts">
    import { queueStore } from '../stores/queue';
</script>

<div class="queue-panel card flex-col gap-3">
    <div class="flex items-center justify-between">
        <h4>Queue</h4>
        <span class="text-sm text-secondary">{$queueStore.length} tracks</span>
    </div>

    {#if $queueStore.length === 0}
        <p class="text-secondary text-sm">Queue is empty — add some songs!</p>
    {:else}
        <div class="queue-list flex-col gap-1 overflow-y-auto">
            {#each $queueStore as item, i}
                <div class="queue-item flex items-center gap-3 p-2" class:is-playing={item.status === 'Playing'}>
                    <span class="queue-index text-xs text-secondary">{i + 1}</span>
                    <div class="flex-col flex-1" style="min-width: 0;">
                        <span class="text-ellipsis text-sm">{item.file_name}</span>
                        <span class="text-xs text-secondary">{Math.floor(item.duration_secs / 60)}:{String(Math.floor(item.duration_secs % 60)).padStart(2, '0')}</span>
                    </div>
                    <span class="text-xs text-secondary">{item.status}</span>
                </div>
            {/each}
        </div>
    {/if}
</div>

<style>
    .queue-list {
        max-height: 300px;
    }

    .queue-item {
        border-radius: var(--radius-sm);
        transition: background var(--transition-fast);
    }

    .queue-item:hover {
        background: var(--bg-elevated);
    }

    .queue-item.is-playing {
        background: var(--bg-elevated);
    }

    .queue-item.is-playing .text-ellipsis {
        color: var(--accent-green);
    }

    .queue-index {
        width: 1.25rem;
        text-align: center;
        flex-shrink: 0;
    }
</style>
