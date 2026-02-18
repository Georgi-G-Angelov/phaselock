<script lang="ts">
    import { createEventDispatcher, onMount, onDestroy } from 'svelte';
    import { invoke } from '@tauri-apps/api/core';
    import { listen } from '@tauri-apps/api/event';
    import type { UnlistenFn } from '@tauri-apps/api/event';
    import { discoveredSessions } from '../stores/discovery';
    import { sessionStore, isHost } from '../stores/session';
    import { queueStore } from '../stores/queue';
    import type { DiscoveredSession, SessionInfo } from '../types';
    import { EVENTS } from '../types';
    import Spinner from './Spinner.svelte';
    import Toast from './Toast.svelte';

    const dispatch = createEventDispatcher<{ joined: void; back: void }>();

    let displayName = '';
    let manualAddress = '';
    let loading = false;
    let loadingAddress = '';
    let toast: Toast;

    // IP:port validation regex
    const IP_PORT_REGEX = /^(\d{1,3}\.\d{1,3}\.\d{1,3}\.\d{1,3}):(\d{1,5})$/;
    $: manualAddressValid = manualAddress.trim() === '' || IP_PORT_REGEX.test(manualAddress.trim());
    $: displayNameTrimmed = displayName.trim();
    $: displayNameError = displayNameTrimmed.length > 30 ? 'Max 30 characters' : '';

    let pollTimer: ReturnType<typeof setInterval> | null = null;
    let unlistenDiscovery: UnlistenFn | null = null;

    onMount(async () => {
        // Initial fetch
        await refreshSessions();

        // Poll every 3 seconds
        pollTimer = setInterval(refreshSessions, 3000);

        // Live updates via Tauri events
        unlistenDiscovery = await listen<{ sessions: DiscoveredSession[] }>(
            EVENTS.DISCOVERY_SESSIONS_UPDATED,
            (event) => {
                discoveredSessions.set(event.payload.sessions);
            }
        );
    });

    onDestroy(() => {
        if (pollTimer) clearInterval(pollTimer);
        if (unlistenDiscovery) unlistenDiscovery();
    });

    async function refreshSessions() {
        try {
            const sessions = await invoke<DiscoveredSession[]>('get_discovered_sessions');
            discoveredSessions.set(sessions);
        } catch {
            // Silently ignore — we'll keep retrying.
        }
    }

    async function joinSession(address: string) {
        if (!displayName.trim()) {
            toast?.show('Please enter your name first.', 'error');
            return;
        }
        loading = true;
        loadingAddress = address;
        try {
            const info = await invoke<SessionInfo>('join_session', {
                address,
                displayName: displayName.trim(),
            });
            console.log('[PhaseLock] join_session response:', JSON.stringify(info));
            sessionStore.set(info);
            isHost.set(false);
            if (info.initial_queue && info.initial_queue.length > 0) {
                console.log('[PhaseLock] Setting initial queue with', info.initial_queue.length, 'items');
                queueStore.set(info.initial_queue);
            } else {
                console.log('[PhaseLock] initial_queue is empty or missing:', info.initial_queue);
            }
            dispatch('joined');
        } catch (e) {
            toast?.show(String(e), 'error');
        } finally {
            loading = false;
            loadingAddress = '';
        }
    }

    function handleManualConnect() {
        const addr = manualAddress.trim();
        if (!addr) return;
        if (!IP_PORT_REGEX.test(addr)) {
            toast?.show('Invalid format. Use IP:port (e.g., 192.168.1.42:17401)', 'error');
            return;
        }
        joinSession(addr);
    }

    function handleManualKeydown(e: KeyboardEvent) {
        if (e.key === 'Enter') handleManualConnect();
    }
</script>

<Toast bind:this={toast} />

<div class="browse-screen flex-col fade-in">
    <div class="browse-header flex items-center gap-3 p-4">
        <button class="btn-icon" on:click={() => dispatch('back')} aria-label="Go back" disabled={loading}>
            ←
        </button>
        <h2>Join a Jam</h2>
    </div>

    <div class="browse-body flex-col gap-4 p-4 overflow-y-auto flex-1">
        <!-- Display name input -->
        <div class="input-group">
            <label class="input-label" for="join-display-name">Your Name</label>
            <input
                id="join-display-name"
                class="input"
                type="text"
                placeholder="e.g., Alex"
                bind:value={displayName}
                disabled={loading}
                maxlength="30"
            />
            {#if displayNameError}
                <span class="text-xs" style="color: var(--error-red)">{displayNameError}</span>
            {:else if displayNameTrimmed.length > 0}
                <span class="text-xs text-secondary">{displayNameTrimmed.length}/30</span>
            {/if}
        </div>

        <!-- Discovered sessions -->
        <div class="flex-col gap-2">
            <h4 class="text-secondary text-sm">Sessions on your network</h4>

            {#if $discoveredSessions.length === 0}
                <div class="empty-state flex-center flex-col gap-2 p-6 card">
                    <Spinner size={20} />
                    <p class="text-secondary text-sm">Searching for sessions…</p>
                    <p class="text-xs text-secondary">Make sure you're on the same Wi-Fi</p>
                </div>
            {:else}
                {#each $discoveredSessions as session}
                    <button
                        class="session-card card flex items-center justify-between w-full"
                        on:click={() => joinSession(session.address)}
                        disabled={loading}
                    >
                        <div class="flex-col gap-1">
                            <div class="flex items-center gap-2">
                                <span class="status-dot"></span>
                                <span class="session-name">{session.session_name}</span>
                            </div>
                            <span class="text-sm text-secondary">
                                hosted by {session.host_name} — {session.peer_count}/{session.max_peers} peers
                            </span>
                        </div>
                        {#if loading && loadingAddress === session.address}
                            <Spinner size={18} />
                        {/if}
                    </button>
                {/each}
            {/if}
        </div>

        <!-- Manual connection -->
        <div class="divider flex items-center gap-3">
            <hr class="flex-1" />
            <span class="text-xs text-secondary">or connect manually</span>
            <hr class="flex-1" />
        </div>

        <div class="flex gap-2">
            <input
                class="input flex-1"
                class:input-error={manualAddress.trim() !== '' && !manualAddressValid}
                type="text"
                placeholder="192.168.1.42:17401"
                bind:value={manualAddress}
                on:keydown={handleManualKeydown}
                disabled={loading}
            />
            <button
                class="btn btn-secondary"
                on:click={handleManualConnect}
                disabled={loading || !manualAddress.trim() || !manualAddressValid || !!displayNameError}
            >
                {#if loading && loadingAddress === manualAddress.trim()}
                    <Spinner size={16} />
                {:else}
                    Connect
                {/if}
            </button>
        </div>
    </div>
</div>

<style>
    .browse-screen {
        height: 100vh;
    }

    .browse-body {
        max-width: 520px;
        margin: 0 auto;
        width: 100%;
    }

    .empty-state {
        margin-top: 1rem;
    }

    .session-card {
        cursor: pointer;
        text-align: left;
        border: 1px solid var(--border-subtle);
        transition: background var(--transition-fast);
    }

    .session-card:hover:not(:disabled) {
        background: var(--bg-elevated);
    }

    .session-card:disabled {
        opacity: 0.6;
        cursor: not-allowed;
    }

    .session-name {
        font-weight: 600;
    }

    .status-dot {
        width: 8px;
        height: 8px;
        border-radius: 50%;
        background: var(--accent-green);
        flex-shrink: 0;
    }

    .divider {
        margin: 0.5rem 0;
    }

    .divider hr {
        border: none;
        border-top: 1px solid var(--border-subtle);
    }
</style>
