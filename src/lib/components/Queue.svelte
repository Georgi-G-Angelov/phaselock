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

    /** Whether yt-dlp is ready (blocks YouTube / Spotify). */
    export let ytdlpReady: boolean = true;

    // Filter out played songs but remember each item's real backend index.
    type VisibleItem = { item: QueueItem; realIndex: number };
    $: visibleQueue = $queueStore
        .map((item, i) => ({ item, realIndex: i }))
        .filter(({ item }) => item.status !== 'Played') as VisibleItem[];

    const dispatch = createEventDispatcher<{ 'toast-message': { message: string; variant: 'success' | 'error' | 'info' } }>();

    let requestPending = false;
    let requestStatus = '';
    let showRequestMenu = false;
    let showRequestYtInput = false;
    let showRequestSearchInput = false;
    let showRequestSpotifyInput = false;
    let requestYtUrl = '';
    let requestSearchQuery = '';
    let requestSpotifyUrl = '';

    interface ResolvedMeta { display_name: string; content: string; }

    /** Resolve metadata, then send the request. */
    async function resolveAndRequest(kind: string, content: string) {
        requestPending = true;
        requestStatus = 'Resolving…';
        try {
            const meta = await invoke<ResolvedMeta>('resolve_song_meta', { kind, content });
            requestStatus = 'Sending…';
            await invoke('request_song', { kind, content: meta.content, displayName: meta.display_name });
            dispatch('toast-message', { message: `Requested: ${meta.display_name}`, variant: 'success' });
            return true;
        } catch (e) {
            dispatch('toast-message', { message: `Request failed: ${e}`, variant: 'error' });
            return false;
        } finally {
            requestPending = false;
            requestStatus = '';
        }
    }

    function toggleRequestMenu() {
        showRequestMenu = !showRequestMenu;
    }

    async function requestFile() {
        showRequestMenu = false;
        const selected = await open({
            multiple: true,
            filters: [{ name: 'Audio', extensions: ['mp3'] }],
        });
        if (selected) {
            const paths = Array.isArray(selected) ? selected : [selected];
            requestPending = true;
            try {
                for (const filePath of paths) {
                    const meta = await invoke<ResolvedMeta>('resolve_song_meta', { kind: 'file', content: filePath });
                    await invoke('request_song', { kind: 'file', content: filePath, displayName: meta.display_name });
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

    async function requestYoutube() {
        const url = requestYtUrl.trim();
        if (!url) return;
        const ok = await resolveAndRequest('youtube_url', url);
        if (ok) { showRequestYtInput = false; requestYtUrl = ''; }
    }

    async function requestSearch() {
        const query = requestSearchQuery.trim();
        if (!query) return;
        const ok = await resolveAndRequest('youtube_search', query);
        if (ok) { showRequestSearchInput = false; requestSearchQuery = ''; }
    }

    async function requestSpotify() {
        const url = requestSpotifyUrl.trim();
        if (!url) return;
        const ok = await resolveAndRequest('spotify_track', url);
        if (ok) { showRequestSpotifyInput = false; requestSpotifyUrl = ''; }
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
        e.stopPropagation();
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

    /** Drop on the container (empty space below items) → move to end of visible list. */
    async function handleContainerDrop(e: DragEvent) {
        e.preventDefault();
        if (dragRealIndex === null || visibleQueue.length === 0) {
            handleDragEnd();
            return;
        }
        const last = visibleQueue[visibleQueue.length - 1];
        if (dragRealIndex !== last.realIndex) {
            try {
                await invoke('reorder_queue', { fromIndex: dragRealIndex, toIndex: last.realIndex });
            } catch (err) {
                console.error('Failed to reorder queue:', err);
            }
        }
        handleDragEnd();
    }

    async function handleDoubleClick(trackId: string) {
        if (!editable) return;
        try {
            await invoke('play_track', { trackId });
        } catch (e) {
            console.error('Failed to play track:', e);
        }
    }

    function formatDuration(secs: number): string {
        const m = Math.floor(secs / 60);
        const s = Math.floor(secs % 60);
        return `${m}:${String(s).padStart(2, '0')}`;
    }

    async function shuffleQueue() {
        try {
            await invoke('shuffle_queue');
        } catch (e) {
            console.error('Failed to shuffle queue:', e);
        }
    }

    // ── Add Song source picker ──────────────────────────────────────────
    // ── Right-click context menu ─────────────────────────────────────
    let ctxMenu = { show: false, x: 0, y: 0, trackId: '', realIndex: -1 };

    function openCtxMenu(e: MouseEvent, trackId: string, realIndex: number) {
        e.preventDefault();
        ctxMenu = { show: true, x: e.clientX, y: e.clientY, trackId, realIndex };
    }

    function closeCtxMenu() {
        ctxMenu = { ...ctxMenu, show: false };
    }

    async function playNext() {
        const fromIndex = ctxMenu.realIndex;
        // Find the currently-playing track's real index in the full queue.
        const playingIdx = $queueStore.findIndex(q => q.status === 'Playing');
        // Target position: right after the playing track, or top of queue.
        const toIndex = playingIdx >= 0 ? playingIdx + 1 : 0;
        closeCtxMenu();
        if (fromIndex === toIndex || fromIndex === toIndex - 1) return; // already there
        try {
            await invoke('reorder_queue', { fromIndex, toIndex });
        } catch (e) {
            console.error('Failed to move track:', e);
        }
    }

    function ctxRemove() {
        const id = ctxMenu.trackId;
        closeCtxMenu();
        removeSong(id);
    }

    let showAddMenu = false;
    let showYoutubeInput = false;
    let youtubeUrl = '';
    let youtubeLoading = false;
    let youtubeStatus = '';
    let showSpotifyInput = false;
    let spotifyUrl = '';
    let spotifyLoading = false;
    let spotifyStatus = '';

    function toggleAddMenu() {
        showAddMenu = !showAddMenu;
    }

    function pickFiles() {
        showAddMenu = false;
        addSong();
    }

    function openYoutubeInput() {
        showAddMenu = false;
        showYoutubeInput = true;
        youtubeUrl = '';
        youtubeStatus = '';
    }

    function closeYoutubeInput() {
        if (!youtubeLoading) {
            showYoutubeInput = false;
            youtubeUrl = '';
            youtubeStatus = '';
        }
    }

    function openSpotifyInput() {
        showAddMenu = false;
        showSpotifyInput = true;
        spotifyUrl = '';
        spotifyStatus = '';
    }

    function closeSpotifyInput() {
        if (!spotifyLoading) {
            showSpotifyInput = false;
            spotifyUrl = '';
            spotifyStatus = '';
        }
    }

    async function submitSpotifyUrl() {
        const url = spotifyUrl.trim();
        if (!url) return;
        spotifyLoading = true;
        spotifyStatus = 'Fetching from Spotify...';
        try {
            const count = await invoke<number>('import_spotify', { url });
            spotifyStatus = '';
            showSpotifyInput = false;
            spotifyUrl = '';
            dispatch('toast-message', {
                message: `Importing ${count} track${count !== 1 ? 's' : ''} from Spotify`,
                variant: 'success' as const,
            });
        } catch (e) {
            spotifyStatus = `Error: ${e}`;
        } finally {
            spotifyLoading = false;
        }
    }

    async function submitYoutubeUrl() {
        const url = youtubeUrl.trim();
        if (!url) return;
        youtubeLoading = true;
        youtubeStatus = 'Fetching info...';
        try {
            await invoke('enqueue_youtube', { url });
            youtubeStatus = '';
            showYoutubeInput = false;
            youtubeUrl = '';
        } catch (e) {
            youtubeStatus = `Error: ${e}`;
        } finally {
            youtubeLoading = false;
        }
    }
</script>

<div class="queue-panel card flex-col gap-3 h-full">
    <div class="flex items-center justify-between">
        <h4>Queue</h4>
        <div class="flex items-center gap-2">
            <span class="text-sm text-secondary">{visibleQueue.length} tracks</span>
            {#if editable}
                <button class="btn-icon shuffle-btn" on:click={shuffleQueue} title="Shuffle upcoming">⇄</button>
                <div class="add-menu-wrapper">
                    <button class="btn btn-primary btn-sm" on:click={toggleAddMenu}>+ Add Song</button>
                    {#if showAddMenu}
                        <div class="add-menu">
                            <button class="add-menu-item" on:click={pickFiles}>📁 Files</button>
                            <button class="add-menu-item" on:click={openYoutubeInput} disabled={!ytdlpReady}>▶ YouTube {!ytdlpReady ? '(loading…)' : ''}</button>
                            <button class="add-menu-item" on:click={openSpotifyInput} disabled={!ytdlpReady}>🎧 Spotify {!ytdlpReady ? '(loading…)' : ''}</button>
                        </div>
                    {/if}
                </div>
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
            on:drop={handleContainerDrop}
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
                    on:dblclick={() => handleDoubleClick(item.id)}
                    on:contextmenu={(e) => editable && openCtxMenu(e, item.id, realIndex)}
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
                    <div class="flex-col flex-1" style="min-width: 0; overflow: hidden;">
                        <span class="text-ellipsis track-title">{item.title}</span>
                        <span class="text-xs text-secondary text-ellipsis">
                            {item.artist} · {formatDuration(item.duration_secs)}
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
            {#if showRequestYtInput}
                <div class="request-inline flex-col gap-2">
                    <input class="yt-input" type="text" placeholder="YouTube URL..." bind:value={requestYtUrl}
                        disabled={requestPending} on:keydown={(e) => e.key === 'Enter' && requestYoutube()} />
                    <div class="flex gap-2">
                        <button class="btn btn-secondary btn-sm flex-1" on:click={() => { showRequestYtInput = false; requestYtUrl = ''; }}>Cancel</button>
                        <button class="btn btn-primary btn-sm flex-1" on:click={requestYoutube} disabled={requestPending || !requestYtUrl.trim()}>Send</button>
                    </div>
                </div>
            {:else if showRequestSearchInput}
                <div class="request-inline flex-col gap-2">
                    <input class="yt-input" type="text" placeholder="Search YouTube..." bind:value={requestSearchQuery}
                        disabled={requestPending} on:keydown={(e) => e.key === 'Enter' && requestSearch()} />
                    <div class="flex gap-2">
                        <button class="btn btn-secondary btn-sm flex-1" on:click={() => { showRequestSearchInput = false; requestSearchQuery = ''; }}>Cancel</button>
                        <button class="btn btn-primary btn-sm flex-1" on:click={requestSearch} disabled={requestPending || !requestSearchQuery.trim()}>Send</button>
                    </div>
                </div>
            {:else if showRequestSpotifyInput}
                <div class="request-inline flex-col gap-2">
                    <input class="yt-input" type="text" placeholder="Spotify track URL..." bind:value={requestSpotifyUrl}
                        disabled={requestPending} on:keydown={(e) => e.key === 'Enter' && requestSpotify()} />
                    <div class="flex gap-2">
                        <button class="btn btn-secondary btn-sm flex-1" on:click={() => { showRequestSpotifyInput = false; requestSpotifyUrl = ''; }}>Cancel</button>
                        <button class="btn btn-primary btn-sm flex-1" on:click={requestSpotify} disabled={requestPending || !requestSpotifyUrl.trim()}>Send</button>
                    </div>
                </div>
            {:else}
                <div class="add-menu-wrapper" style="width: 100%;">
                    <button
                        class="btn btn-primary"
                        style="width: 100%;"
                        on:click={toggleRequestMenu}
                        disabled={requestPending}
                    >
                        {requestPending ? (requestStatus || 'Sending…') : '🎵 Request Song'}
                    </button>
                    {#if showRequestMenu}
                        <div class="add-menu" style="bottom: 100%; top: auto; margin-bottom: 0.25rem; margin-top: 0;">
                            <button class="add-menu-item" on:click={requestFile}>📁 File</button>
                            <button class="add-menu-item" on:click={() => { showRequestMenu = false; showRequestYtInput = true; }}>▶ YouTube URL</button>
                            <button class="add-menu-item" on:click={() => { showRequestMenu = false; showRequestSearchInput = true; }}>🔍 YouTube Search</button>
                            <button class="add-menu-item" on:click={() => { showRequestMenu = false; showRequestSpotifyInput = true; }}>🎧 Spotify Track</button>
                        </div>
                    {/if}
                </div>
            {/if}
        </div>
    {/if}
</div>

<!-- Right-click context menu -->
{#if ctxMenu.show}
    <!-- svelte-ignore a11y-click-events-have-key-events -->
    <div class="ctx-backdrop" on:click={closeCtxMenu} role="presentation">
        <div
            class="ctx-menu"
            style="left: {ctxMenu.x}px; top: {ctxMenu.y}px;"
            on:click|stopPropagation
            role="menu"
            tabindex="-1"
        >
            <button class="ctx-menu-item" on:click={playNext} role="menuitem">▶ Play next</button>
            <button class="ctx-menu-item ctx-menu-danger" on:click={ctxRemove} role="menuitem">✕ Remove from queue</button>
        </div>
    </div>
{/if}

<!-- YouTube URL input overlay -->
{#if showYoutubeInput}
    <div class="yt-overlay" on:click={closeYoutubeInput} role="presentation">
        <div class="yt-modal card flex-col gap-4 p-6" on:click|stopPropagation role="presentation">
            <h3>Add from YouTube</h3>
            <p class="text-sm text-secondary">Paste a YouTube link below to download and add it to the queue.</p>
            <input
                class="yt-input"
                type="text"
                placeholder="https://www.youtube.com/watch?v=..."
                bind:value={youtubeUrl}
                disabled={youtubeLoading}
                on:keydown={(e) => e.key === 'Enter' && submitYoutubeUrl()}
            />
            {#if youtubeStatus}
                <p class="text-sm {youtubeStatus.startsWith('Error') ? 'text-error' : 'text-secondary'}">
                    {#if youtubeLoading}<Spinner size={14} />{/if}
                    {youtubeStatus}
                </p>
            {/if}
            <div class="flex gap-2 justify-end">
                <button class="btn btn-secondary" on:click={closeYoutubeInput} disabled={youtubeLoading}>Cancel</button>
                <button class="btn btn-primary" on:click={submitYoutubeUrl} disabled={youtubeLoading || !youtubeUrl.trim()}>
                    {youtubeLoading ? 'Adding...' : 'Add'}
                </button>
            </div>
        </div>
    </div>
{/if}

<!-- Spotify URL input overlay -->
{#if showSpotifyInput}
    <div class="yt-overlay" on:click={closeSpotifyInput} role="presentation">
        <div class="yt-modal card flex-col gap-4 p-6" on:click|stopPropagation role="presentation">
            <h3>Add from Spotify</h3>
            <p class="text-sm text-secondary">Paste a Spotify playlist or track link. Each song will be searched on YouTube and added to the queue.</p>
            <input
                class="yt-input"
                type="text"
                placeholder="https://open.spotify.com/playlist/... or /track/..."
                bind:value={spotifyUrl}
                disabled={spotifyLoading}
                on:keydown={(e) => e.key === 'Enter' && submitSpotifyUrl()}
            />
            {#if spotifyStatus}
                <p class="text-sm {spotifyStatus.startsWith('Error') ? 'text-error' : 'text-secondary'}">
                    {#if spotifyLoading}<Spinner size={14} />{/if}
                    {spotifyStatus}
                </p>
            {/if}
            <div class="flex gap-2 justify-end">
                <button class="btn btn-secondary" on:click={closeSpotifyInput} disabled={spotifyLoading}>Cancel</button>
                <button class="btn btn-primary" on:click={submitSpotifyUrl} disabled={spotifyLoading || !spotifyUrl.trim()}>
                    {spotifyLoading ? 'Importing...' : 'Import'}
                </button>
            </div>
        </div>
    </div>
{/if}

<style>
    /* ── Add menu dropdown ─────────────────────────────────────────── */
    .add-menu-wrapper {
        position: relative;
    }

    .add-menu {
        position: absolute;
        right: 0;
        top: 100%;
        margin-top: 0.25rem;
        background: var(--bg-elevated);
        border: 1px solid var(--border-subtle);
        border-radius: var(--radius-sm);
        overflow: hidden;
        z-index: 20;
        min-width: 130px;
        box-shadow: 0 4px 12px rgba(0, 0, 0, 0.3);
    }

    .add-menu-item {
        display: block;
        width: 100%;
        padding: 0.5rem 0.75rem;
        background: none;
        border: none;
        color: var(--text-primary);
        font-size: 0.8rem;
        text-align: left;
        cursor: pointer;
        transition: background var(--transition-fast);
    }

    .add-menu-item:hover {
        background: var(--bg-subtle);
    }

    .add-menu-item:disabled {
        opacity: 0.4;
        cursor: default;
    }

    .add-menu-item:disabled:hover {
        background: none;
    }

    /* ── YouTube overlay ───────────────────────────────────────────── */
    .yt-overlay {
        position: fixed;
        inset: 0;
        background: rgba(0, 0, 0, 0.6);
        display: flex;
        align-items: center;
        justify-content: center;
        z-index: 100;
    }

    .yt-modal {
        width: 480px;
        max-width: 90vw;
    }

    .yt-input {
        width: 100%;
        padding: 0.625rem 0.75rem;
        background: var(--bg-elevated);
        border: 1px solid var(--border-subtle);
        border-radius: var(--radius-sm);
        color: var(--text-primary);
        font-size: 0.875rem;
    }

    .yt-input:focus {
        outline: none;
        border-color: var(--accent-green);
    }

    .yt-input:disabled {
        opacity: 0.5;
    }

    .text-error {
        color: #ef4444;
    }

    /* ── Context menu ──────────────────────────────────────────────── */
    .ctx-backdrop {
        position: fixed;
        inset: 0;
        z-index: 50;
    }

    .ctx-menu {
        position: fixed;
        background: var(--bg-elevated);
        border: 1px solid var(--border-subtle);
        border-radius: var(--radius-sm);
        overflow: hidden;
        z-index: 51;
        min-width: 160px;
        box-shadow: 0 4px 12px rgba(0, 0, 0, 0.4);
    }

    .ctx-menu-item {
        display: block;
        width: 100%;
        padding: 0.5rem 0.75rem;
        background: none;
        border: none;
        color: var(--text-primary);
        font-size: 0.8rem;
        text-align: left;
        cursor: pointer;
        transition: background var(--transition-fast);
    }

    .ctx-menu-item:hover {
        background: var(--bg-subtle);
    }

    .ctx-menu-danger:hover {
        background: rgba(239, 68, 68, 0.15);
        color: #ef4444;
    }

    /* ── Existing styles ───────────────────────────────────────────── */
    .shuffle-btn {
        color: white;
        font-size: 1rem;
        padding: 0;
        background: none;
        border: none;
        cursor: pointer;
        opacity: 0.7;
        transition: opacity var(--transition-fast);
    }

    .shuffle-btn:hover {
        opacity: 1;
    }

    .btn-sm {
        padding: 0.375rem 0.75rem;
        font-size: 0.75rem;
    }

    .queue-list {
        flex: 1;
        min-height: 0;
    }

    .queue-item {
        border-radius: var(--radius-sm);
        transition: background var(--transition-fast);
        width: 100%;
    }

    .queue-item:hover {
        background: var(--bg-elevated);
    }

    .queue-item.is-playing {
        background: var(--bg-elevated);
    }

    .queue-item.is-playing .track-title {
        color: var(--accent-green);
    }

    .track-title {
        font-size: 0.875rem;
        font-weight: 500;
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
