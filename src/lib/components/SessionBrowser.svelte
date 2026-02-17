<script lang="ts">
    import { createEventDispatcher } from 'svelte';
    import { discoveredSessions } from '../stores/discovery';

    const dispatch = createEventDispatcher<{ joined: void; back: void }>();
</script>

<div class="browse-screen flex-col fade-in">
    <div class="browse-header flex items-center gap-3 p-4">
        <button class="btn-icon" on:click={() => dispatch('back')} aria-label="Go back">
            ←
        </button>
        <h2>Join a Jam</h2>
    </div>

    <div class="browse-content flex-col gap-3 p-4 overflow-y-auto flex-1">
        {#if $discoveredSessions.length === 0}
            <div class="empty-state flex-center flex-col gap-2 p-6">
                <p class="text-secondary">Searching for sessions on your network…</p>
                <p class="text-xs text-secondary">Make sure you're on the same Wi-Fi</p>
            </div>
        {:else}
            {#each $discoveredSessions as session}
                <button
                    class="session-card card flex items-center justify-between w-full"
                    on:click={() => dispatch('joined')}
                >
                    <div class="flex-col gap-1">
                        <span class="font-medium">{session.session_name}</span>
                        <span class="text-sm text-secondary">Hosted by {session.host_name}</span>
                    </div>
                    <span class="text-sm text-secondary">
                        {session.peer_count}/{session.max_peers} peers
                    </span>
                </button>
            {/each}
        {/if}
    </div>
</div>

<style>
    .browse-screen {
        height: 100vh;
    }

    .empty-state {
        margin-top: 4rem;
    }

    .session-card {
        cursor: pointer;
        text-align: left;
        border: 1px solid var(--border-subtle);
        transition: background var(--transition-fast);
    }

    .session-card:hover {
        background: var(--bg-elevated);
    }
</style>
