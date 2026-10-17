import { fetchMe } from './api.js';

// The current caller, shared by every page.
export const session = $state({ me: null, loaded: false });

export async function loadSession() {
	session.me = await fetchMe();
	session.loaded = true;
	return session.me;
}
