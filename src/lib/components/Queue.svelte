<script lang="ts">
    import { invoke } from '@tauri-apps/api/core';
    import { open } from '@tauri-apps/plugin-dialog';
    import { queueStore } from '../stores/queue';
    import Spinner from './Spinner.svelte';

    import { createEventDispatcher } from 'svelte';
    import type { QueueItem } from '../types';

    /** Whether the queue is editable (host only). */
    export let editable: boolean = false;

    /** Whether to show the Request Song button (peer only). */
    export let showRequestButton: boolean = false;

    // Filter out played songs but remember each item's real backend index.
    type VisibleItem = { item: QueueItem; realIndex: number };
    $: visibleQueue = $queueStore
        .map((item, i) => ({ item, realIndex: i }))
        .filter(({ item }) => item.status !== 'Played') as VisibleItem[];

    const dispatch = createEventDispatcher<{ 'toast-message': { message: string; variant: 'success' | 'error' | 'info' } }>();

    let requestPending = false;

    async function requestSong() {
        const selected = await open({
            multiple: true,
            filters: [{ name: 'Audio', extensions: ['mp3'] }],
        });
        if (selected) {
            const paths = Array.isArray(selected) ? selected : [selected];
            requestPending = true;
            try {
                for (const filePath of paths) {
                    await invoke('request_song', { filePath });
                }
                dispatch('toast-message', {
                    message: paths.length === 1 ? 'Song request sent!' : `${paths.length} song requests sent!`,
                    variant: 'success',
                });
            } catch (e) {
                dispatch('toast-message', { message: `Request failed: ${e}`, variant: 'error' });
            } finally {
                requestPending = false;
            }
        }
    }

    let dragIndex: number | null = null;
    let dropIndex: number | null = null;
    let dragRealIndex: number | null = null;

    async function addSong() {
        const selected = await open({
            multiple: true,
            filters: [{ name: 'Audio', extensions: ['mp3'] }],
        });
        if (selected) {
            const paths = Array.isArray(selected) ? selected : [selected];
            for (const filePath of paths) {
                try {
                    await invoke('add_song', { filePath });
                } catch (e) {
                    console.error(`Failed to add song "${filePath}":`, e);
                }
            }
        }
    }

    async function removeSong(trackId: string) {
        try {
            await invoke('remove_from_queue', { trackId });
        } catch (e) {
            console.error('Failed to remove song:', e);
        }
    }

    function handleDragStart(e: DragEvent, i: number, realIdx: number) {
        dragIndex = i;
        dragRealIndex = realIdx;
        if (e.dataTransfer) {
            e.dataTransfer.effectAllowed = 'move';
            e.dataTransfer.setData('text/plain', String(i));
        }
    }

    function handleDragOver(e: DragEvent, i: number) {
        e.preventDefault();
        if (e.dataTransfer) {
            e.dataTransfer.dropEffect = 'move';
        }
        dropIndex = i;
    }

    function handleDragLeave() {
        dropIndex = null;
    }

    async function handleDrop(e: DragEvent, toIndex: number, toRealIndex: number) {
        e.preventDefault();
        if (dragRealIndex !== null && dragRealIndex !== toRealIndex) {
            try {
                await invoke('reorder_queue', { fromIndex: dragRealIndex, toIndex: toRealIndex });
            } catch (err) {
                console.error('Failed to reorder queue:', err);
            }
        }
        dragIndex = null;
        dropIndex = null;
        dragRealIndex = null;
    }

    function handleDragEnd() {
        dragIndex = null;
        dropIndex = null;
        dragRealIndex = null;
    }

    function formatDuration(secs: number): string {
        const m = Math.floor(secs / 60);
        const s = Math.floor(secs % 60);
        return `${m}:${String(s).padStart(2, '0')}`;
    }
</script>

