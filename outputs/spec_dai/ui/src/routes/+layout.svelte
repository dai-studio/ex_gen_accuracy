<script>
	import { page } from '$app/state';
	import { me, loginUrl } from '#lib/api.js';

	let { children } = $props();
	let user = $state(null);

	$effect(() => {
		me()
			.then((u) => (user = u))
			.catch(() => (user = null));
	});

	const links = [
		{ href: '/', label: 'Endpoints' },
		{ href: '/chat', label: 'Chat' },
		{ href: '/admin', label: 'Admin' }
	];
</script>

<header>
	<a class="brand" href="/">moxy</a>
	<nav>
		{#each links as l (l.href)}
			{#if l.href !== '/admin' || user?.role === 'admin'}
				<a href={l.href} data-sveltekit-reload class:active={page.url.pathname === l.href}>{l.label}</a>
			{/if}
		{/each}
	</nav>
	<div class="who">
		{#if user}
			<span title={user.sub}>{user.email || user.name}</span>
			<span class="badge">{user.role}</span>
			<form method="POST" action="/auth/logout"><button class="link">Sign out</button></form>
		{:else}
			<a href={loginUrl(page.url.pathname)} data-sveltekit-reload>Sign in</a>
		{/if}
	</div>
</header>

<main>
	{@render children()}
</main>

<style>
	:global(:root) {
		--bg: #f7f8fa;
		--panel: #ffffff;
		--text: #1d2330;
		--muted: #5f6b7d;
		--border: #dfe3ea;
		--accent: #3b5bdb;
		--accent-text: #ffffff;
		--danger: #c92a2a;
		--ok: #2b8a3e;
		--code: #eef1f6;
		color-scheme: light;
	}
	@media (prefers-color-scheme: dark) {
		:global(:root) {
			--bg: #12151b;
			--panel: #1a1f27;
			--text: #e4e8ef;
			--muted: #98a2b3;
			--border: #2c333f;
			--accent: #7b93f5;
			--accent-text: #0d1117;
			--danger: #ff6b6b;
			--ok: #69db7c;
			--code: #232a35;
			color-scheme: dark;
		}
	}
	:global(body) {
		margin: 0;
		background: var(--bg);
		color: var(--text);
		font: 15px/1.5 system-ui, -apple-system, 'Segoe UI', sans-serif;
	}
	:global(code, pre, textarea.mono) {
		font-family: ui-monospace, 'SF Mono', Menlo, monospace;
		font-size: 13px;
	}
	:global(code) {
		background: var(--code);
		padding: 1px 5px;
		border-radius: 4px;
	}
	:global(input, select, textarea) {
		font: inherit;
		color: inherit;
		background: var(--panel);
		border: 1px solid var(--border);
		border-radius: 6px;
		padding: 6px 8px;
		box-sizing: border-box;
	}
	:global(button) {
		font: inherit;
		cursor: pointer;
		border: 1px solid var(--accent);
		background: var(--accent);
		color: var(--accent-text);
		border-radius: 6px;
		padding: 6px 14px;
	}
	:global(button.secondary) {
		background: transparent;
		color: var(--accent);
	}
	:global(button.danger) {
		background: transparent;
		color: var(--danger);
		border-color: var(--danger);
	}
	:global(button.link) {
		background: none;
		border: none;
		color: var(--accent);
		padding: 0;
	}
	:global(button:disabled) {
		opacity: 0.5;
		cursor: default;
	}
	:global(.panel) {
		background: var(--panel);
		border: 1px solid var(--border);
		border-radius: 10px;
		padding: 16px 20px;
		margin-bottom: 16px;
	}
	:global(.error) {
		color: var(--danger);
	}
	:global(.muted) {
		color: var(--muted);
	}
	:global(table) {
		width: 100%;
		border-collapse: collapse;
	}
	:global(th, td) {
		text-align: left;
		padding: 8px 10px;
		border-bottom: 1px solid var(--border);
		vertical-align: top;
	}
	:global(th) {
		color: var(--muted);
		font-weight: 600;
		font-size: 13px;
	}
	header {
		display: flex;
		align-items: center;
		gap: 24px;
		padding: 10px 24px;
		border-bottom: 1px solid var(--border);
		background: var(--panel);
		flex-wrap: wrap;
	}
	.brand {
		font-weight: 700;
		font-size: 18px;
		color: var(--accent);
		text-decoration: none;
		letter-spacing: 0.5px;
	}
	nav {
		display: flex;
		gap: 16px;
		flex: 1;
	}
	nav a {
		color: var(--muted);
		text-decoration: none;
	}
	nav a.active {
		color: var(--text);
		font-weight: 600;
	}
	.who {
		display: flex;
		align-items: center;
		gap: 10px;
		font-size: 14px;
	}
	.who form {
		margin: 0;
	}
	.badge {
		font-size: 12px;
		border: 1px solid var(--border);
		border-radius: 10px;
		padding: 0 8px;
		color: var(--muted);
	}
	main {
		max-width: 1100px;
		margin: 24px auto;
		padding: 0 16px;
	}
</style>
