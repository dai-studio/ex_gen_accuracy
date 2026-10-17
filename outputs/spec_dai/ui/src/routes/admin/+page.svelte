<script>
	import { api, me } from '#lib/api.js';
	import MiddlewareEditor from '#lib/MiddlewareEditor.svelte';

	let tab = $state('routes');
	let status = $state(null); // version, loadedAt, path, lastError
	let routes = $state([]);
	let savedPaths = $state([]); // route paths as currently saved in moxy.xml
	let selected = $state(-1);
	let settings = $state(null);
	let keys = $state([]);
	let newKeyName = $state('');
	let createdKey = $state(null);
	let message = $state(null);
	let failure = $state(null);

	const clone = (v) => JSON.parse(JSON.stringify(v));

	function applyConfig(c) {
		status = { version: c.version, loadedAt: c.loadedAt, path: c.path, lastError: c.lastError };
		routes = clone(c.config.routes ?? []);
		savedPaths = routes.map((r) => r.path);
		const auth = c.config.auth;
		settings = {
			host: c.config.server?.host ?? '',
			port: c.config.server?.port ?? '',
			authEnabled: !!auth,
			issuer: auth?.issuer ?? '',
			clientId: auth?.clientId ?? '',
			clientSecret: auth?.clientSecret ?? '',
			sessionTtl: auth?.sessionTtl ?? '',
			admins: (auth?.admins ?? []).join('\n')
		};
		if (selected >= routes.length) selected = routes.length - 1;
	}

	async function load() {
		try {
			await me();
			applyConfig(await api('GET', '/api/config'));
			keys = (await api('GET', '/api/keys')).keys;
		} catch (e) {
			failure = { message: e.message, details: e.details };
		}
	}

	$effect(() => {
		load();
	});

	async function run(fn, ok) {
		message = null;
		failure = null;
		try {
			await fn();
			message = ok;
		} catch (e) {
			failure = { message: e.message, details: e.details };
		}
	}

	// Drop empty optional fields so they are omitted from moxy.xml.
	function cleanRoute(r) {
		const out = clone(r);
		for (const k of ['auth', 'description']) if (!out[k]) delete out[k];
		for (const m of out.middleware ?? []) {
			if (m.type === 'ai' && !m.model) delete m.model;
		}
		if (out.upstream.type === 'load-balance') {
			out.upstream.upstreams = out.upstream.upstreams.filter((u) => u.trim());
		}
		return out;
	}

	const saveRoutes = () =>
		run(async () => {
			applyConfig(await api('PUT', '/api/config/routes', { routes: routes.map(cleanRoute) }));
		}, 'Routes saved to moxy.xml.');

	function saveMiddleware(i) {
		const path = savedPaths.includes(routes[i].path) ? routes[i].path : null;
		if (!path) {
			failure = { message: 'Save the routes first: this route is not in moxy.xml yet.' };
			return;
		}
		run(async () => {
			const mw = cleanRoute(routes[i]).middleware;
			applyConfig(await api('PUT', `/api/config/routes/${encodeURIComponent(path)}/middleware`, mw));
		}, `Middleware of ${path} saved.`);
	}

	function addRoute() {
		routes.push({
			path: '/new/*',
			auth: 'api-key',
			description: '',
			middleware: [],
			upstream: { type: 'upstream', url: 'https://' }
		});
		selected = routes.length - 1;
	}

	function setUpstreamType(r, type) {
		if (type === r.upstream.type) return;
		const urls = r.upstream.type === 'upstream' ? [r.upstream.url] : r.upstream.upstreams;
		r.upstream =
			type === 'upstream'
				? { type, url: urls[0] ?? '' }
				: { type, strategy: 'round-robin', upstreams: [...urls, ''].slice(0, Math.max(2, urls.length)) };
	}

	const saveSettings = () =>
		run(async () => {
			const s = settings;
			const body = { server: {} };
			if (s.host.trim()) body.server.host = s.host.trim();
			if (String(s.port).trim()) body.server.port = String(s.port).trim();
			body.auth = s.authEnabled
				? {
						issuer: s.issuer.trim(),
						clientId: s.clientId.trim(),
						...(s.clientSecret.trim() ? { clientSecret: s.clientSecret.trim() } : {}),
						...(s.sessionTtl.trim() ? { sessionTtl: s.sessionTtl.trim() } : {}),
						admins: s.admins.split('\n').map((a) => a.trim()).filter(Boolean)
					}
				: null;
			applyConfig(await api('PUT', '/api/config/settings', body));
		}, 'Settings saved to moxy.xml. Host/port changes apply after a restart.');

	const createKey = () =>
		run(async () => {
			createdKey = await api('POST', '/api/keys', { name: newKeyName.trim() });
			newKeyName = '';
			keys = (await api('GET', '/api/keys')).keys;
		}, 'API key created.');

	const revokeKey = (id) =>
		run(async () => {
			await api('DELETE', `/api/keys/${encodeURIComponent(id)}`);
			keys = (await api('GET', '/api/keys')).keys;
		}, 'API key revoked.');

	const fmt = (t) => (t ? new Date(t).toLocaleString() : '—');