<div class="queue-panel card flex-col gap-3 h-full">
    <div class="flex items-center justify-between">
        <h4>Queue</h4>
        <div class="flex items-center gap-2">
            <span class="text-sm text-secondary">{visibleQueue.length} tracks</span>
            {#if editable}
                <button class="btn btn-primary btn-sm" on:click={addSong}>+ Add Song</button>
            {/if}
        </div>
    </div>

    {#if visibleQueue.length === 0}
        <div class="empty-queue flex-center flex-col gap-2 p-4">
            <p class="text-secondary text-sm">Queue is empty.</p>
            {#if editable}
                <p class="text-xs text-secondary">Add a song to get started!</p>
            {/if}
        </div>
    {:else}
        <div
            class="queue-list flex-col gap-1 overflow-y-auto"
            role="list"
            on:dragover|preventDefault
            on:drop|preventDefault
        >
            {#each visibleQueue as { item, realIndex }, i (item.id)}
                <div
                    class="queue-item flex items-center gap-2 p-2"
                    class:is-playing={item.status === 'Playing'}
                    class:is-dragging={dragIndex === i}
                    class:drop-target={dropIndex === i}
                    draggable={editable ? 'true' : 'false'}
                    on:dragstart={(e) => handleDragStart(e, i, realIndex)}
                    on:dragover={(e) => handleDragOver(e, i)}
                    on:dragleave={handleDragLeave}
                    on:drop={(e) => handleDrop(e, i, realIndex)}
                    on:dragend={handleDragEnd}
                    role={editable ? 'listitem' : undefined}
                >
                    <!-- Drag handle -->
                    {#if editable}
                        <span class="drag-handle text-xs text-secondary" aria-label="Drag to reorder">⠿</span>
                    {/if}

                    <!-- Status indicator -->
                    <span class="status-indicator flex-shrink-0">
                        {#if item.status === 'Transferring'}
                            <Spinner size={14} />
                        {:else if item.status === 'Ready'}
                            <span class="status-ready">✓</span>
                        {:else if item.status === 'Playing'}
                            <span class="status-playing">♪</span>
                        {/if}
                    </span>

                    <!-- Track info -->
                    <div class="flex-col flex-1" style="min-width: 0;">
                        <span class="text-ellipsis text-sm">{item.file_name}</span>
                        <span class="text-xs text-secondary">
                            {formatDuration(item.duration_secs)} · {item.added_by}
                        </span>
                    </div>

                    <!-- Remove button -->
                    {#if editable}
                        <button
                            class="btn-icon remove-btn"
                            on:click|stopPropagation={() => removeSong(item.id)}
                            aria-label="Remove"
                        >✕</button>
                    {/if}
                </div>
            {/each}
        </div>
    {/if}

    {#if showRequestButton}
        <div class="request-section" style="margin-top: auto; padding-top: 0.75rem; border-top: 1px solid var(--border-subtle);">
            <button
                class="btn btn-primary"
                style="width: 100%;"
                on:click={requestSong}
                disabled={requestPending}
            >
                {requestPending ? 'Request pending...' : '🎵 Request Song'}
            </button>
        </div>
    {/if}
</div>

<style>
    .btn-sm {
        padding: 0.375rem 0.75rem;
        font-size: 0.75rem;
    }

    .queue-list {
        max-height: calc(100vh - 280px);
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

    .queue-item.is-dragging {
        opacity: 0.4;
    }

    .queue-item.drop-target {
        border-top: 2px solid var(--accent-green);
    }

    .drag-handle {
        cursor: grab;
        opacity: 0.4;
        user-select: none;
    }

    .drag-handle:hover {
        opacity: 1;
    }

    .status-indicator {
        width: 1rem;
        display: flex;
        align-items: center;
        justify-content: center;
    }

    .status-ready {
        color: var(--accent-green);
        font-size: 0.75rem;
        font-weight: 700;
    }

    .status-playing {
        color: var(--accent-green);
        font-size: 0.875rem;
        animation: pulse 1s ease-in-out infinite;
    }

    .remove-btn {
        width: 1.5rem;
        height: 1.5rem;
        font-size: 0.625rem;
        opacity: 0;
        transition: opacity var(--transition-fast);
    }

    .queue-item:hover .remove-btn {
        opacity: 1;
    }

    .empty-queue {
        margin-top: 1rem;
    }

    @keyframes pulse {
        0%, 100% { opacity: 1; }
        50% { opacity: 0.4; }
    }
</style>
