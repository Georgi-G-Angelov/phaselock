<script lang="ts">
    export let size: number = 24;
</script>

<div class="spinner" style:width="{size}px" style:height="{size}px"></div>

<style>
    .spinner {
        border: 3px solid var(--bg-elevated);
        border-top-color: var(--accent-green);
        border-radius: 50%;
        animation: spin 0.7s linear infinite;
    }

    @keyframes spin {
        to {
            transform: rotate(360deg);
        }
    }
</style>
