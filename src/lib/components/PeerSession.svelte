<script lang="ts">
    import { createEventDispatcher } from 'svelte';
    import NowPlaying from './NowPlaying.svelte';
    import Queue from './Queue.svelte';
    import PeerList from './PeerList.svelte';

    const dispatch = createEventDispatcher<{ disconnected: void }>();
</script>

<div class="peer-session flex-col fade-in">
    <header class="session-header flex items-center justify-between p-4">
        <h3>🎵 Listening</h3>
        <button class="btn btn-secondary" on:click={() => dispatch('disconnected')}>
            Leave Session
        </button>
    </header>

    <div class="session-body flex flex-1 overflow-y-auto">
        <div class="main-area flex-col flex-1 gap-4 p-4">
            <NowPlaying />
            <Queue />
        </div>
        <aside class="sidebar flex-col gap-4 p-4">
            <PeerList />
        </aside>
    </div>
</div>

<style>
    .peer-session {
        height: 100vh;
    }

    .session-header {
        border-bottom: 1px solid var(--border-subtle);
    }

    .session-body {
        min-height: 0;
    }

    .main-area {
        min-width: 0;
    }

    .sidebar {
        width: 280px;
        flex-shrink: 0;
        border-left: 1px solid var(--border-subtle);
        overflow-y: auto;
    }
</style>
