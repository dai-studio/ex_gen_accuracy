<script>
	import { onMount } from 'svelte';
	import { api } from '#lib/api.js';
	import { session, loadSession } from '#lib/session.svelte.js';
	import MiddlewareEditor from '#lib/MiddlewareEditor.svelte';

	let tab = $state('routes');
	let status = $state(null);
	let config = $state(null);
	let error = $state('');
	let details = $state([]);
	let notice = $state('');

	// route editing
	let editing = $state(null); // { index, draft, original }

	// system settings
	let system = $state(null);

	// API keys
	let keys = $state([]);
	let newKeyName = $state('');
	let createdKey = $state(null);

	onMount(async () => {
		if (!session.loaded) await loadSession();
		await refresh();
		await loadKeys();
	});

	function fail(e) {
		error = e.message;
		details = e.details ?? [];
		notice = '';
	}
	function ok(msg) {
		error = '';
		details = [];
		notice = msg;
		setTimeout(() => (notice = ''), 4000);
	}

	function apply(data) {
		status = data.status;
		config = data.config;
		system = {
			server: { host: '', port: '', ...(config.server ?? {}) },
			auth: config.auth
				? { client_secret: '', session_ttl: '', ...config.auth, admins: [...config.auth.admins] }
				: null
		};
	}

	async function refresh() {
		try {
			apply(await api('GET', '/api/config'));
		} catch (e) {
			fail(e);
		}
	}

	// ---- routes -------------------------------------------------------------

	function clone(v) {
		return JSON.parse(JSON.stringify(v));
	}

	function emptyToUndefined(v) {
		return v === '' || v === null ? undefined : v;
	}

	/** Drop empty optional fields so the server sees them as absent. */
	function cleanRoute(r) {
		const out = {
			path: r.path.trim(),
			auth: emptyToUndefined(r.auth),
			description: emptyToUndefined(r.description),
			middleware: r.middleware.map((m) => {
				const c = { ...m };
				for (const k of ['model', 'key']) if (k in c) c[k] = emptyToUndefined(c[k]);
				return c;
			}),
			destination:
				r.destination.type === 'upstream'
					? { type: 'upstream', url: r.destination.url.trim() }
					: {
							type: 'load_balance',
							strategy: r.destination.strategy,
							upstreams: r.destination.upstreams.map((u) => u.trim()).filter(Boolean)
						}
		};
		return out;
	}

	function startEdit(index) {
		const r = clone(config.routes[index]);
		const draft = {
			path: r.path,
			auth: r.auth ?? '',
			description: r.description ?? '',
			middleware: (r.middleware ?? []).map((m) => ({ model: '', key: '', ...m })),
			destination: r.destination ?? { type: 'upstream', url: '' }
		};
		if (draft.destination.type === 'upstream') draft.destination.upstreams = [draft.destination.url, ''];
		else draft.destination.url = draft.destination.upstreams[0] ?? '';
		draft.destination.strategy ??= 'round-robin';
		editing = { index, draft, original: clone(cleanRoute(draft)) };
	}

	function startNew() {
		editing = {
			index: -1,
			draft: {
				path: '/new/*',
				auth: 'api-key',
				description: '',
				middleware: [],
				destination: { type: 'upstream', url: 'https://', upstreams: ['', ''], strategy: 'round-robin' }
			},
			original: null
		};
	}

	function sameExceptMiddleware(a, b) {
		if (!a || !b) return false;
		const strip = (r) => JSON.stringify({ ...r, middleware: undefined });
		return strip(a) === strip(b);
	}

	async function saveRoute() {
		const route = cleanRoute(editing.draft);
		try {
			let data;
			if (editing.index >= 0 && sameExceptMiddleware(route, editing.original)) {
				data = await api('PUT', `/api/config/middleware?path=${encodeURIComponent(route.path)}`, route.middleware);
			} else {
				const routes = clone(config.routes);
				if (editing.index >= 0) routes[editing.index] = route;
				else routes.push(route);
				data = await api('PUT', '/api/config/routes', routes);
			}
			apply(data);
			editing = null;
			ok('Saved to moxy.xml and reloaded.');
		} catch (e) {
			fail(e);
		}
	}

	async function deleteRoute(index) {
		const path = config.routes[index].path;
		if (!confirm(`Delete route ${path}?`)) return;
		const routes = clone(config.routes);
		routes.splice(index, 1);
		try {
			apply(await api('PUT', '/api/config/routes', routes));
			ok(`Deleted ${path}.`);
		} catch (e) {
			fail(e);
		}
	}

	function describeDestination(d) {
		if (!d) return '—';
		return d.type === 'upstream' ? d.url : `${d.strategy}: ${d.upstreams.join(', ')}`;
	}

	// ---- system ---------------------------------------------------------------

	function enableAuth() {
		system.auth = { issuer: 'https://', client_id: 'moxy', client_secret: '${MOXY_OIDC_CLIENT_SECRET}', session_ttl: '24h', admins: [''] };
	}

	async function saveSystem() {
		const body = {
			server: {
				host: emptyToUndefined(system.server.host?.trim()),
				port: emptyToUndefined(String(system.server.port ?? '').trim())
			},
			auth: system.auth
				? {
						issuer: system.auth.issuer.trim(),
						client_id: system.auth.client_id.trim(),
						client_secret: emptyToUndefined(system.auth.client_secret?.trim()),
						session_ttl: emptyToUndefined(system.auth.session_ttl?.trim()),
						admins: system.auth.admins.map((a) => a.trim()).filter(Boolean)
					}
				: undefined
		};
		try {
			apply(await api('PUT', '/api/config/system', body));
			ok('System settings saved. Server address changes take effect after a restart.');
		} catch (e) {
			fail(e);
		}
	}

	// ---- keys -----------------------------------------------------------------

	async function loadKeys() {
		try {
			keys = (await api('GET', '/api/keys')).keys;
		} catch (e) {
			fail(e);
		}
	}

	async function createKey(e) {
		e.preventDefault();
		try {
			createdKey = await api('POST', '/api/keys', { name: newKeyName.trim() });
			newKeyName = '';
			await loadKeys();
		} catch (err) {
			fail(err);
		}
	}

	async function revokeKey(k) {
		if (!confirm(`Revoke key "${k.name}"? Callers using it will be rejected.`)) return;
		try {
			await api('DELETE', `/api/keys/${encodeURIComponent(k.id)}`);
			await loadKeys();
			ok(`Revoked ${k.name}.`);
		} catch (e) {
			fail(e);
		}
	}

	function when(t) {
		return t ? new Date(t).toLocaleString() : 'never';
	}
