// @ts-check
import { defineConfig } from 'astro/config';

// https://astro.build/config/
export default defineConfig({
    // Served from the GitHub Pages project path, not the domain root.
    site: 'https://incoderr.github.io',
    base: '/DataBridge/',
});
