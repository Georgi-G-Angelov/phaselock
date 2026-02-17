<script lang="ts">
    import { createEventDispatcher } from 'svelte';
    import { invoke } from '@tauri-apps/api/core';
    import { sessionStore, isHost } from '../stores/session';
    import type { SessionInfo } from '../types';
    import Spinner from './Spinner.svelte';
    import Toast from './Toast.svelte';

    const dispatch = createEventDispatcher<{ created: void; back: void }>();

    let sessionName = '';
    let displayName = '';
    let loading = false;
    let toast: Toast;

    $: sessionNameTrimmed = sessionName.trim();
    $: displayNameTrimmed = displayName.trim();
    $: sessionNameError = sessionNameTrimmed.length > 50 ? 'Max 50 characters' : '';
    $: displayNameError = displayNameTrimmed.length > 30 ? 'Max 30 characters' : '';
    $: canSubmit = sessionNameTrimmed.length > 0 && displayNameTrimmed.length > 0
        && !sessionNameError && !displayNameError && !loading;

    async function handleCreate() {
        if (!canSubmit) return;
        loading = true;
        try {
            const info = await invoke<SessionInfo>('create_session', {
                sessionName: sessionName.trim(),
                displayName: displayName.trim(),
            });
            sessionStore.set(info);
            isHost.set(true);
            dispatch('created');
        } catch (e) {
            toast?.show(String(e), 'error');
        } finally {
            loading = false;
        }
    }

    function handleKeydown(e: KeyboardEvent) {
        if (e.key === 'Enter' && canSubmit) handleCreate();
    }
</script>

<Toast bind:this={toast} />

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
                    placeholder="e.g., Friday Night Vibes"
                    bind:value={sessionName}
                    on:keydown={handleKeydown}
                    disabled={loading}
                    maxlength="50"
                />
                {#if sessionNameError}
                    <span class="text-xs" style="color: var(--error-red)">{sessionNameError}</span>
                {:else if sessionNameTrimmed.length > 0}
                    <span class="text-xs text-secondary">{sessionNameTrimmed.length}/50</span>
                {/if}
            </div>

            <div class="input-group">
                <label class="input-label" for="display-name">Your Name</label>
                <input
                    id="display-name"
                    class="input"
                    type="text"
                    placeholder="e.g., Alex"
                    bind:value={displayName}
                    on:keydown={handleKeydown}
                    disabled={loading}
                    maxlength="30"
                />
                {#if displayNameError}
                    <span class="text-xs" style="color: var(--error-red)">{displayNameError}</span>
                {:else if displayNameTrimmed.length > 0}
                    <span class="text-xs text-secondary">{displayNameTrimmed.length}/30</span>
                {/if}
            </div>

            <button
                class="btn btn-primary w-full"
                disabled={!canSubmit}
                on:click={handleCreate}
            >
                {#if loading}
                    <Spinner size={18} />
                    Creating…
                {:else}
                    Create
                {/if}
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
