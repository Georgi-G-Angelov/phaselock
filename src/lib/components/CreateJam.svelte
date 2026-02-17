<script lang="ts">
    import { createEventDispatcher } from 'svelte';

    const dispatch = createEventDispatcher<{ created: void; back: void }>();

    let sessionName = '';
    let displayName = '';
</script>

<div class="create-screen flex-center fade-in">
    <div class="create-content flex-col gap-6">
        <div class="flex items-center gap-3">
            <button class="btn-icon" on:click={() => dispatch('back')} aria-label="Go back">
                ←
            </button>
            <h2>Create a Jam</h2>
        </div>

        <div class="card flex-col gap-4">
            <div class="input-group">
                <label class="input-label" for="session-name">Session Name</label>
                <input
                    id="session-name"
                    class="input"
                    type="text"
                    placeholder="My awesome jam"
                    bind:value={sessionName}
                />
            </div>

            <div class="input-group">
                <label class="input-label" for="display-name">Your Name</label>
                <input
                    id="display-name"
                    class="input"
                    type="text"
                    placeholder="DJ Awesome"
                    bind:value={displayName}
                />
            </div>

            <button
                class="btn btn-primary w-full"
                disabled={!sessionName.trim() || !displayName.trim()}
                on:click={() => dispatch('created')}
            >
                Start Session
            </button>
        </div>
    </div>
</div>

<style>
    .create-screen {
        height: 100vh;
    }

    .create-content {
        max-width: 400px;
        width: 100%;
        padding: 2rem;
    }
</style>
