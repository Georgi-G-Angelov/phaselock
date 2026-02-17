<script lang="ts">
    import HomeScreen from './lib/components/HomeScreen.svelte';
    import CreateJam from './lib/components/CreateJam.svelte';
    import SessionBrowser from './lib/components/SessionBrowser.svelte';
    import HostSession from './lib/components/HostSession.svelte';
    import PeerSession from './lib/components/PeerSession.svelte';

    type Screen = 'home' | 'create' | 'browse' | 'host-session' | 'peer-session';
    let currentScreen: Screen = 'home';
</script>

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
