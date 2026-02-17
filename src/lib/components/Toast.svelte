<script lang="ts">
    import { onDestroy } from 'svelte';

    export type ToastVariant = 'error' | 'success' | 'info';

    interface ToastItem {
        id: number;
        message: string;
        variant: ToastVariant;
    }

    let toasts: ToastItem[] = [];
    let nextId = 0;
    const timers: Map<number, ReturnType<typeof setTimeout>> = new Map();

    export function show(message: string, variant: ToastVariant = 'info') {
        const id = nextId++;
        toasts = [...toasts, { id, message, variant }];

        const timer = setTimeout(() => {
            dismiss(id);
        }, 4000);
        timers.set(id, timer);
    }

    function dismiss(id: number) {
        toasts = toasts.filter(t => t.id !== id);
        const timer = timers.get(id);
        if (timer) {
            clearTimeout(timer);
            timers.delete(id);
        }
    }

    onDestroy(() => {
        timers.forEach(t => clearTimeout(t));
        timers.clear();
    });
</script>

{#if toasts.length > 0}
    <div class="toast-container">
        {#each toasts as toast (toast.id)}
            <button
                class="toast toast-{toast.variant}"
                on:click={() => dismiss(toast.id)}
            >
                <span class="toast-icon">
                    {#if toast.variant === 'error'}✕
                    {:else if toast.variant === 'success'}✓
                    {:else}ℹ
                    {/if}
                </span>
                <span class="toast-message">{toast.message}</span>
            </button>
        {/each}
    </div>
{/if}

<style>
    .toast-container {
        position: fixed;
        top: 1rem;
        right: 1rem;
        z-index: 9999;
        display: flex;
        flex-direction: column;
        gap: 0.5rem;
        max-width: 360px;
    }

    .toast {
        display: flex;
        align-items: center;
        gap: 0.625rem;
        padding: 0.75rem 1rem;
        border-radius: var(--radius-md);
        border: none;
        cursor: pointer;
        font-size: 0.875rem;
        color: var(--text-primary);
        animation: slideIn 200ms ease-out;
        text-align: left;
        width: 100%;
    }

    .toast-error {
        background: var(--error-red);
    }

    .toast-success {
        background: var(--accent-green);
        color: #000;
    }

    .toast-info {
        background: var(--bg-elevated);
        border: 1px solid var(--border-subtle);
    }

    .toast-icon {
        flex-shrink: 0;
        font-weight: 700;
    }

    .toast-message {
        flex: 1;
        min-width: 0;
    }

    @keyframes slideIn {
        from {
            opacity: 0;
            transform: translateX(1rem);
        }
        to {
            opacity: 1;
            transform: translateX(0);
        }
    }
</style>
