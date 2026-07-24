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
export default defineConfig({
	site: 'https://nrakochy.github.io',
	base: '/hotl',
	integrations: [
		starlight({
			title: 'hotl',
			description:
				'A human-on-the-loop terminal AI agent: gated tools under a kernel sandbox floor, an append-only session log with resume and undo, MCP/ACP, any Anthropic or OpenAI-compatible model.',
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
						{ label: 'MCP servers', slug: 'mcp' },
						{ label: 'Retrieval (recall)', slug: 'retrieval' },
						{ label: 'Hooks & diagnostics', slug: 'hooks' },
						{ label: 'Gateways & key sources', slug: 'gateway' },
					],
				},
				{
					label: 'Reference',
					items: [
						{ label: 'Configuration', slug: 'configuration' },
						{ label: 'Troubleshooting', slug: 'troubleshooting' },
						{ label: 'Uninstall', slug: 'uninstall' },
					],
				},
			],
		}),
	],
});
