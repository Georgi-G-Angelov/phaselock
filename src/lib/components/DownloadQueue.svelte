<script lang="ts">
    import { onMount, onDestroy } from 'svelte';
    import { listen } from '@tauri-apps/api/event';
    import type { UnlistenFn } from '@tauri-apps/api/event';
    import { EVENTS } from '../types';
    import type { DownloadQueuePayload, DownloadTask } from '../types';
    import Spinner from './Spinner.svelte';

    let tasks: DownloadTask[] = [];
    let unlisten: UnlistenFn | null = null;

    onMount(async () => {
        unlisten = await listen<DownloadQueuePayload>(EVENTS.YOUTUBE_DOWNLOAD_QUEUE, (e) => {
            tasks = e.payload.tasks;
        });
    });

    onDestroy(() => {
        unlisten?.();
    });

    function statusIcon(status: string): string {
        if (status === 'downloading') return '⬇';
        if (status === 'failed') return '✕';
        if (status === 'queued') return '✓';
        if (status === 'fetchingMeta') return '🔍';
        if (status === 'searching') return '🔍';
        return '⏳';
    }

    function statusLabel(task: DownloadTask): string {
        if (task.status === 'searching') return 'Searching...';
        if (task.status === 'pending') return 'Queued';
        if (task.status === 'fetchingMeta') return 'Fetching info...';
        if (task.status === 'queued') return 'Waiting to download';
        if (task.status === 'downloading') return 'Downloading...';
        if (task.status === 'failed') return task.error ?? 'Failed';
        return '';
    }
</script>

{#if tasks.length > 0}
    <div class="dl-queue panel flex-col gap-2">
        <div class="flex items-center justify-between">
            <h4>Downloads</h4>
            <span class="text-sm text-secondary">{tasks.length}</span>
        </div>

        <div class="dl-list flex-col gap-1">
            {#each tasks as task (task.id)}
                <div class="dl-item flex items-center gap-2 p-2" class:is-failed={task.status === 'failed'}>
                    <span class="dl-status flex-shrink-0">
                        {#if task.status === 'downloading' || task.status === 'fetchingMeta' || task.status === 'searching'}
                            <Spinner size={14} />
                        {:else}
                            <span class="dl-icon">{statusIcon(task.status)}</span>
                        {/if}
                    </span>
                    <div class="flex-col flex-1" style="min-width: 0; overflow: hidden;">
                        <span class="text-sm text-ellipsis">{task.title}</span>
                        <span class="text-xs text-secondary text-ellipsis">
                            {#if task.title === 'Unknown'}
                                {statusLabel(task)}
                            {:else}
                                {task.artist} · {statusLabel(task)}
                            {/if}
                        </span>
                        {#if task.error}
                            <span class="text-xs dl-error text-ellipsis" title={task.error}>{task.error}</span>
                        {/if}
                    </div>
                </div>
            {/each}
        </div>
    </div>
{/if}

<style>
    .dl-queue {
        margin-top: 1rem;
    }

    .dl-list {
        max-height: 200px;
        overflow-y: auto;
    }

    .dl-item {
        border-radius: var(--radius-sm);
        transition: background var(--transition-fast);
    }

    .dl-item:hover {
        background: var(--bg-elevated);
    }

    .dl-item.is-failed {
        opacity: 0.6;
    }

    .dl-status {
        width: 1rem;
        display: flex;
        align-items: center;
        justify-content: center;
    }

    .dl-icon {
        font-size: 0.75rem;
    }

    .dl-error {
        color: #ef4444;
    }
</style>
