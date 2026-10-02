import { chromium } from 'playwright';
const browser = await chromium.launch();
for (const scheme of ['light', 'dark']) {
  const ctx = await browser.newContext({ viewport: { width: 1440, height: 900 }, colorScheme: scheme, storageState: 'state.json' });
  const page = await ctx.newPage();
  await page.goto((process.env.ZENITH_URL ?? 'http://127.0.0.1:4747') + '/'); await page.waitForTimeout(3000);
  // Keep the page's own backdrop and the sidebar's own glass, drop everything drawn on top.
  await page.evaluate(() => {
    const root = document.getElementById('root') || document.body.firstElementChild;
    const glass = document.createElement('div');
    const inner = document.querySelector('[data-app-sidebar] [data-slot="sidebar-inner"]');
    const s = getComputedStyle(inner);
    glass.style.cssText = `position:fixed;inset:0;background:${s.background};backdrop-filter:${s.backdropFilter};-webkit-backdrop-filter:${s.backdropFilter};z-index:2147483647`;
    root.style.visibility = 'hidden';
    document.body.appendChild(glass);
    window.__info = { bg: s.backgroundColor, filter: s.backdropFilter };
  });
  await page.waitForTimeout(500);
  console.log(scheme, await page.evaluate(() => JSON.stringify(window.__info)));
  await page.screenshot({ path: `material-${scheme}-full.png` });
  await ctx.close();
}
await browser.close();
