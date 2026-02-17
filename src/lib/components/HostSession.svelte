<script lang="ts">
    import { createEventDispatcher } from 'svelte';
    import NowPlaying from './NowPlaying.svelte';
    import Queue from './Queue.svelte';
    import PeerList from './PeerList.svelte';
    import SongRequests from './SongRequests.svelte';

    const dispatch = createEventDispatcher<{ ended: void }>();
</script>

<div class="host-session flex-col fade-in">
    <header class="session-header flex items-center justify-between p-4">
        <h3>🎵 Your Jam</h3>
        <button class="btn btn-danger" on:click={() => dispatch('ended')}>
            End Session
        </button>
    </header>

    <div class="session-body flex flex-1 overflow-y-auto">
        <div class="main-area flex-col flex-1 gap-4 p-4">
            <NowPlaying />
            <Queue />
        </div>
        <aside class="sidebar flex-col gap-4 p-4">
            <PeerList />
            <SongRequests />
        </aside>
    </div>
</div>

<style>
    .host-session {
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
