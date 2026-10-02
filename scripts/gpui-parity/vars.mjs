import { chromium } from 'playwright';
import { writeFileSync } from 'fs';
const browser = await chromium.launch();
const result = {};
for (const scheme of ['light', 'dark']) {
  const ctx = await browser.newContext({ viewport: { width: 1440, height: 900 }, colorScheme: scheme, storageState: 'state.json' });
  const page = await ctx.newPage();
  await page.goto((process.env.ZENITH_URL ?? 'http://127.0.0.1:4747') + '/'); await page.waitForTimeout(3000);
  result[scheme] = await page.evaluate(() => {
    const names = new Set();
    const walk = (rules) => { for (const r of rules) { if (r.style) for (const p of r.style) if (p.startsWith('--')) names.add(p); if (r.cssRules) walk(r.cssRules); } };
    for (const s of document.styleSheets) { try { walk(s.cssRules); } catch {} }
    const cs = getComputedStyle(document.documentElement);
    const probe = document.createElement('div'); document.body.appendChild(probe);
    const out = {};
    for (const n of [...names].sort()) {
      const raw = cs.getPropertyValue(n).trim();
      if (!raw) continue;
      probe.style.color = ''; probe.style.color = `var(${n})`;
      const col = probe.style.color && getComputedStyle(probe).color;
      out[n] = { raw: raw.slice(0, 160), color: col && /^(rgb|oklab|oklch|color|lab|lch|hsl)/.test(raw.replace(/^var.*/, 'x')) || /^(#|rgb|oklab|oklch|color\(|hsl|color-mix)/.test(raw) ? col : null };
    }
    return { dark: document.documentElement.classList.contains('dark'), html: document.documentElement.outerHTML.slice(0, 200), vars: out };
  });
  await ctx.close();
}
writeFileSync('vars.json', JSON.stringify(result, null, 1));
console.log(result.light.html, '\n', result.dark.html, Object.keys(result.light.vars).length);
await browser.close();
