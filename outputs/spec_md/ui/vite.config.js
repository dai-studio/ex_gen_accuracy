import { sveltekit } from '@sveltejs/kit/vite';
import adapter from '@sveltejs/adapter-static';
import { defineConfig } from 'vite';

// During `npm run dev`, API and sign-in requests go to a running Moxy.
const backend = process.env.MOXY_URL ?? 'http://127.0.0.1:8080';

export default defineConfig({
	plugins: [
		sveltekit({
			// A single-page application: one fallback shell, assets under /_app.
			adapter: adapter({ fallback: '200.html' }),
			appDir: '_app',
			paths: { relative: false }
		})
	],
	server: {
		proxy: { '/api': backend, '/auth': backend }
	}
});
