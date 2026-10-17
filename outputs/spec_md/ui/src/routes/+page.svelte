<script>
	import { onMount } from 'svelte';
	import { api } from '#lib/api.js';

	let endpoints = $state([]);
	let error = $state('');
	let loaded = $state(false);
	let origin = $state('');

	onMount(async () => {
		origin = window.location.origin;
		try {
			endpoints = (await api('GET', '/api/endpoints')).endpoints;
		} catch (e) {
			error = e.message;
		}
		loaded = true;
	});

	function example(ep) {
		const prefix = ep.path.endsWith('/*') ? ep.path.slice(0, -1) : ep.path;
		const sample = ep.chat && ep.provider === 'openai' ? `${prefix}chat/completions` : prefix;
		const auth = ep.auth === 'api-key' ? ` \\\n  -H "Authorization: Bearer $MOXY_API_KEY"` : '';
		return `curl ${origin}${sample}${auth}`;
	}

	let selected = $state(null);
</script>

<h1>Endpoints</h1>
<p class="muted">
	Routes served by this Moxy gateway. Call them with your usual SDKs; use a Moxy API key where a route
	requires one.
</p>

{#if error}
	<div class="error">{error}</div>
{/if}

<div class="panel">
	{#if !loaded}
		<p class="muted">Loading…</p>
	{:else if endpoints.length === 0}
		<p class="muted">No routes are configured yet.</p>
	{:else}
		<table>
			<thead>
				<tr><th>Path</th><th>Description</th><th>Authentication</th><th></th></tr>
			</thead>
			<tbody>
				{#each endpoints as ep (ep.path)}
					<tr>
						<td><code>{ep.path}</code></td>
						<td>
							{ep.description || '—'}
							{#if ep.chat}
								<span class="badge" title="Available in the chat interface">chat · {ep.provider}{ep.model ? ` · ${ep.model}` : ''}</span>
							{/if}
						</td>
						<td>{ep.auth}</td>
						<td>
							<button class="link" onclick={() => (selected = selected === ep.path ? null : ep.path)}>
								{selected === ep.path ? 'Hide' : 'Example'}
							</button>
						</td>
					</tr>
					{#if selected === ep.path}
						<tr><td colspan="4"><pre>{example(ep)}</pre></td></tr>
					{/if}
				{/each}
			</tbody>
		</table>
	{/if}
</div>
<p class="muted">The same list is available as JSON from <a href="/api/endpoints">/api/endpoints</a>.</p>

<style>
	h1 {
		margin-top: 0;
	}
	pre {
		margin: 0;
		background: var(--code);
		padding: 0.75rem;
		border-radius: 6px;
		overflow-x: auto;
	}
	a {
		color: var(--accent);
	}
</style>
