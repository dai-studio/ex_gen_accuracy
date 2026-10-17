<script>
	// Edits a route's middleware list (the JSON form of the MML elements, in execution order).
	let { items = $bindable([]) } = $props();

	const templates = {
		headers: () => ({ type: 'headers', ops: [{ op: 'set', name: '', value: '' }] }),
		request: () => ({ type: 'request', ops: [{ op: 'timeout', duration: '60s' }] }),
		response: () => ({ type: 'response', ops: [{ op: 'set', name: '', value: '' }] }),
		ai: () => ({ type: 'ai', provider: 'openai', model: '' }),
		'rate-limit': () => ({ type: 'rate-limit', requests: '60', per: '1m', key: 'api-key' }),
		'token-budget': () => ({ type: 'token-budget', tokens: '100000', per: '1d', key: 'api-key' })
	};
	const opChoices = {
		headers: ['set', 'add', 'remove'],
		request: ['rewrite-path', 'max-body-size', 'timeout'],
		response: ['set', 'add', 'remove', 'cache']
	};
	const opTemplate = {
		set: { op: 'set', name: '', value: '' },
		add: { op: 'add', name: '', value: '' },
		remove: { op: 'remove', name: '' },
		'rewrite-path': { op: 'rewrite-path', from: '/', to: '/' },
		'max-body-size': { op: 'max-body-size', bytes: '1048576' },
		timeout: { op: 'timeout', duration: '60s' },
		cache: { op: 'cache', ttl: '5m' }
	};
	let newType = $state('headers');

	function add() {
		items.push(templates[newType]());
	}
	function move(i, d) {
		const j = i + d;
		if (j < 0 || j >= items.length) return;
		[items[i], items[j]] = [items[j], items[i]];
	}
	function setOp(ops, k, op) {
		ops[k] = { ...opTemplate[op] };
	}
</script>

<div class="mw">
	{#each items as m, i (i)}
		<div class="item">
			<div class="head">
				<strong>&lt;{m.type}&gt;</strong>
				<span class="spacer"></span>
				<button type="button" class="link" onclick={() => move(i, -1)} disabled={i === 0}>↑</button>
				<button type="button" class="link" onclick={() => move(i, 1)} disabled={i === items.length - 1}>↓</button>
				<button type="button" class="link danger-text" onclick={() => items.splice(i, 1)}>remove</button>
			</div>

			{#if m.type in opChoices}
				{#each m.ops as o, k (k)}
					<div class="row">
						<select value={o.op} onchange={(e) => setOp(m.ops, k, e.currentTarget.value)}>
							{#each opChoices[m.type] as c (c)}<option value={c}>{c}</option>{/each}
						</select>
						{#if o.op === 'set' || o.op === 'add' || o.op === 'remove'}
							<input bind:value={o.name} placeholder="header name" />
						{/if}
						{#if o.op === 'set' || o.op === 'add'}
							<input bind:value={o.value} placeholder="value (use ${'{'}NAME} for secrets)" class="grow" />
						{/if}
						{#if o.op === 'rewrite-path'}
							<input bind:value={o.from} placeholder="from" /> → <input bind:value={o.to} placeholder="to" />
						{/if}
						{#if o.op === 'max-body-size'}<input bind:value={o.bytes} placeholder="bytes" /> bytes{/if}
						{#if o.op === 'timeout'}<input bind:value={o.duration} placeholder="60s" />{/if}
						{#if o.op === 'cache'}ttl <input bind:value={o.ttl} placeholder="5m" />{/if}
						<button type="button" class="link danger-text" onclick={() => m.ops.splice(k, 1)}>×</button>
					</div>
				{/each}
				<button type="button" class="link" onclick={() => m.ops.push({ ...opTemplate[opChoices[m.type][0]] })}>+ add</button>
			{:else if m.type === 'ai'}
				<div class="row">
					provider
					<select bind:value={m.provider}>
						<option>openai</option><option>anthropic</option><option>ollama</option>
					</select>
					model override <input bind:value={m.model} placeholder="(none)" />
				</div>
			{:else if m.type === 'rate-limit'}
				<div class="row">
					<input bind:value={m.requests} size="6" /> requests per
					<select bind:value={m.per}><option>1s</option><option>1m</option><option>1h</option></select>
					keyed by
					<select bind:value={m.key}><option>api-key</option><option>user</option><option>ip</option></select>
				</div>
			{:else if m.type === 'token-budget'}
				<div class="row">
					<input bind:value={m.tokens} size="8" /> tokens per
					<select bind:value={m.per}><option>1h</option><option>1d</option><option>30d</option></select>
					keyed by
					<select bind:value={m.key}><option>api-key</option><option>user</option></select>
				</div>
			{/if}
		</div>
	{:else}
		<p class="muted">No middleware.</p>
	{/each}

	<div class="row">
		<select bind:value={newType}>
			{#each Object.keys(templates) as t (t)}<option value={t}>{t}</option>{/each}
		</select>
		<button type="button" class="secondary" onclick={add}>Add middleware</button>
	</div>
</div>

<style>
	.item {
		border: 1px solid var(--border);
		border-radius: 8px;
		padding: 8px 12px;
		margin-bottom: 8px;
	}
	.head {
		display: flex;
		gap: 10px;
		align-items: center;
		margin-bottom: 6px;
	}
	.spacer {
		flex: 1;
	}
	.row {
		display: flex;
		gap: 8px;
		align-items: center;
		flex-wrap: wrap;
		margin: 4px 0;
	}
	.grow {
		flex: 1;
		min-width: 200px;
	}
	.danger-text {
		color: var(--danger);
	}
</style>
