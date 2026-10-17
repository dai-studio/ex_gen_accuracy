// Thin client for Moxy's JSON API. All data comes from /api.

let csrfToken = null;

export class ApiError extends Error {
	constructor(status, body) {
		super(body?.error?.message ?? `request failed with status ${status}`);
		this.status = status;
		this.type = body?.error?.type ?? 'unknown';
		this.details = body?.error?.details ?? [];
	}
}

/** The signed-in caller, or null when anonymous. Also remembers the CSRF token. */
export async function fetchMe() {
	const r = await fetch('/api/me', { credentials: 'same-origin' });
	if (!r.ok) return null;
	const me = await r.json();
	csrfToken = me.csrf_token ?? null;
	return me;
}

export function csrfHeaders() {
	return csrfToken ? { 'x-csrf-token': csrfToken } : {};
}

export async function api(method, path, body) {
	const init = { method, credentials: 'same-origin', headers: { ...csrfHeaders() } };
	if (body !== undefined) {
		init.headers['content-type'] = 'application/json';
		init.body = JSON.stringify(body);
	}
	const r = await fetch(path, init);
	if (r.status === 204) return null;
	const text = await r.text();
	const data = text ? JSON.parse(text) : null;
	if (!r.ok) throw new ApiError(r.status, data);
	return data;
}

export async function logout() {
	await fetch('/auth/logout', { method: 'POST', credentials: 'same-origin', headers: csrfHeaders(), redirect: 'manual' });
	window.location.href = '/';
}

/** Stream a chat reply. Calls onEvent for each server-sent event object. */
export async function streamChat(body, onEvent, signal) {
	const r = await fetch('/api/chat', {
		method: 'POST',
		credentials: 'same-origin',
		headers: { 'content-type': 'application/json', ...csrfHeaders() },
		body: JSON.stringify(body),
		signal
	});
	if (!r.ok) {
		let data = null;
		try {
			data = await r.json();
		} catch {
			/* not JSON: an upstream error passed through */
		}
		throw new ApiError(r.status, data);
	}
	const reader = r.body.getReader();
	const decoder = new TextDecoder();
	let buffer = '';
	for (;;) {
		const { value, done } = await reader.read();
		if (done) break;
		buffer += decoder.decode(value, { stream: true });
		let idx;
		while ((idx = buffer.indexOf('\n\n')) >= 0) {
			const block = buffer.slice(0, idx);
			buffer = buffer.slice(idx + 2);
			for (const line of block.split('\n')) {
				if (line.startsWith('data:')) {
					try {
						onEvent(JSON.parse(line.slice(5).trim()));
					} catch {
						/* ignore malformed lines */
					}
				}
			}
		}
	}
}
