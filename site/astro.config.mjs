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
			social: [{ icon: 'github', label: 'GitHub', href: 'https://github.com/nrakochy/hotl' }],
			// Relative links are intentional: they stay correct when `base` changes hosts.
			plugins: [starlightLinksValidator({ errorOnRelativeLinks: false })],
			sidebar: [
				{ label: 'Quickstart', slug: 'quickstart' },
				{ label: 'Configuration', slug: 'configuration' },
				{ label: 'Permissions & sandbox', slug: 'permissions-and-sandbox' },
				{ label: 'The TUI console', slug: 'tui' },
				{ label: 'Backgrounding', slug: 'backgrounding' },
				{ label: 'Hooks', slug: 'hooks' },
				{ label: 'MCP servers', slug: 'mcp' },
				{ label: 'Gateways', slug: 'gateway' },
				{ label: 'Troubleshooting', slug: 'troubleshooting' },
				{ label: 'Uninstall', slug: 'uninstall' },
			],
		}),
	],
});
