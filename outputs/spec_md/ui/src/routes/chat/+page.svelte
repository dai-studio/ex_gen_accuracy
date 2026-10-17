<script>
	import { onMount, tick } from 'svelte';
	import { api, streamChat } from '#lib/api.js';
	import { session, loadSession } from '#lib/session.svelte.js';

	// Chat history lives only in this browser tab; it is never stored on the server.
	let routes = $state([]);
	let routePath = $state('');
	let model = $state('');
	let system = $state('');
	let messages = $state([]);
	let input = $state('');
	let busy = $state(false);
	let error = $state('');
	let usage = $state(null);
	let controller = null;
	let log;

	const route = $derived(routes.find((r) => r.path === routePath));

	onMount(async () => {
		if (!session.loaded) await loadSession();
		try {
			routes = (await api('GET', '/api/endpoints')).endpoints.filter((e) => e.chat);
			routePath = routes[0]?.path ?? '';
		} catch (e) {
			error = e.message;
		}
	});

	async function scroll() {
		await tick();
		log?.scrollTo({ top: log.scrollHeight, behavior: 'smooth' });
	}

	async function send(event) {
		event?.preventDefault();
		const text = input.trim();
		if (!text || busy || !route) return;
		error = '';
		usage = null;
		input = '';
		messages.push({ role: 'user', content: text });
		const history = [
			...(system.trim() ? [{ role: 'system', content: system.trim() }] : []),
			...messages.map((m) => ({ role: m.role, content: m.content }))
		];
		messages.push({ role: 'assistant', content: '' });
		const reply = messages[messages.length - 1];
		busy = true;
		controller = new AbortController();
		scroll();
		try {
			await streamChat(
				{ route: route.path, model: route.model ? undefined : model.trim() || undefined, messages: history },
				(ev) => {
					if (ev.type === 'delta') {
						reply.content += ev.text;
						scroll();
					} else if (ev.type === 'error') {
						error = ev.message;
					} else if (ev.type === 'done') {
						usage = ev.usage;
					}
				},
				controller.signal
			);
		} catch (e) {
			if (e.name !== 'AbortError') error = e.message;
		} finally {
			busy = false;
			controller = null;
			if (!reply.content) messages.pop();
		}
	}

	function stop() {
		controller?.abort();
	}

	function clear() {
		messages = [];
		usage = null;
		error = '';
	}

	function onKey(e) {
		if (e.key === 'Enter' && !e.shiftKey) send(e);
	}
</script>

<div class="chat">
	<aside class="panel">
		<label>
			Model route
			<select bind:value={routePath} disabled={busy}>
				{#each routes as r (r.path)}
					<option value={r.path}>{r.path}{r.description ? ` — ${r.description}` : ''}</option>
				{/each}
			</select>
		</label>
		{#if routes.length === 0}
			<p class="muted">No route has an <code>&lt;ai&gt;</code> element, so there is nothing to chat with.</p>
		{/if}
		{#if route}
			<p class="muted small">Provider: {route.provider}</p>
			{#if route.model}
				<p class="muted small">Model: <code>{route.model}</code> (fixed by the route)</p>
			{:else}
				<label>
					Model
					<input bind:value={model} placeholder="e.g. gpt-4o-mini" disabled={busy} />
				</label>
			{/if}
		{/if}
		<label>
			System prompt
			<textarea bind:value={system} rows="4" placeholder="Optional" disabled={busy}></textarea>
		</label>
		<button class="secondary" onclick={clear} disabled={busy || messages.length === 0}>Clear conversation</button>
		{#if usage}
			<p class="muted small">Last reply: {usage.input_tokens} in · {usage.output_tokens} out · {usage.total_tokens} tokens</p>
		{/if}
	</aside>

	<section class="panel conversation">
		<div class="log" bind:this={log}>
			{#if messages.length === 0}
				<p class="muted empty">Send a message to start. Rate limits and token budgets of the route apply to you.</p>
			{/if}
			{#each messages as m, i (i)}
				<div class="msg {m.role}">
					<div class="role">{m.role === 'user' ? 'You' : 'Assistant'}</div>
					<div class="content">{m.content}{#if busy && i === messages.length - 1 && m.role === 'assistant'}<span class="cursor">▍</span>{/if}</div>
				</div>
			{/each}
		</div>
		{#if error}
			<div class="error">{error}</div>
		{/if}
		<form onsubmit={send}>
			<textarea bind:value={input} onkeydown={onKey} rows="3" placeholder="Message (Enter to send, Shift+Enter for a new line)" disabled={!route}></textarea>
			{#if busy}
				<button type="button" class="secondary" onclick={stop}>Stop</button>
			{:else}
				<button type="submit" disabled={!route || !input.trim() || (!route.model && !model.trim())}>Send</button>
			{/if}
		</form>
	</section>
</div>

<style>
	.chat {
		display: grid;
		grid-template-columns: 280px 1fr;
		gap: 1rem;
		align-items: start;
	}
	@media (max-width: 760px) {
		.chat {
			grid-template-columns: 1fr;
		}
	}
	aside label {
		display: flex;
		flex-direction: column;
		gap: 0.3rem;
		margin-bottom: 0.9rem;
		font-size: 0.9rem;
	}
	.small {
		font-size: 0.85rem;
	}
	.conversation {
		display: flex;
		flex-direction: column;
		height: calc(100vh - 170px);
		min-height: 420px;
	}
	.log {
		flex: 1;
		overflow-y: auto;
		padding-right: 0.25rem;
	}
	.empty {
		text-align: center;
		margin-top: 3rem;
	}
	.msg {
		margin-bottom: 1rem;
	}
	.role {
		font-size: 0.75rem;
		font-weight: 600;
		color: var(--muted);
		margin-bottom: 0.2rem;
	}
	.content {
		white-space: pre-wrap;
		line-height: 1.5;
	}
	.msg.user .content {
		background: var(--accent-soft);
		padding: 0.6rem 0.8rem;
		border-radius: 8px;
	}
	.cursor {
		animation: blink 1s steps(2) infinite;
	}
	@keyframes blink {
		50% {
			opacity: 0;
		}
	}
	form {
		display: flex;
		gap: 0.6rem;
		align-items: flex-end;
		margin-top: 0.75rem;
	}
	form textarea {
		flex: 1;
		resize: vertical;
	}
</style>
