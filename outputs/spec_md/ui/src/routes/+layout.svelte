<script>
	import { onMount } from 'svelte';
	import { session, loadSession } from '#lib/session.svelte.js';
	import { logout } from '#lib/api.js';

	let { children } = $props();
	let path = $state('/');

	onMount(() => {
		path = window.location.pathname;
		loadSession();
	});

	const links = [
		{ href: '/', label: 'Endpoints' },
		{ href: '/chat', label: 'Chat' },
		{ href: '/admin', label: 'Admin' }
	];

	function active(href) {
		return href === '/' ? path === '/' : path.startsWith(href);
	}
</script>

<header>
	<a class="brand" href="/" data-sveltekit-reload>
		<svg viewBox="0 0 32 32" width="22" height="22" aria-hidden="true"
			><rect width="32" height="32" rx="7" fill="var(--accent)" /><path
				d="M8 23V9l8 8 8-8v14"
				stroke="white"
				stroke-width="3"
				fill="none"
				stroke-linejoin="round"
			/></svg
		>
		Moxy
	</a>
	<nav>
		{#each links as link (link.href)}
			{#if link.href !== '/admin' || session.me?.role === 'admin'}
				<!-- full page loads, so the server can apply its sign-in checks -->
				<a href={link.href} class:active={active(link.href)} data-sveltekit-reload>{link.label}</a>
			{/if}
		{/each}
	</nav>
	<div class="who">
		{#if !session.loaded}
			&nbsp;
		{:else if session.me}
			<span class="email" title={session.me.subject}>{session.me.email ?? session.me.subject}</span>
			<span class="badge">{session.me.role}</span>
			<button class="link" onclick={logout}>Sign out</button>
		{:else}
			<a href={`/auth/login?return_to=${encodeURIComponent(path)}`} data-sveltekit-reload>Sign in</a>
		{/if}
	</div>
</header>

<main>
	{@render children()}
</main>

<style>
	:global(:root) {
		--bg: #f7f7fb;
		--panel: #ffffff;
		--text: #1d1d27;
		--muted: #676779;
		--border: #e2e2ea;
		--accent: #4f46e5;
		--accent-soft: #eef0ff;
		--danger: #c62828;
		--danger-soft: #fdecec;
		--ok: #1b7f4b;
		--code: #f1f1f6;
		font-family: system-ui, -apple-system, 'Segoe UI', Roboto, sans-serif;
		color: var(--text);
		background: var(--bg);
	}
	@media (prefers-color-scheme: dark) {
		:global(:root) {
			--bg: #121218;
			--panel: #1b1b24;
			--text: #e8e8f0;
			--muted: #9a9aae;
			--border: #2c2c3a;
			--accent: #7c74ff;
			--accent-soft: #24234a;
			--danger: #ff6b6b;
			--danger-soft: #3a1d1d;
			--ok: #4cc38a;
			--code: #23232f;
		}
	}
	:global(body) {
		margin: 0;
	}
	:global(code),
	:global(pre) {
		font-family: ui-monospace, SFMono-Regular, Menlo, monospace;
		font-size: 0.9em;
	}
	:global(code) {
		background: var(--code);
		padding: 0.1em 0.35em;
		border-radius: 4px;
	}
	:global(button),
	:global(input),
	:global(select),
	:global(textarea) {
		font: inherit;
		color: inherit;
	}
	:global(input),
	:global(select),
	:global(textarea) {
		background: var(--panel);
		border: 1px solid var(--border);
		border-radius: 6px;
		padding: 0.4rem 0.55rem;
		box-sizing: border-box;
	}
	:global(button) {
		background: var(--accent);
		color: white;
		border: none;
		border-radius: 6px;
		padding: 0.45rem 0.9rem;
		cursor: pointer;
	}
	:global(button.secondary) {
		background: transparent;
		color: var(--text);
		border: 1px solid var(--border);
	}
	:global(button.danger) {
		background: var(--danger);
	}
	:global(button:disabled) {
		opacity: 0.5;
		cursor: default;
	}
	:global(button.link) {
		background: none;
		color: var(--accent);
		padding: 0;
	}
	:global(.panel) {
		background: var(--panel);
		border: 1px solid var(--border);
		border-radius: 10px;
		padding: 1rem 1.25rem;
		margin-bottom: 1rem;
	}
	:global(.error) {
		background: var(--danger-soft);
		color: var(--danger);
		border-radius: 8px;
		padding: 0.6rem 0.9rem;
		margin: 0.75rem 0;
		white-space: pre-wrap;
	}
	:global(.muted) {
		color: var(--muted);
	}
	:global(table) {
		width: 100%;
		border-collapse: collapse;
	}
	:global(th),
	:global(td) {
		text-align: left;
		padding: 0.55rem 0.5rem;
		border-bottom: 1px solid var(--border);
		vertical-align: top;
	}
	:global(th) {
		font-weight: 600;
		color: var(--muted);
		font-size: 0.85rem;
	}
	:global(.badge) {
		display: inline-block;
		font-size: 0.75rem;
		padding: 0.1rem 0.5rem;
		border-radius: 999px;
		background: var(--accent-soft);
		color: var(--accent);
	}
	header {
		display: flex;
		align-items: center;
		gap: 1.5rem;
		padding: 0.75rem 1.5rem;
		background: var(--panel);
		border-bottom: 1px solid var(--border);
	}
	.brand {
		display: flex;
		align-items: center;
		gap: 0.5rem;
		font-weight: 700;
		color: var(--text);
		text-decoration: none;
	}
	nav {
		display: flex;
		gap: 1rem;
		flex: 1;
	}
	nav a {
		color: var(--muted);
		text-decoration: none;
		padding: 0.25rem 0;
	}
	nav a.active {
		color: var(--text);
		border-bottom: 2px solid var(--accent);
	}
	.who {
		display: flex;
		align-items: center;
		gap: 0.6rem;
		font-size: 0.9rem;
	}
	.who a {
		color: var(--accent);
	}
	.email {
		color: var(--muted);
	}
	main {
		max-width: 1100px;
		margin: 0 auto;
		padding: 1.5rem;
	}
</style>
