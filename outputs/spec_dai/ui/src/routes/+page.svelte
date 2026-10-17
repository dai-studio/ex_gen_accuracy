<script>
	import { api } from '#lib/api.js';

	let endpoints = $state([]);
	let error = $state(null);
	let loading = $state(true);

	$effect(() => {
		api('GET', '/api/endpoints')
			.then((d) => (endpoints = d.endpoints))
			.catch((e) => (error = e.message))
			.finally(() => (loading = false));
	});

	const authLabel = { none: 'open', 'api-key': 'API key', session: 'browser session' };
	const origin = typeof location !== 'undefined' ? location.origin : '';
	const example = (p) => `${origin}${p.replace(/\/\*$/, '/')}`;
</script>

<h1>Endpoints</h1>
<p class="muted">
	Routes served by this Moxy. Point your SDK or HTTP client at a route and authenticate with a Moxy
	API key (<code>Authorization: Bearer moxy_…</code>).
</p>

<div class="panel">
	{#if loading}
		<p class="muted">Loading…</p>
	{:else if error}
		<p class="error">{error}</p>
	{:else if endpoints.length === 0}
		<p class="muted">No routes are configured yet.</p>
	{:else}
		<table>
			<thead>
				<tr><th>Path</th><th>Description</th><th>Auth</th><th>Base URL</th></tr>
			</thead>
			<tbody>
				{#each endpoints as e (e.path)}
					<tr>
						<td><code>{e.path}</code> {#if e.chat}<span class="tag">chat</span>{/if}</td>
						<td>{e.description || '—'}</td>
						<td>{authLabel[e.auth] ?? e.auth}</td>
						<td><code>{example(e.path)}</code></td>
					</tr>
				{/each}
			</tbody>
		</table>
	{/if}
</div>

<style>
	h1 {
		margin-top: 0;
	}
	.tag {
		font-size: 11px;
		border-radius: 8px;
		padding: 0 6px;
		margin-left: 4px;
		background: var(--accent);
		color: var(--accent-text);
	}
</style>
