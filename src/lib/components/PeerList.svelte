<script lang="ts">
    import { onMount, onDestroy } from 'svelte';
    import { listen } from '@tauri-apps/api/event';
    import type { UnlistenFn } from '@tauri-apps/api/event';
    import { peersStore } from '../stores/session';
    import { EVENTS } from '../types';
    import type { SyncLatency } from '../types';

    /** Map of peer_id → latency_ms, updated by sync:latency-updated events. */
    let latencies: Map<number, number> = new Map();
    let unlistenLatency: UnlistenFn | null = null;

    onMount(async () => {
        unlistenLatency = await listen<SyncLatency>(EVENTS.SYNC_LATENCY_UPDATED, (e) => {
            latencies = new Map(latencies);
            latencies.set(e.payload.peer_id, e.payload.latency_ms);
        });
    });

    onDestroy(() => {
        unlistenLatency?.();
    });

    function latencyColor(ms: number | undefined): string {
        if (ms === undefined) return 'var(--text-secondary)';
        if (ms < 10) return 'var(--accent-green)';
        if (ms <= 100) return '#f0c040';
        return 'var(--error-red)';
    }

    function latencyLabel(ms: number | undefined): string {
        if (ms === undefined) return '—';
        return `${Math.round(ms)}ms`;
    }
</script>

<div class="peer-list panel flex-col gap-3">
    <div class="flex items-center justify-between">
        <h4>Listeners</h4>
        <span class="text-sm text-secondary">{$peersStore.length}</span>
    </div>

    {#if $peersStore.length === 0}
        <p class="text-secondary text-sm">Waiting for peers to join…</p>
    {:else}
        <div class="peers flex-col gap-1">
            {#each $peersStore as peer (peer.peer_id)}
                {@const ms = latencies.get(peer.peer_id)}
                <div class="peer-item flex items-center gap-2 p-2">
                    <span
                        class="latency-dot flex-shrink-0"
                        style:background={latencyColor(ms)}
                    ></span>
                    <span class="peer-avatar flex-center flex-shrink-0">
                        {peer.display_name.charAt(0).toUpperCase()}
                    </span>
                    <span class="text-sm text-ellipsis flex-1">{peer.display_name}</span>
                    <span class="text-xs text-secondary flex-shrink-0">{latencyLabel(ms)}</span>
                </div>
            {/each}
        </div>
    {/if}
</div>

<style>
    .latency-dot {
        width: 8px;
        height: 8px;
        border-radius: 50%;
    }

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