</script>

<h1>Administration</h1>

{#if status}
	<div class="panel status" class:failed={status.last_error}>
		<div>
			<strong>Active configuration</strong> · generation {status.generation} ·
			<code title={status.hash}>{status.hash.slice(0, 12)}</code> · loaded {when(status.loaded_at)} ·
			<span class="muted">{status.path}</span>
		</div>
		{#if status.last_error}
			<div class="error">
				The last reload failed at {when(status.last_error.at)}; the previous configuration is still active.
				{#each status.last_error.errors as e, i (i)}
					{'\n'}• {e.location}: {e.message}
				{/each}
			</div>
		{/if}
		<button class="secondary" onclick={refresh}>Refresh</button>
	</div>
{/if}

{#if error}
	<div class="error">
		{error}
		{#if details.length}
			{#each details as d, i (i)}{'\n'}• {d.location}: {d.message}{/each}
		{/if}
	</div>
{/if}
{#if notice}
	<div class="notice">{notice}</div>
{/if}

<div class="tabs">
	{#each [['routes', 'Routes'], ['system', 'System settings'], ['keys', 'API keys']] as [id, label] (id)}
		<button class:active={tab === id} class="tab" onclick={() => (tab = id)}>{label}</button>
	{/each}
</div>

{#if tab === 'routes' && config}
	{#if editing}
		<div class="panel">
			<h2>{editing.index >= 0 ? `Edit ${editing.original.path}` : 'New route'}</h2>
			<div class="grid">
				<label>Path <input bind:value={editing.draft.path} placeholder="/openai/*" /></label>
				<label>Authentication
					<select bind:value={editing.draft.auth}>
						<option value="api-key">api-key</option>
						<option value="session">session</option>
						<option value="none">none</option>
					</select>
				</label>
				<label class="wide">Description <input bind:value={editing.draft.description} /></label>
			</div>

			<h3>Destination</h3>
			<div class="grid">
				<label>Type
					<select bind:value={editing.draft.destination.type}>
						<option value="upstream">single upstream</option>
						<option value="load_balance">load balance</option>
					</select>
				</label>
				{#if editing.draft.destination.type === 'upstream'}
					<label class="wide">Upstream URL <input bind:value={editing.draft.destination.url} /></label>
				{:else}
					<label>Strategy
						<select bind:value={editing.draft.destination.strategy}>
							<option value="round-robin">round-robin</option>
							<option value="random">random</option>
							<option value="failover">failover</option>
						</select>
					</label>
					<div class="wide">
						{#each editing.draft.destination.upstreams as _, i (i)}
							<div class="url-row">
								<input bind:value={editing.draft.destination.upstreams[i]} placeholder="https://…" />
								<button type="button" class="secondary" onclick={() => editing.draft.destination.upstreams.splice(i, 1)}>✕</button>
							</div>
						{/each}
						<button type="button" class="secondary" onclick={() => editing.draft.destination.upstreams.push('')}>+ upstream</button>
					</div>
				{/if}
			</div>

			<h3>Middleware</h3>
			<p class="muted small">Runs top to bottom on requests and bottom to top on responses. Use <code>{'${VAR}'}</code> references for secrets.</p>
			<MiddlewareEditor bind:items={editing.draft.middleware} />

			<div class="buttons">
				<button onclick={saveRoute}>Save</button>
				<button class="secondary" onclick={() => (editing = null)}>Cancel</button>
			</div>
		</div>
	{:else}
		<div class="panel">
			<table>
				<thead><tr><th>Path</th><th>Auth</th><th>Middleware</th><th>Destination</th><th></th></tr></thead>
				<tbody>
					{#each config.routes as r, i (r.path)}
						<tr>
							<td><code>{r.path}</code><div class="muted small">{r.description ?? ''}</div></td>
							<td>{r.auth ?? 'api-key'}</td>
							<td>{(r.middleware ?? []).map((m) => m.type.replace('_', '-')).join(', ') || '—'}</td>
							<td class="dest">{describeDestination(r.destination)}</td>
							<td class="nowrap">
								<button class="secondary" onclick={() => startEdit(i)}>Edit</button>
								<button class="secondary" onclick={() => deleteRoute(i)}>Delete</button>
							</td>
						</tr>
					{:else}
						<tr><td colspan="5" class="muted">No routes yet.</td></tr>
					{/each}
				</tbody>
			</table>
			<div class="buttons"><button onclick={startNew}>Add route</button></div>
		</div>
	{/if}
{/if}

{#if tab === 'system' && system}
	<div class="panel">
		<h2>Server</h2>
		<div class="grid">
			<label>Host <input bind:value={system.server.host} placeholder="127.0.0.1" /></label>
			<label>Port <input bind:value={system.server.port} placeholder="8080" /></label>
		</div>
		<p class="muted small">Changes to the address take effect after Moxy restarts.</p>

		<h2>Sign-in (OpenID Connect)</h2>
		{#if system.auth}
			<div class="grid">
				<label class="wide">Issuer <input bind:value={system.auth.issuer} /></label>
				<label>Client ID <input bind:value={system.auth.client_id} /></label>
				<label>Client secret <input bind:value={system.auth.client_secret} placeholder={'${MOXY_OIDC_CLIENT_SECRET}'} /></label>
				<label>Session lifetime <input bind:value={system.auth.session_ttl} placeholder="24h" /></label>
			</div>
			<p class="muted small">The client secret must be an environment variable reference, never the secret itself.</p>
			<h3>Administrators</h3>
			{#each system.auth.admins as _, i (i)}
				<div class="url-row">
					<input bind:value={system.auth.admins[i]} placeholder="person@example.com" />
					<button type="button" class="secondary" onclick={() => system.auth.admins.splice(i, 1)}>✕</button>
				</div>
			{/each}
			<button type="button" class="secondary" onclick={() => system.auth.admins.push('')}>+ administrator</button>
		{:else}
			<p class="muted">Sign-in is not configured.</p>
			<button class="secondary" onclick={enableAuth}>Configure sign-in</button>
		{/if}
		<div class="buttons"><button onclick={saveSystem}>Save system settings</button></div>
	</div>
{/if}

{#if tab === 'keys'}
	<div class="panel">
		<form class="url-row" onsubmit={createKey}>
			<input bind:value={newKeyName} placeholder="Key name, e.g. build-agent" />
			<button type="submit" disabled={!newKeyName.trim()}>Create key</button>
		</form>
		{#if createdKey}
			<div class="created">
				<strong>New key for “{createdKey.name}”.</strong> Copy it now; it will not be shown again.
				<div class="url-row">
					<code class="key">{createdKey.key}</code>
					<button class="secondary" onclick={() => navigator.clipboard?.writeText(createdKey.key)}>Copy</button>
					<button class="secondary" onclick={() => (createdKey = null)}>Done</button>
				</div>
			</div>
		{/if}
		<table>
			<thead><tr><th>Name</th><th>ID</th><th>Owner</th><th>Created</th><th>Last used</th><th>Status</th><th></th></tr></thead>
			<tbody>
				{#each keys as k (k.id)}
					<tr class:revoked={k.revoked}>
						<td>{k.name}</td>
						<td><code class="small">{k.id.slice(0, 8)}</code></td>
						<td class="small">{k.owner}</td>
						<td class="small">{when(k.created_at)}</td>
						<td class="small">{when(k.last_used_at)}</td>
						<td>{k.revoked ? 'revoked' : 'active'}</td>
						<td>{#if !k.revoked}<button class="secondary" onclick={() => revokeKey(k)}>Revoke</button>{/if}</td>
					</tr>
				{:else}
					<tr><td colspan="7" class="muted">No API keys yet.</td></tr>
				{/each}
			</tbody>
		</table>
	</div>
{/if}

<style>
	h1 {
		margin-top: 0;
	}
	h2 {
		font-size: 1.1rem;
	}
	h3 {
		font-size: 0.95rem;
		margin: 1.1rem 0 0.5rem;
	}
	.status {
		display: flex;
		flex-direction: column;
		gap: 0.5rem;
		align-items: flex-start;
	}
	.status.failed {
		border-color: var(--danger);
	}
	.notice {
		background: var(--accent-soft);
		color: var(--accent);
		border-radius: 8px;
		padding: 0.6rem 0.9rem;
		margin: 0.75rem 0;
	}
	.tabs {
		display: flex;
		gap: 0.25rem;
		margin-bottom: 0.75rem;
	}
	.tab {
		background: transparent;
		color: var(--muted);
		border-bottom: 2px solid transparent;
		border-radius: 0;
	}
	.tab.active {
		color: var(--text);
		border-bottom-color: var(--accent);
	}
	.grid {
		display: grid;
		grid-template-columns: repeat(auto-fill, minmax(220px, 1fr));
		gap: 0.75rem;
	}
	.grid label {
		display: flex;
		flex-direction: column;
		gap: 0.25rem;
		font-size: 0.85rem;
		color: var(--muted);
	}
	.wide {
		grid-column: 1 / -1;
	}
	.url-row {
		display: flex;
		gap: 0.5rem;
		margin-bottom: 0.5rem;
		align-items: center;
	}
	.url-row input {
		flex: 1;
	}
	.buttons {
		display: flex;
		gap: 0.5rem;
		margin-top: 1rem;
	}
	.small {
		font-size: 0.85rem;
	}
	.nowrap {
		white-space: nowrap;
	}
	.dest {
		word-break: break-all;
		font-size: 0.85rem;
	}
	.created {
		border: 1px solid var(--accent);
		border-radius: 8px;
		padding: 0.75rem;
		margin: 0.75rem 0;
	}
	.key {
		flex: 1;
		padding: 0.4rem;
		word-break: break-all;
	}
	tr.revoked td {
		color: var(--muted);
		text-decoration: line-through;
	}
	tr.revoked td:last-child,
	tr.revoked td:nth-last-child(2) {
		text-decoration: none;
	}
</style>
