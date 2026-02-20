<script lang="ts">
    import { invoke } from '@tauri-apps/api/core';
    import { songRequests } from '../stores/requests';

    /** Map of request_id → upload progress (0–100). */
    let uploadProgress: Record<string, number> = {};

    /** Set of request IDs currently being processed (to disable buttons). */
    let processing = new Set<string>();

    function formatFileSize(bytes: number): string {
        if (bytes < 1024) return `${bytes} B`;
        if (bytes < 1024 * 1024) return `${(bytes / 1024).toFixed(1)} KB`;
        return `${(bytes / (1024 * 1024)).toFixed(1)} MB`;
    }

    async function acceptRequest(requestId: string) {
        processing.add(requestId);
        processing = processing; // trigger reactivity
        try {
            await invoke('accept_song_request', { requestId });
            songRequests.update(r => r.filter(req => req.request_id !== requestId));
        } catch (e) {
            console.error('Failed to accept song request:', e);
        } finally {
            processing.delete(requestId);
            processing = processing;
        }
    }

    async function rejectRequest(requestId: string) {
        processing.add(requestId);
        processing = processing;
        try {
            await invoke('reject_song_request', { requestId });
            songRequests.update(r => r.filter(req => req.request_id !== requestId));
        } catch (e) {
            console.error('Failed to reject song request:', e);
        } finally {
            processing.delete(requestId);
            processing = processing;
        }
    }

    /** Called from parent (HostSession) when upload progress events fire. */
    export function setUploadProgress(requestId: string, percent: number) {
        uploadProgress[requestId] = percent;
        uploadProgress = uploadProgress;
    }

    /** Called from parent when a transfer completes or fails. */
    export function clearUploadProgress(requestId: string) {
        delete uploadProgress[requestId];
        uploadProgress = uploadProgress;
    }
</script>

<div class="requests-panel panel flex-col gap-3">
    <div class="flex items-center justify-between">
        <h4>Song Requests</h4>
        {#if $songRequests.length > 0}
            <span class="badge">{$songRequests.length}</span>
        {/if}
    </div>

    {#if $songRequests.length === 0}
        <p class="text-secondary text-sm no-requests">No pending requests</p>
    {:else}
        <div class="requests-list flex-col gap-2">
            {#each $songRequests as request (request.request_id)}
                <div class="request-card card-elevated flex-col gap-2">
                    <div class="flex-col gap-1">
                        <span class="text-sm text-ellipsis">{request.file_name}</span>
                        <div class="flex items-center gap-2">
                            <span class="text-xs text-secondary">from {request.peer_name}</span>
                            {#if request.file_size}
                                <span class="text-xs text-secondary">· {formatFileSize(request.file_size)}</span>
                            {/if}
                        </div>
                    </div>

                    <!-- Upload progress bar (if accepting / transferring) -->
                    {#if uploadProgress[request.request_id] !== undefined}
                        <div class="progress-container">
                            <div class="progress-bar" style="width: {uploadProgress[request.request_id]}%"></div>
                        </div>
                        <span class="text-xs text-secondary">{uploadProgress[request.request_id].toFixed(0)}% transferred</span>
                    {:else}
                        <div class="flex gap-2">
                            <button
                                class="btn btn-primary btn-sm flex-1"
                                disabled={processing.has(request.request_id)}
                                on:click={() => acceptRequest(request.request_id)}
                            >Accept</button>
                            <button
                                class="btn btn-danger btn-sm flex-1"
                                disabled={processing.has(request.request_id)}
                                on:click={() => rejectRequest(request.request_id)}
                            >Reject</button>
                        </div>
                    {/if}
                </div>
            {/each}
        </div>
    {/if}
</div>

<style>
    .btn-sm {
        padding: 0.375rem 0.75rem;
        font-size: 0.75rem;
    }

    .badge {
        background: var(--accent-green);
        color: var(--bg-primary);
        font-size: 0.625rem;
        font-weight: 700;
        padding: 0.125rem 0.5rem;
        border-radius: 999px;
        min-width: 1.25rem;
        text-align: center;
    }

    .no-requests {
        padding: 0.5rem 0;
    }

    .request-card {
        padding: 0.75rem;
    }

    .progress-container {
        width: 100%;
        height: 4px;
        background: var(--bg-primary);
        border-radius: 2px;
        overflow: hidden;
    }

    .progress-bar {
        height: 100%;
        background: var(--accent-green);
        border-radius: 2px;
        transition: width 0.3s ease;
    }
</style>
