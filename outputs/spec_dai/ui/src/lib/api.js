// Thin client for Moxy's JSON API (/api). All UI data comes from here.

let csrfToken = null;

export class ApiError extends Error {
	constructor(status, body) {
		super(body?.error?.message ?? `HTTP ${status}`);
		this.status = status;
		this.type = body?.error?.type ?? 'error';
		this.details = body?.error?.details ?? [];
	}
}

/** The signed-in identity, or null when anonymous. Remembers the CSRF token. */
export async function me() {
	const r = await fetch('/api/me', { credentials: 'same-origin' });
	if (r.status === 401) return null;
	const body = await r.json();
	if (!r.ok) throw new ApiError(r.status, body);
	csrfToken = body.csrfToken ?? null;
	return body;
}

export async function api(method, path, body) {
	const headers = { accept: 'application/json' };
	if (body !== undefined) headers['content-type'] = 'application/json';
	if (method !== 'GET' && csrfToken) headers['x-csrf-token'] = csrfToken;
	const r = await fetch(path, {
		method,
		headers,
		credentials: 'same-origin',
		body: body === undefined ? undefined : JSON.stringify(body)
	});
	const text = await r.text();
	const data = text ? JSON.parse(text) : null;
	if (!r.ok) throw new ApiError(r.status, data);
	return data;
}

/**
 * POST /api/chat and call onEvent for every server-sent event.
 * Resolves when the stream ends.
 */
export async function chat(payload, onEvent, signal) {
	const r = await fetch('/api/chat', {
		method: 'POST',
		credentials: 'same-origin',
		signal,
		headers: {
			'content-type': 'application/json',
			accept: 'text/event-stream',
			...(csrfToken ? { 'x-csrf-token': csrfToken } : {})
		},
		body: JSON.stringify(payload)
	});
	if (!r.ok) {
		let body = null;
		try {
			body = await r.json();
		} catch {
			/* not JSON */
		}
		throw new ApiError(r.status, body);
	}
	const reader = r.body.getReader();
	const decoder = new TextDecoder();
	let buf = '';
	for (;;) {
		const { value, done } = await reader.read();
		if (done) break;
		buf += decoder.decode(value, { stream: true });
		let i;
		while ((i = buf.indexOf('\n\n')) >= 0) {
			const raw = buf.slice(0, i);
			buf = buf.slice(i + 2);
			for (const line of raw.split('\n')) {
				if (line.startsWith('data: ')) onEvent(JSON.parse(line.slice(6)));
			}
		}
	}
}

export function loginUrl(returnTo) {
	return `/auth/login?return_to=${encodeURIComponent(returnTo)}`;
}
