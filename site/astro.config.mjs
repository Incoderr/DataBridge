// @ts-check
import { defineConfig } from 'astro/config';

// https://astro.build/config/
export default defineConfig({
    // Served from the domain root on Cloudflare Pages (https://<project>.pages.dev/),
    // so no `base` is needed. The GitHub Pages setup used `site` + `base: '/DataBridge/'`;
    // re-add both if the site is ever served from a sub-path again.
});
