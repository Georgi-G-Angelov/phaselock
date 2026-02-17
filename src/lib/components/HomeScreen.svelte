<script lang="ts">
    import { createEventDispatcher } from 'svelte';

    const dispatch = createEventDispatcher<{ create: void; join: void }>();
</script>

<div class="home-screen flex-center fade-in">
    <div class="home-content flex-col items-center gap-8">
        <div class="logo-section flex-col items-center gap-2">
            <h1 class="logo-title">🎵 PhaseLock</h1>
            <p class="text-secondary">Synchronized music playback</p>
        </div>

        <div class="home-actions flex-col gap-3">
            <button class="btn btn-primary w-full" on:click={() => dispatch('create')}>
                Create a Jam
            </button>
            <button class="btn btn-secondary w-full" on:click={() => dispatch('join')}>
                Join a Jam
            </button>
        </div>
    </div>
</div>

<style>
    .home-screen {
        height: 100vh;
    }

    .home-content {
        max-width: 320px;
        width: 100%;
        padding: 2rem;
    }

    .logo-title {
        font-size: 2.5rem;
    }
</style>
