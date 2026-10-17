import adapter from '@sveltejs/adapter-static';
import { sveltekit } from '@sveltejs/kit/vite';
import { defineConfig } from 'vite';

// `npm run dev` proxies data calls to a Moxy running on the default address.
const moxy = 'http://127.0.0.1:8080';

export default defineConfig({
	plugins: [
		sveltekit({
			// Built as a single-page app; Moxy embeds ./build and serves it under /_app/*.
			adapter: adapter({ pages: 'build', assets: 'build', fallback: 'index.html', strict: false }),
			appDir: '_app'
		})
	],
	server: { proxy: { '/api': moxy, '/auth': moxy } }
});
