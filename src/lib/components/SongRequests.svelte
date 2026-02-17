<script lang="ts">
    import { songRequests } from '../stores/requests';
</script>

<div class="requests-panel panel flex-col gap-3">
    <div class="flex items-center justify-between">
        <h4>Song Requests</h4>
        <span class="text-sm text-secondary">{$songRequests.length}</span>
    </div>

    {#if $songRequests.length === 0}
        <p class="text-secondary text-sm">No pending requests</p>
    {:else}
        <div class="requests-list flex-col gap-2">
            {#each $songRequests as request}
                <div class="request-card card-elevated flex-col gap-2">
                    <div class="flex-col gap-1">
                        <span class="text-sm text-ellipsis">{request.file_name}</span>
                        <span class="text-xs text-secondary">from {request.peer_name}</span>
                    </div>
                    <div class="flex gap-2">
                        <button class="btn btn-primary btn-sm flex-1">Accept</button>
                        <button class="btn btn-danger btn-sm flex-1">Reject</button>
                    </div>
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
</style>
