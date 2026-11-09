#!/usr/bin/env node
/**
 * LOOM UI capture — screenshots + walkthrough GIFs for README/docs.
 *
 * Renders every admin-dashboard and client-UI page on a 2560×1440 canvas
 * and records scripted walkthroughs as animated GIFs.
 *
 * Usage:
 *   cd docs/scripts
 *   npm install
 *   npx playwright-core install chromium      # first run only
 *   LOOM_SESSION=<admin-session-token> node capture-ui.mjs
 *
 * Env:
 *   LOOM_ADMIN_URL   admin UI base   (default http://localhost:8080)
 *   LOOM_CLIENT_URL  client UI base  (default http://127.0.0.1:8391)
 *   LOOM_SESSION     loom_session cookie value for the admin UI
 *   LOOM_CLIENT_KEY  local api key for the client UI (loom_local_key)
 *   OUT              output dir      (default ../screenshots)
 *   ONLY             "stills" | "gifs" | unset = both
 *   GIF_WIDTH        export width    (default 1600 — keeps README size sane;
 *                                    frames are still captured at 1440p)
 */

import { chromium } from 'playwright-core';
import { createRequire } from 'module';
import sharp from 'sharp';
import fs from 'fs';
import path from 'path';
import { fileURLToPath } from 'url';

const { GIFEncoder, quantize, applyPalette } = createRequire(import.meta.url)('gifenc');

const ADMIN = process.env.LOOM_ADMIN_URL ?? 'http://localhost:8080';
const CLIENT = process.env.LOOM_CLIENT_URL ?? 'http://127.0.0.1:8391';
const TOK = process.env.LOOM_SESSION ?? '';
const CLIENT_KEY = process.env.LOOM_CLIENT_KEY ?? '';
const OUT = process.env.OUT ?? path.resolve(path.dirname(fileURLToPath(import.meta.url)), '../screenshots');
const ONLY = process.env.ONLY ?? '';
const GIF_W = parseInt(process.env.GIF_WIDTH ?? '1600', 10);

// 1440p canvas
const VP = { width: 2560, height: 1440 };
const FRAME_MS = 260;
const DELAY_CS = 20; // 200ms/frame → ~1.3× speed vs capture cadence

const adminPages = [
  ['overview', '/ui/'],
  ['monitoring', '/ui/monitoring'],
  ['scheduler', '/ui/scheduler'],
  ['models', '/ui/models'],
  ['deployments', '/ui/deployments'],
  ['clients', '/ui/clients'],
  ['keys', '/ui/keys'],
  ['api-users', '/ui/api-users'],
  ['routers', '/ui/routers'],
  ['notes', '/ui/notes'],
  ['settings', '/ui/settings'],
];
const clientPages = [
  ['status', '/ui/'],
  ['backends', '/ui/backends'],
  ['sharing', '/ui/sharing'],
  ['requests', '/ui/requests'],
  ['connection', '/ui/connection'],
];

async function makeGif(frames, file) {
  const gif = GIFEncoder();
  for (const buf of frames) {
    const { data, info } = await sharp(buf)
      .resize({ width: GIF_W })
      .ensureAlpha()
      .raw()
      .toBuffer({ resolveWithObject: true });
    const palette = quantize(data, 160, { format: 'rgba4444' });
    const index = applyPalette(data, palette, 'rgba4444');
    gif.writeFrame(index, info.width, info.height, { palette, delay: DELAY_CS * 10 });
  }
  gif.finish();
  fs.writeFileSync(file, gif.bytes());
  console.log(`${path.basename(file)}: ${(gif.bytes().length / 1e6).toFixed(2)} MB, ${frames.length} frames`);
}

