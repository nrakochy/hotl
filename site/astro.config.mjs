// @ts-check
import { defineConfig } from 'astro/config';
import starlight from '@astrojs/starlight';
import starlightLinksValidator from 'starlight-links-validator';

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
			social: [{ icon: 'github', label: 'GitHub', href: 'https://github.com/nrakochy/hotl' }],
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
