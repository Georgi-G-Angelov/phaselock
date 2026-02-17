<script lang="ts">
    import { peersStore } from '../stores/session';
</script>

<div class="peer-list panel flex-col gap-3">
    <div class="flex items-center justify-between">
        <h4>Listeners</h4>
        <span class="text-sm text-secondary">{$peersStore.length}</span>
    </div>

    {#if $peersStore.length === 0}
        <p class="text-secondary text-sm">No one else is here yet</p>
    {:else}
        <div class="peers flex-col gap-1">
            {#each $peersStore as peer}
                <div class="peer-item flex items-center gap-2 p-2">
                    <span class="peer-avatar flex-center flex-shrink-0">
                        {peer.display_name.charAt(0).toUpperCase()}
                    </span>
                    <span class="text-sm text-ellipsis">{peer.display_name}</span>
                </div>
            {/each}
        </div>
    {/if}
</div>

<style>
    .peer-avatar {
        width: 1.75rem;
        height: 1.75rem;
        border-radius: var(--radius-full);
        background: var(--bg-elevated);
        font-size: 0.75rem;
        font-weight: 600;
        color: var(--accent-green);
    }

    .peer-item {
        border-radius: var(--radius-sm);
    }
</style>
