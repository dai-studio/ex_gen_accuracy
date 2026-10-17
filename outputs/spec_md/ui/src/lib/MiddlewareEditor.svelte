<script>
	// Edits a route's middleware list (document order = request-phase order).
	let { items = $bindable([]) } = $props();

	const TYPES = {
		headers: 'Headers',
		request: 'Request',
		response: 'Response',
		ai: 'AI',
		rate_limit: 'Rate limit',
		token_budget: 'Token budget'
	};
	let newType = $state('headers');

	function blank(type) {
		switch (type) {
			case 'headers':
			case 'request':
			case 'response':
				return { type, ops: [] };
			case 'ai':
				return { type, provider: 'openai', model: '' };
			case 'rate_limit':
				return { type, requests: '60', per: '1m', key: '' };
			case 'token_budget':
				return { type, tokens: '100000', per: '1d', key: '' };
		}
	}

	function add() {
		items.push(blank(newType));
	}
	function remove(i) {
		items.splice(i, 1);
	}
	function move(i, d) {
		const j = i + d;
		if (j < 0 || j >= items.length) return;
		const [it] = items.splice(i, 1);
		items.splice(j, 0, it);
	}

	function blankOp(type, op) {
		switch (op) {
			case 'set':
			case 'add':
				return { op, name: '', value: '' };
			case 'remove':
				return { op, name: '' };
			case 'rewrite_path':
				return { op, from: '/', to: '/' };
			case 'max_body_size':
				return { op, bytes: '1048576' };
			case 'timeout':
				return { op, duration: '60s' };
			case 'cache':
				return { op, ttl: '5m' };
		}
	}
	const OPS = {
		headers: ['set', 'add', 'remove'],
		request: ['rewrite_path', 'max_body_size', 'timeout'],
		response: ['set', 'add', 'remove', 'cache']
	};
</script>

<div class="mw">
	{#each items as item, i (item)}
		<div class="item">
			<div class="head">
				<strong>{i + 1}. {TYPES[item.type] ?? item.type}</strong>
				<span class="actions">
					<button type="button" class="secondary" onclick={() => move(i, -1)} disabled={i === 0} title="Move up">↑</button>
					<button type="button" class="secondary" onclick={() => move(i, 1)} disabled={i === items.length - 1} title="Move down">↓</button>
					<button type="button" class="secondary" onclick={() => remove(i)} title="Remove">✕</button>
				</span>
			</div>

			{#if item.type in OPS}
				{#each item.ops as op, j (op)}
					<div class="row">
						<code class="op">{op.op.replace('_', '-')}</code>
						{#if op.op === 'set' || op.op === 'add'}
							<input bind:value={op.name} placeholder="Header name" />
							<input class="grow" bind:value={op.value} placeholder={'Value, e.g. Bearer ${API_KEY}'} />
						{:else if op.op === 'remove'}
							<input bind:value={op.name} placeholder="Header name" />
						{:else if op.op === 'rewrite_path'}
							<input bind:value={op.from} placeholder="from" />
							<input bind:value={op.to} placeholder="to" />
						{:else if op.op === 'max_body_size'}
							<input bind:value={op.bytes} placeholder="bytes" />
						{:else if op.op === 'timeout'}
							<input bind:value={op.duration} placeholder="60s" />
						{:else if op.op === 'cache'}
							<input bind:value={op.ttl} placeholder="ttl, e.g. 5m" />
						{/if}
						<button type="button" class="secondary" onclick={() => item.ops.splice(j, 1)}>✕</button>
					</div>
				{/each}
				<div class="row">
					{#each OPS[item.type] as op (op)}
						<button type="button" class="secondary small" onclick={() => item.ops.push(blankOp(item.type, op))}>+ {op.replace('_', '-')}</button>
					{/each}
				</div>
				{#if item.type === 'response'}
					<p class="hint">Response headers apply on the way back, in reverse document order.</p>
				{/if}
			{:else if item.type === 'ai'}
				<div class="row">
					<label>Provider
						<select bind:value={item.provider}>
							<option value="openai">openai</option>
							<option value="anthropic">anthropic</option>
							<option value="ollama">ollama</option>
						</select>
					</label>
					<label class="grow">Model override <input bind:value={item.model} placeholder="optional" /></label>
				</div>
			{:else if item.type === 'rate_limit'}
				<div class="row">
					<label>Requests <input bind:value={item.requests} /></label>
					<label>Per
						<select bind:value={item.per}>
							<option value="1s">second</option>
							<option value="1m">minute</option>
							<option value="1h">hour</option>
						</select>
					</label>
					<label>Counted per
						<select bind:value={item.key}>
							<option value="">route default</option>
							<option value="api-key">API key</option>
							<option value="user">user</option>
							<option value="ip">IP address</option>
						</select>
					</label>
				</div>
			{:else if item.type === 'token_budget'}
				<div class="row">
					<label>Tokens <input bind:value={item.tokens} /></label>
					<label>Per
						<select bind:value={item.per}>
							<option value="1h">hour</option>
							<option value="1d">day</option>
							<option value="30d">30 days</option>
						</select>
					</label>
					<label>Counted per
						<select bind:value={item.key}>
							<option value="">route default</option>
							<option value="api-key">API key</option>
							<option value="user">user</option>
						</select>
					</label>
				</div>
				<p class="hint">Requires an AI element on the same route.</p>
			{/if}
		</div>
	{/each}

	<div class="row">
		<select bind:value={newType}>
			{#each Object.entries(TYPES) as [value, label] (value)}
				<option {value}>{label}</option>
			{/each}
		</select>
		<button type="button" class="secondary" onclick={add}>Add middleware</button>
	</div>
</div>

<style>
	.item {
		border: 1px solid var(--border);
		border-radius: 8px;
		padding: 0.6rem 0.75rem;
		margin-bottom: 0.6rem;
	}
	.head {
		display: flex;
		justify-content: space-between;
		align-items: center;
		margin-bottom: 0.4rem;
	}
	.actions {
		display: flex;
		gap: 0.3rem;
	}
	.actions button,
	.row button {
		padding: 0.2rem 0.55rem;
	}
	.row {
		display: flex;
		flex-wrap: wrap;
		gap: 0.5rem;
		align-items: center;
		margin: 0.35rem 0;
	}
	.row label {
		display: flex;
		flex-direction: column;
		font-size: 0.8rem;
		color: var(--muted);
		gap: 0.15rem;
	}
	.grow {
		flex: 1;
		min-width: 200px;
	}
	.op {
		min-width: 6.5rem;
	}
	.small {
		font-size: 0.85rem;
	}
	.hint {
		font-size: 0.8rem;
		color: var(--muted);
		margin: 0.3rem 0 0;
	}
</style>
