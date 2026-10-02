import { chromium } from 'playwright';
import { writeFileSync } from 'fs';
const [,, scheme = 'light', which = 'thread'] = process.argv;
if (which === 'thread' && !process.env.THREAD) throw new Error('set THREAD to a thread id');
const browser = await chromium.launch();
const ctx = await browser.newContext({ viewport: { width: 1440, height: 900 }, deviceScaleFactor: 1, colorScheme: scheme, storageState: 'state.json' });
const page = await ctx.newPage();
const base = process.env.ZENITH_URL ?? 'http://127.0.0.1:4747';
const env = await (await fetch(`${base}/.well-known/t3/environment`)).json();
await page.goto(which === 'thread' ? `${base}/${env.environmentId}/${process.env.THREAD}` : `${base}/`);
await page.waitForTimeout(4000);
if (which === 'thread') await page.waitForTimeout(4000);
const out = await page.evaluate(() => {
  const rows = [];
  const root = getComputedStyle(document.documentElement);
  const vars = {};
  for (const sheet of document.styleSheets) { try { for (const r of sheet.cssRules) { if (r.selectorText === ':root' || r.selectorText?.includes('.dark')) { for (const p of r.style) if (p.startsWith('--')) vars[r.selectorText + ' ' + p] = r.style.getPropertyValue(p).trim(); } } } catch {} }
  for (const el of document.querySelectorAll('body *')) {
    const r = el.getBoundingClientRect();
    if (r.width < 1 || r.height < 1 || r.bottom < 0 || r.top > 900 || r.right < 0 || r.left > 1440) continue;
    const s = getComputedStyle(el);
    if (s.visibility === 'hidden' || s.display === 'none') continue;
    const own = [...el.childNodes].filter(n => n.nodeType === 3).map(n => n.textContent.trim()).join(' ').slice(0, 60);
    const bg = s.backgroundColor !== 'rgba(0, 0, 0, 0)' ? s.backgroundColor : '';
    const bimg = s.backgroundImage !== 'none' ? s.backgroundImage.slice(0, 200) : '';
    const border = ['Top','Right','Bottom','Left'].map(k => s['border'+k+'Width'] !== '0px' ? `${k[0]}:${s['border'+k+'Width']} ${s['border'+k+'Color']}` : '').filter(Boolean).join(' ');
    const interesting = own || bg || bimg || border || s.boxShadow !== 'none' || el.tagName === 'svg' || el.tagName === 'IMG';
    if (!interesting) continue;
    rows.push({
      tag: el.tagName.toLowerCase(), cls: (el.getAttribute('class') || '').slice(0, 300),
      x: Math.round(r.x*10)/10, y: Math.round(r.y*10)/10, w: Math.round(r.width*10)/10, h: Math.round(r.height*10)/10,
      text: own, font: own ? `${s.fontFamily.split(',')[0]} ${s.fontSize}/${s.lineHeight} w${s.fontWeight} ls${s.letterSpacing}` : '',
      color: own || el.tagName === 'svg' ? s.color : '', bg, bimg, border, radius: s.borderRadius !== '0px' ? s.borderRadius : '',
      shadow: s.boxShadow !== 'none' ? s.boxShadow : '', opacity: s.opacity !== '1' ? s.opacity : '',
      pad: s.padding !== '0px' ? s.padding : '', backdrop: s.backdropFilter !== 'none' ? s.backdropFilter : '',
      svgStroke: el.tagName === 'svg' ? `${s.strokeWidth}` : '',
    });
  }
  return { vars, rows, bodyFont: getComputedStyle(document.body).fontFamily, bodyBg: getComputedStyle(document.body).backgroundColor };
});
writeFileSync(`dump-${which}-${scheme}.json`, JSON.stringify(out, null, 1));
console.log(out.rows.length, 'rows', Object.keys(out.vars).length, 'vars', out.bodyFont);
await browser.close();