</script>

<h1>Administration</h1>

{#if status}
	<div class="panel status">
		<div>Config <code>{status.path}</code> · version <code>{status.version}</code> · loaded {fmt(status.loadedAt)}</div>
		{#if status.lastError}
			<div class="error">
				<strong>Last reload failed</strong> ({fmt(status.lastError.at)}): the previous config is still active.
				<ul>{#each status.lastError.errors as e, i (i)}<li>{e}</li>{/each}</ul>
			</div>
		{/if}
	</div>
{/if}

<div class="tabs">
	{#each [['routes', 'Routes'], ['settings', 'Settings'], ['keys', 'API keys']] as [id, label] (id)}
		<button class:secondary={tab !== id} onclick={() => (tab = id)}>{label}</button>
	{/each}
	<button class="link" onclick={load}>Reload</button>
</div>

{#if message}<p class="ok">{message}</p>{/if}
{#if failure}
	<div class="error">
		{failure.message}
		{#if failure.details?.length > 1}<ul>{#each failure.details as d, i (i)}<li>{d}</li>{/each}</ul>{/if}
	</div>
{/if}

{#if tab === 'routes'}
	<div class="routes">
		<div class="panel list">
			{#each routes as r, i (i)}
				<button class="item" class:secondary={selected !== i} onclick={() => (selected = i)}>
					<code>{r.path}</code>
				</button>
			{/each}
			<button class="secondary" onclick={addRoute}>+ New route</button>
		</div>
		<div class="panel editor">
			{#if selected >= 0 && routes[selected]}
				{@const r = routes[selected]}
				<div class="grid">
					<label>Path <input bind:value={r.path} placeholder="/prefix/* or /exact" /></label>
					<label
						>Auth
						<select bind:value={r.auth}>
							<option value="api-key">api-key</option>
							<option value="session">session</option>
							<option value="none">none</option>
						</select>
					</label>
					<label class="wide">Description <input bind:value={r.description} /></label>
				</div>

				<h3>Upstream</h3>
				<div class="row">
					<select value={r.upstream.type} onchange={(e) => setUpstreamType(r, e.currentTarget.value)}>
						<option value="upstream">single upstream</option>
						<option value="load-balance">load-balance</option>
					</select>
					{#if r.upstream.type === 'upstream'}
						<input class="grow" bind:value={r.upstream.url} placeholder="https://api.example.com/v1" />
					{:else}
						<select bind:value={r.upstream.strategy}>
							<option>round-robin</option><option>random</option><option>failover</option>
						</select>
					{/if}
				</div>
				{#if r.upstream.type === 'load-balance'}
					{#each r.upstream.upstreams as _, k (k)}
						<div class="row">
							<input class="grow" bind:value={r.upstream.upstreams[k]} placeholder="https://…" />
							<button class="link" onclick={() => r.upstream.upstreams.splice(k, 1)}>×</button>
						</div>
					{/each}
					<button class="link" onclick={() => r.upstream.upstreams.push('')}>+ upstream</button>
				{/if}

				<h3>Middleware <span class="muted small">(request phase runs top to bottom, response phase bottom to top)</span></h3>
				<MiddlewareEditor bind:items={r.middleware} />

				<div class="actions">
					<button onclick={saveRoutes}>Save all routes</button>
					<button class="secondary" onclick={() => saveMiddleware(selected)}>Save only this route's middleware</button>
					<span class="spacer"></span>
					<button
						class="danger"
						onclick={() => {
							routes.splice(selected, 1);
							selected = -1;
						}}>Delete route</button
					>
				</div>
				<p class="muted small">Deleting takes effect when you save all routes.</p>
			{:else}
				<p class="muted">Select a route, or create a new one.</p>
			{/if}
		</div>
	</div>
{:else if tab === 'settings' && settings}
	<div class="panel">
		<h3>Server</h3>
		<div class="grid">
			<label>Host <input bind:value={settings.host} placeholder="127.0.0.1" /></label>
			<label>Port <input bind:value={settings.port} placeholder="8080" /></label>
		</div>
		<h3><label><input type="checkbox" bind:checked={settings.authEnabled} /> Sign-in (OpenID Connect)</label></h3>
		{#if settings.authEnabled}
			<div class="grid">
				<label class="wide">Issuer <input bind:value={settings.issuer} placeholder="https://idp.example.com" /></label>
				<label>Client ID <input bind:value={settings.clientId} /></label>
				<label>Client secret <input bind:value={settings.clientSecret} placeholder={'${MOXY_OIDC_CLIENT_SECRET}'} /></label>
				<label>Session TTL <input bind:value={settings.sessionTtl} placeholder="24h" /></label>
				<label class="wide">Admin emails (one per line) <textarea rows="3" bind:value={settings.admins}></textarea></label>
			</div>
			<p class="muted small">Never type secrets literally; reference environment variables as <code>${'{'}NAME}</code>.</p>
		{/if}
		<button onclick={saveSettings}>Save settings</button>
	</div>
{:else if tab === 'keys'}
	<div class="panel">
		<form
			class="row"
			onsubmit={(e) => {
				e.preventDefault();
				createKey();
			}}
		>
			<input bind:value={newKeyName} placeholder="Key name, e.g. build-agent" class="grow" />
			<button disabled={!newKeyName.trim()}>Create API key</button>
		</form>
		{#if createdKey}
			<div class="created">
				Copy this key now; it will not be shown again:<br />
				<code>{createdKey.key}</code>
			</div>
		{/if}
	</div>
	<div class="panel">
		<table>
			<thead><tr><th>Name</th><th>ID</th><th>Owner</th><th>Created</th><th>Last used</th><th></th></tr></thead>
			<tbody>
				{#each keys as k (k.id)}
					<tr class:revoked={k.revoked}>
						<td>{k.name}</td>
						<td><code>{k.id}</code></td>
						<td>{k.ownerSub}</td>
						<td>{fmt(k.createdAt)}</td>
						<td>{fmt(k.lastUsedAt)}</td>
						<td>
							{#if k.revoked}<span class="muted">revoked</span>{:else}
								<button class="danger" onclick={() => revokeKey(k.id)}>Revoke</button>
							{/if}
						</td>
					</tr>
				{:else}
					<tr><td colspan="6" class="muted">No API keys yet.</td></tr>
				{/each}
			</tbody>
		</table>
	</div>
{/if}

<style>
	h1 {
		margin-top: 0;
	}
	h3 {
		margin: 18px 0 8px;
		font-size: 15px;
	}
	.status {
		font-size: 14px;
	}
	.tabs {
		display: flex;
		gap: 8px;
		margin-bottom: 12px;
		align-items: center;
	}
	.routes {
		display: grid;
		grid-template-columns: 240px 1fr;
		gap: 16px;
		align-items: start;
	}
	@media (max-width: 760px) {
		.routes {
			grid-template-columns: 1fr;
		}
	}
	.list {
		display: flex;
		flex-direction: column;
		gap: 6px;
	}
	.list .item {
		text-align: left;
	}
	.list .item code {
		background: none;
		color: inherit;
	}
	.grid {
		display: grid;
		grid-template-columns: 1fr 1fr;
		gap: 10px 16px;
	}
	.grid label {
		display: flex;
		flex-direction: column;
		font-size: 13px;
		color: var(--muted);
		gap: 4px;
	}
	.grid .wide {
		grid-column: 1 / -1;
	}
	.row {
		display: flex;
		gap: 8px;
		align-items: center;
		margin: 6px 0;
	}
	.grow {
		flex: 1;
	}
	.actions {
		display: flex;
		gap: 10px;
		margin-top: 16px;
		flex-wrap: wrap;
	}
	.spacer {
		flex: 1;
	}
	.small {
		font-size: 13px;
		font-weight: normal;
	}
	.ok {
		color: var(--ok);
	}
	.created {
		margin-top: 12px;
		padding: 10px;
		border: 1px dashed var(--accent);
		border-radius: 8px;
	}
	.created code {
		word-break: break-all;
	}
	tr.revoked td {
		opacity: 0.55;
	}
</style>
