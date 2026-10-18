// Screenshots of every screen of the console, in light and dark mode, with Playwright. Used by demo.sh --shots DIR.
//
//   CONSOLE_URL=http://127.0.0.1:8081 CONSOLE_TOKEN=demo [PLAYWRIGHT=...] [CHROMIUM=...] node tools/screenshots.js DIR
"use strict";
const { chromium } = require(process.env.PLAYWRIGHT || "playwright");
const out = process.argv[2];
const base = process.env.CONSOLE_URL || "http://127.0.0.1:8081";
const orbitd = process.env.ORBIT_CLOUD_URL || "http://127.0.0.1:8080";
const token = process.env.ORBIT_CLOUD_TOKEN || "demo";

async function api(path) { const r = await fetch(orbitd + "/v1" + path, { headers: { Authorization: "Bearer " + token } }); return r.json(); }

async function settle(page) {
  await page.waitForLoadState("networkidle");
  await page.waitForTimeout(250);
}

(async () => {
  const browser = await chromium.launch({ executablePath: process.env.CHROMIUM || undefined, args: ["--no-sandbox"] });
  const run = (await api("/apps/etl/runs?limit=1"))[0];
  const failedRun = (await api("/runs?limit=200")).find(r => r.status === "failed" && r.app === "sync-upstream");
  const etl = await api("/apps/etl");
  const olderDeployment = etl.deployments[0].id;
  const steps = [
    ["overview", "/", null],
    ["apps", "/apps", null],
    ["app-overview", "/apps/etl", null],
    ["app-deployments", "/apps/etl/deployments", null],
    ["app-deployments-diff", "/apps/etl/deployments", async p => { await p.click("button:has-text('Compare') >> nth=0"); await settle(p); }],
    ["app-activate-confirm", "/apps/etl/deployments", async p => { await p.click("button:has-text('Activate')"); await settle(p); }],
    ["app-runs", "/apps/etl/runs", null],
    ["app-logs", "/apps/etl/logs", async p => { await p.click("button:has-text('Follow')"); await p.waitForSelector(".logline"); await settle(p); }],
    ["app-kv", "/apps/etl/kv?ns=cfg", async p => { await p.click("tr:has-text('settings') button:has-text('View')"); await settle(p); }],
    ["app-settings", "/apps/etl/settings", null],
    ["app-delete-confirm", "/apps/etl/settings", async p => { await p.click("button:has-text('Delete app')"); await settle(p); }],
    ["runs", "/runs", null],
    ["run-detail", "/runs/" + run.id, null],
    ["run-failed", "/runs/" + failedRun.id, null],
    ["logs", "/logs", async p => { await p.click("button:has-text('Follow')"); await p.waitForSelector(".logline"); await settle(p); }],
    ["queues", "/queues", null],
    ["queues-dead-letters", "/queues", async p => { await p.click("button:has-text('Dead letters') >> nth=0"); await settle(p); }],
    ["queues-publish", "/queues", async p => { await p.click("button:has-text('Publish message')"); await settle(p); }],
    ["kv", "/kv", null],
    ["settings", "/settings", null],
    ["not-found", "/apps/no-such-app", null],
  ];
  let n = 0;
  for (const scheme of ["light", "dark"]) {
    const ctx = await browser.newContext({ viewport: { width: 1440, height: 900 }, colorScheme: scheme });
    const page = await ctx.newPage();
    const errors = [];
    page.on("pageerror", e => errors.push(e.message));
    await page.goto(base + "/login");
    await page.fill("input[name=token]", process.env.CONSOLE_TOKEN || "demo");
    await page.click("button:has-text('Sign in')");
    await page.waitForSelector(".ou-shell");
    n = 0;
    for (const [name, path, action] of steps) {
      await page.goto(base + path);
      await settle(page);
      if (action) { await action(page); }
      n += 1;
      await page.screenshot({ path: `${out}/${String(n).padStart(2, "0")}-${name}-${scheme}.png`, fullPage: true });
    }
    // a tablet width
    await page.setViewportSize({ width: 768, height: 1024 });
    for (const [name, path] of [["tablet-overview", "/"], ["tablet-app", "/apps/etl/deployments"], ["tablet-run", "/runs/" + run.id]]) {
      await page.goto(base + path);
      await settle(page);
      n += 1;
      await page.screenshot({ path: `${out}/${String(n).padStart(2, "0")}-${name}-${scheme}.png`, fullPage: true });
    }
    if (errors.length) { console.error("page errors in " + scheme + ": " + errors.join("; ")); process.exitCode = 1; }
    await ctx.close();
  }
  await browser.close();
  console.log("screenshots in " + out);
})().catch(e => { console.error(e); process.exit(1); });
