// @ts-check
import { readFileSync } from 'node:fs';
import { defineConfig } from 'astro/config';
import starlight from '@astrojs/starlight';
import starlightLinksValidator from 'starlight-links-validator';

// The released version, read from the workspace manifest at build time.
// docs.yml builds from the release tag, so this always matches what
// `cargo install hotl` delivers.
const hotlVersion = readFileSync(new URL('../Cargo.toml', import.meta.url), 'utf8').match(
	/^version = "([^"]+)"/m,
)?.[1];

// Host-specific values (GitHub Pages). On the Cloudflare Pages + custom domain
// migration, point `site` at the domain and drop `base`.
const site = 'https://nrakochy.github.io';
const base = '/hotl';

// Link-preview image (source: og-image.html). Scrapers don't resolve relative
// paths, so this must be an absolute URL.
const ogImageUrl = `${site}${base}/og.png`;

export default defineConfig({
	site,
	base,
	integrations: [
		starlight({
			title: 'hotl',
			description:
				'A human-on-the-loop agent harness in one binary — fast, slim, secure, and extensible: a coding agent behind a permission gate with a kernel sandbox floor, an append-only session log with resume and undo, and a tmux dashboard for every agent you run.',
			head: [
				{ tag: 'meta', attrs: { property: 'og:image', content: ogImageUrl } },
				{ tag: 'meta', attrs: { property: 'og:image:width', content: '1200' } },
				{ tag: 'meta', attrs: { property: 'og:image:height', content: '630' } },
				{
					tag: 'meta',
					attrs: {
						property: 'og:image:alt',
						content: 'hotl — human on the loop: a terminal AI agent whose loop of plan, run, log is gated by you.',
					},
				},
				{ tag: 'meta', attrs: { name: 'twitter:image', content: ogImageUrl } },
			],
			social: [
				{ icon: 'github', label: 'GitHub', href: 'https://github.com/nrakochy/hotl' },
				{
					icon: 'seti:rust',
					label: `crates.io — hotl v${hotlVersion}`,
					href: 'https://crates.io/crates/hotl',
				},
			],
			// Relative links are intentional: they stay correct when `base` changes hosts.
			plugins: [starlightLinksValidator({ errorOnRelativeLinks: false })],
			sidebar: [
				{
					label: 'Start here',
					items: [
						{ label: 'Overview', slug: 'overview' },
						{ label: 'Quickstart', slug: 'quickstart' },
					],
				},
				{
					label: 'Using the agent',
					items: [
						{ label: 'The TUI console', slug: 'tui' },
						{ label: 'Sessions, resume & forking', slug: 'sessions' },
						{ label: 'Shell integration (zsh)', slug: 'shell' },
						{ label: 'Background sessions', slug: 'backgrounding' },
					],
				},
				{
					label: 'Safety model',
					items: [{ label: 'Permissions & sandbox', slug: 'permissions-and-sandbox' }],
				},
				{
					label: 'Extending',
					items: [
						{ label: 'Plugins (Agent Plugins)', slug: 'plugins' },
						{ label: 'Skills', slug: 'skills' },
						{ label: 'MCP servers', slug: 'mcp' },
						{ label: 'Retrieval (recall)', slug: 'retrieval' },
						{ label: 'Sub-agents (spawn, agent defs)', slug: 'agents' },
						{ label: 'Hooks & diagnostics', slug: 'hooks' },
						{ label: 'Gateways & key sources', slug: 'gateway' },
					],
				},
				{
					label: 'Reference',
					items: [
						{ label: 'Configuration', slug: 'configuration' },
						{ label: 'Troubleshooting', slug: 'troubleshooting' },
						{ label: 'Updating', slug: 'updating' },
						{ label: 'Uninstall', slug: 'uninstall' },
					],
				},
			],
		}),
	],
});
