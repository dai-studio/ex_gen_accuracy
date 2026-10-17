<script>
	import { api, chat, me } from '#lib/api.js';

	// Chat history lives in the browser only.
	let routes = $state([]);
	let route = $state('');
	let model = $state('');
	let messages = $state([]);
	let input = $state('');
	let busy = $state(false);
	let error = $state(null);
	let usage = $state(null);
	let controller = null;

	$effect(() => {
		Promise.all([me(), api('GET', '/api/endpoints')])
			.then(([, d]) => {
				routes = d.endpoints.filter((e) => e.chat);
				if (routes.length && !route) route = routes[0].path;
			})
			.catch((e) => (error = e.message));
	});

	async function send(ev) {
		ev?.preventDefault();
		const text = input.trim();
		if (!text || busy || !route) return;
		error = null;
		usage = null;
		messages.push({ role: 'user', content: text });
		input = '';
		const reply = { role: 'assistant', content: '' };
		messages.push(reply);
		const idx = messages.length - 1;
		busy = true;
		controller = new AbortController();
		try {
			const payload = {
				route,
				messages: messages.slice(0, -1).map(({ role, content }) => ({ role, content }))
			};
			if (model.trim()) payload.model = model.trim();
			await chat(
				payload,
				(e) => {
					if (e.type === 'delta') messages[idx].content += e.text;
					else if (e.type === 'error') error = `Upstream error ${e.status}: ${e.message}`;
					else if (e.type === 'done') usage = e.usage;
				},
				controller.signal
			);
		} catch (e) {
			if (e.name !== 'AbortError') error = e.message;
		} finally {
			busy = false;
			if (!messages[idx].content) messages.splice(idx, 1);
		}
	}

	function stop() {
		controller?.abort();
	}

	function clear() {
		messages = [];
		usage = null;
		error = null;
	}

	function onKey(e) {
		if (e.key === 'Enter' && !e.shiftKey) send(e);
	}
</script>

<div class="bar">
	<label>
		Model route
		<select bind:value={route} disabled={busy}>
			{#each routes as r (r.path)}
				<option value={r.path}>{r.path}{r.description ? ` — ${r.description}` : ''}</option>
			{/each}
		</select>
	</label>
	<label>
		Model <input bind:value={model} placeholder="route default" disabled={busy} />
	</label>
	<button class="secondary" onclick={clear} disabled={busy || !messages.length}>New chat</button>
</div>

{#if !routes.length && !error}
	<p class="muted">No route has an <code>&lt;ai&gt;</code> element, so there is nothing to chat with.</p>
{/if}

<div class="log panel">
	{#each messages as m, i (i)}
		<div class="msg {m.role}">
			<div class="role">{m.role}</div>
			<div class="content">{m.content}{#if busy && i === messages.length - 1}<span class="cursor">▍</span>{/if}</div>
		</div>
	{:else}
		<p class="muted">Messages are sent through the route's middleware (rate limits and token budgets apply) and are kept only in this browser tab.</p>
	{/each}
</div>

{#if error}<p class="error">{error}</p>{/if}
{#if usage}<p class="muted small">Tokens: {usage.input} in, {usage.output} out</p>{/if}

<form class="compose" onsubmit={send}>
	<textarea bind:value={input} onkeydown={onKey} rows="3" placeholder="Message (Enter to send, Shift+Enter for a new line)"></textarea>
	{#if busy}
		<button type="button" class="danger" onclick={stop}>Stop</button>
	{:else}
		<button type="submit" disabled={!input.trim() || !route}>Send</button>
	{/if}
</form>

<style>
	.bar {
		display: flex;
		gap: 16px;
		align-items: end;
		flex-wrap: wrap;
		margin-bottom: 16px;
	}
	.bar label {
		display: flex;
		flex-direction: column;
		font-size: 13px;
		color: var(--muted);
		gap: 4px;
	}
	.log {
		min-height: 280px;
		max-height: 60vh;
		overflow-y: auto;
	}
	.msg {
		margin-bottom: 14px;
	}
	.role {
		font-size: 12px;
		text-transform: uppercase;
		color: var(--muted);
		letter-spacing: 0.5px;
	}
	.content {
		white-space: pre-wrap;
	}
	.msg.user .content {
		font-weight: 500;
	}
	.cursor {
		opacity: 0.6;
	}
	.compose {
		display: flex;
		gap: 10px;
		align-items: stretch;
	}
	.compose textarea {
		flex: 1;
		resize: vertical;
	}
	.small {
		font-size: 13px;
	}
</style>