async function shootStills(browser) {
  const ctx = await browser.newContext({ viewport: VP, deviceScaleFactor: 1, colorScheme: 'dark' });
  if (TOK) await ctx.addCookies([{ name: 'loom_session', value: TOK, url: ADMIN }]);
  if (CLIENT_KEY) await ctx.addInitScript(k => localStorage.setItem('loom_local_key', k), CLIENT_KEY);
  const page = await ctx.newPage();
  for (const [name, p] of adminPages) {
    await page.goto(ADMIN + p, { waitUntil: 'networkidle' }).catch(() => {});
    await page.waitForTimeout(1500);
    await page.screenshot({ path: `${OUT}/admin-${name}.png` });
    console.log('admin-' + name);
  }
  for (const [name, p] of clientPages) {
    await page.goto(CLIENT + p, { waitUntil: 'networkidle' }).catch(() => {});
    await page.waitForTimeout(1800);
    await page.screenshot({ path: `${OUT}/client-${name}.png` });
    console.log('client-' + name);
  }
  await ctx.close();
  // login page without session
  const ctx2 = await browser.newContext({ viewport: VP, deviceScaleFactor: 1, colorScheme: 'dark' });
  const p2 = await ctx2.newPage();
  await p2.goto(ADMIN + '/ui/login', { waitUntil: 'networkidle' }).catch(() => {});
  await p2.waitForTimeout(1200);
  await p2.screenshot({ path: `${OUT}/admin-login.png` });
  console.log('admin-login');
  await ctx2.close();
}

async function walk(browser, base, steps, file, cookie) {
  const ctx = await browser.newContext({ viewport: VP, deviceScaleFactor: 1, colorScheme: 'dark' });
  if (cookie) await ctx.addCookies([{ name: 'loom_session', value: TOK, url: base }]);
  if (CLIENT_KEY) await ctx.addInitScript(k => localStorage.setItem('loom_local_key', k), CLIENT_KEY);
  const page = await ctx.newPage();
  const frames = [];
  let shooting = true;
  let shotErrors = 0;
  const cap = (async () => {
    while (shooting) {
      try { frames.push(await page.screenshot()); }
      catch { shotErrors++; }
      try { await page.waitForTimeout(FRAME_MS); } catch { break; }
    }
    if (shotErrors) console.log(`  (${shotErrors} frame captures skipped mid-navigation)`);
  })();
  for (const s of steps) {
    try {
      if (s.goto) await page.goto(base + s.goto, { waitUntil: 'networkidle' }).catch(() => {});
      if (s.click) await page.getByRole('button', { name: s.click, exact: true }).first().click({ timeout: 1500 }).catch(() => {});
      if (s.scroll) await page.mouse.wheel(0, s.scroll);
      if (s.scrollTop) await page.evaluate(() => window.scrollTo({ top: 0, behavior: 'smooth' }));
      await page.waitForTimeout(s.wait ?? 1500);
    } catch {}
  }
  shooting = false;
  await cap;
  await makeGif(frames, file);
  await ctx.close();
}

const adminWalk = [
  { goto: '/ui/', wait: 2800 },
  { click: '7d', wait: 2600 },
  { click: 'All', wait: 2600 },
  { click: '24h', wait: 1600 },
  { scroll: 900, wait: 2200 },
  { scroll: 900, wait: 2200 },
  { goto: '/ui/monitoring', wait: 2400 },
  { click: 'Client', wait: 1800 },
  { click: 'API key', wait: 1800 },
  { scroll: 800, wait: 2000 },
  { goto: '/ui/models', wait: 2000 },
  { click: 'New model', wait: 2800 },
  { goto: '/ui/scheduler', wait: 2000 },
  { goto: '/ui/clients', wait: 2000 },
  { goto: '/ui/', wait: 1800 },
];
const clientWalk = [
  { goto: '/ui/', wait: 2800 },
  { scroll: 900, wait: 2200 },
  { scroll: 900, wait: 2200 },
  { goto: '/ui/backends', wait: 2200 },
  { goto: '/ui/sharing', wait: 2200 },
  { goto: '/ui/requests', wait: 2000 },
  { goto: '/ui/connection', wait: 2000 },
  { goto: '/ui/', wait: 1600 },
];

fs.mkdirSync(OUT, { recursive: true });
const browser = await chromium.launch();
if (ONLY !== 'gifs') await shootStills(browser);
if (ONLY !== 'stills') {
  await walk(browser, ADMIN, adminWalk, `${OUT}/walkthrough-admin.gif`, !!TOK);
  await walk(browser, CLIENT, clientWalk, `${OUT}/walkthrough-client.gif`, false);
}
await browser.close();
console.log('DONE →', OUT);
