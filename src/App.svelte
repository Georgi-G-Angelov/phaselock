<script lang="ts">
    import { onMount, onDestroy } from 'svelte';
    import { listen } from '@tauri-apps/api/event';
    import type { UnlistenFn } from '@tauri-apps/api/event';
    import HomeScreen from './lib/components/HomeScreen.svelte';
    import CreateJam from './lib/components/CreateJam.svelte';
    import SessionBrowser from './lib/components/SessionBrowser.svelte';
    import HostSession from './lib/components/HostSession.svelte';
    import PeerSession from './lib/components/PeerSession.svelte';
    import Toast from './lib/components/Toast.svelte';
    import { EVENTS, type ErrorEvent } from './lib/types';

    type Screen = 'home' | 'create' | 'browse' | 'host-session' | 'peer-session';
    let currentScreen: Screen = 'home';
    let globalToast: Toast;
    let unlistenError: UnlistenFn | null = null;

    onMount(async () => {
        // Global error boundary: listen for error:general events from Rust backend
        unlistenError = await listen<ErrorEvent>(EVENTS.ERROR_GENERAL, (e) => {
            globalToast?.show(e.payload.message, 'error');
        });

        // Catch unhandled promise rejections
        window.addEventListener('unhandledrejection', handleUnhandledRejection);
    });

    onDestroy(() => {
        if (unlistenError) unlistenError();
        window.removeEventListener('unhandledrejection', handleUnhandledRejection);
    });

    function handleUnhandledRejection(event: PromiseRejectionEvent) {
        event.preventDefault();
        const msg = event.reason?.message || event.reason?.toString() || 'An unexpected error occurred.';
        globalToast?.show(msg, 'error');
        console.error('Unhandled rejection:', event.reason);
    }
</script>

<Toast bind:this={globalToast} />

{#if currentScreen === 'home'}
    <HomeScreen on:create={() => currentScreen = 'create'} on:join={() => currentScreen = 'browse'} />
{:else if currentScreen === 'create'}
    <CreateJam on:created={() => currentScreen = 'host-session'} on:back={() => currentScreen = 'home'} />
{:else if currentScreen === 'browse'}
    <SessionBrowser on:joined={() => currentScreen = 'peer-session'} on:back={() => currentScreen = 'home'} />
{:else if currentScreen === 'host-session'}
    <HostSession on:ended={() => currentScreen = 'home'} />
{:else if currentScreen === 'peer-session'}
    <PeerSession on:disconnected={() => currentScreen = 'home'} />
{/if}
