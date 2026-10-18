// Drives the console in a real Chromium through every screen, with Playwright. Optional: the e2e script runs it when Playwright
// is available.
//
//   CONSOLE_URL=http://127.0.0.1:8081 CONSOLE_TOKEN=secret OCCTL=/path/to/occtl ORBIT_CLOUD_URL=... ORBIT_CLOUD_TOKEN=... \
//   PLAYWRIGHT=/path/to/node_modules/playwright CHROMIUM=/path/to/chrome node tools/browser_check.js [screenshot-dir]
//
// Needs the apps `hello` (a job with two versions and a KV namespace `cfg`) and the queue `orders` with a dead letter;
// tools/e2e.sh sets that up.
"use strict";
const { execFileSync } = require("child_process");
const { chromium } = require(process.env.PLAYWRIGHT || "playwright");
const assert = require("assert");

const base = process.env.CONSOLE_URL || "http://127.0.0.1:8081";
const shots = process.argv[2];
let step = 0;
async function shot(page, name) { if (shots) { await page.screenshot({ path: `${shots}/${String(++step).padStart(2, "0")}-${name}.png`, fullPage: true }); } }
function occtl(...args) { return execFileSync(process.env.OCCTL || "occtl", args, { encoding: "utf8" }); }

(async () => {
  const browser = await chromium.launch({ executablePath: process.env.CHROMIUM || undefined, args: ["--no-sandbox"] });
  const page = await browser.newPage({ viewport: { width: 1440, height: 900 } });
  const problems = [];
  page.on("pageerror", e => problems.push("pageerror: " + e.message));
  page.on("console", m => { if (m.type() === "error" && !/favicon|404|401/.test(m.text())) { problems.push("console: " + m.text()); } });
  const ok = msg => console.log("ok: " + msg);
  const text = sel => page.locator(sel).first().innerText();

  // sign in through the form
  await page.goto(base + "/");
  assert.ok(page.url().endsWith("/login"), "redirected to the login form: " + page.url());
  await page.fill("input[name=token]", "wrong");
  await page.click("button:has-text('Sign in')");
  await page.waitForSelector("text=not right");
  await page.fill("input[name=token]", process.env.CONSOLE_TOKEN);
  await page.click("button:has-text('Sign in')");
  await page.waitForSelector("h1:text('Overview')");
  ok("login form, wrong token refused, overview");

  // overview: stat cards and charts
  assert.strictEqual(await page.locator(".ou-stat").count(), 8, "eight stat cards");
  assert.ok(await page.locator("svg.ou-chart-svg[role=img]").count() >= 2, "two charts with an accessible role");
  assert.ok(await page.locator(".ou-chart-svg .ou-bar").count() > 0, "bars drawn");
  await page.locator(".ou-hit").nth(3).hover();
  await shot(page, "overview");
  ok("overview: stat cards and charts");

  // the sidebar navigates between full pages and marks the current one
  await page.click("nav a:has-text('Apps')");
  await page.waitForSelector("h1:text('Apps')");
  assert.ok((await page.locator("nav a.active").innerText()).includes("Apps"));
  await page.click("th button:has-text('Name')");
  await page.waitForSelector("th[aria-sort=ascending]");
  ok("apps table sorts in place");

  // an app: tabs, run now, a toast, a live patch (no reload)
  await page.click("a:text-is('hello')");
  await page.waitForSelector("h1:text('hello')");
  await page.evaluate(() => { window.__marker = "same document"; });
  await page.click("button:has-text('Run now')");
  await page.waitForSelector(".ou-toast");
  assert.match(await text(".ou-toast"), /succeeded, exit 0/);
  assert.strictEqual(await page.evaluate(() => window.__marker), "same document", "the page was patched, not reloaded");
  await page.click(".ou-toast .ou-icon-btn");
  await page.waitForSelector(".ou-toast", { state: "detached" });
  ok("run now patches the page in place, the toast can be dismissed");

  // deployments: spec groups, compare, activate with a confirmation, raw JSON
  await page.click("a[role=tab]:has-text('Deployments')");
  await page.waitForSelector("h2:text('Versions')");
  await page.waitForSelector("h3:text('Capabilities')");
  await page.click("button:has-text('Compare') >> nth=0");
  await page.waitForSelector("text=Changes: Version");
  await page.click("button[role=switch]");
  await page.waitForSelector("pre.ou-json");
  await page.click("button:has-text('Activate')");
  await page.waitForSelector("[role=dialog]:has-text('Make this version active?')");
  await page.keyboard.press("Escape");
  await page.waitForSelector("[role=dialog]", { state: "detached" });
  await page.click("button:has-text('Activate')");
  await page.click("[role=dialog] button:has-text('Activate version')");
  await page.waitForSelector("text=is active");
  await shot(page, "deployments");
  ok("deployments: compare, raw JSON, activate behind a confirm dialog, Escape closes it");

  // KV: namespaces, entries, a value
  await page.click("a[role=tab]:has-text('KV')");
  await page.click("button.ou-listbtn:has-text('cfg')");
  await page.waitForSelector("td .ou-mono:text('color')");
  await page.click("tr:has-text('color') button:has-text('View')");
  await page.waitForSelector("h2.ou-card-title:text-is('color')");
  ok("kv: namespace, entries and the value viewer");

  // a run: trace waterfall, resources, logs
  await page.goto(base + "/apps/hello/runs");
  await page.click("a.ou-mono >> nth=0");
  await page.waitForSelector("h2:text('Trace')");
  assert.ok(await page.locator(".wf-bar").count() >= 3, "trace spans drawn");
  assert.ok(await page.locator(".logline").count() >= 1, "log lines");
  assert.ok(await page.locator(".wf-tick").count() === 5, "a labelled time axis");
  await shot(page, "run");
  ok("run page: status, waterfall with an axis, logs");

  // runs and queues
  await page.click("nav a:has-text('Runs')");
  await page.waitForSelector("h1:text('Runs')");
  await page.click("nav a:has-text('Queues')");
  await page.waitForSelector("h2.ou-card-title:text-is('orders')");
  await page.click("button:has-text('Dead letters')");
  await page.waitForSelector("text=Dead letters of orders");
  await page.click("button:has-text('Publish message')");
  await page.waitForSelector("[role=dialog]:has-text('Publish a message')");
  await page.fill("[role=dialog] input[name=queue]", "orders");
  await page.fill("[role=dialog] textarea[name=body]", '{"id": 99}');
  await page.click("[role=dialog] button:has-text('Publish')");
  await page.waitForSelector("text=Queued message");
  await page.waitForSelector("[role=dialog]", { state: "detached" });
  await page.click("button:has-text('Redrive') >> nth=0");
  await page.waitForSelector("text=Redrove");
  await shot(page, "queues");
  ok("queues: dead letters, publish in a dialog, redrive");

  // live logs: lines produced after Follow arrive through the stream, without a reload
  await page.click("nav a:has-text('Logs')");
  await page.waitForSelector("button:has-text('Follow')");
  await page.click("button:has-text('Follow')");
  await page.waitForSelector(".logline");
  const before = await page.locator(".logline").count();
  occtl("run", "hello");
  await page.waitForFunction(n => document.querySelectorAll(".logline").length > n, before, { timeout: 10000 });
  await shot(page, "logs");
  ok("live logs: a new line arrived through the stream");
  await page.fill("input[name=contains]", "no-such-text");
  await page.click("button:has-text('Apply')");
  await page.waitForFunction(() => document.querySelectorAll(".logline").length === 0);
  await page.click("button:has-text('Pause')");
  await page.waitForSelector("button:has-text('Follow')");
  ok("log filter restarts the stream, pause stops it");

  // theme toggle: light -> dark -> system, remembered
  await page.click("[data-ou-theme-toggle]");
  assert.strictEqual(await page.evaluate(() => document.documentElement.getAttribute("data-theme")), "light");
  await page.click("[data-ou-theme-toggle]");
  assert.strictEqual(await page.evaluate(() => document.documentElement.getAttribute("data-theme")), "dark");
  await page.reload();
  assert.strictEqual(await page.evaluate(() => document.documentElement.getAttribute("data-theme")), "dark", "the choice is remembered");
  await page.click("[data-ou-theme-toggle]");
  ok("theme toggle");

  // delete: a typed-name confirmation
  await page.goto(base + "/apps/hello/settings");
  await page.click("button:has-text('Delete app')");
  await page.waitForSelector("[role=dialog]:has-text('Type hello to confirm')");
  await page.fill("[role=dialog] input[name=confirm]", "hallo");
  await page.click("[role=dialog] button:has-text('Delete app')");
  await page.waitForSelector("text=Type the app");
  await page.fill("[role=dialog] input[name=confirm]", "hello");
  await page.click("[role=dialog] button:has-text('Delete app')");
  await page.waitForSelector("h1:text('Deleted hello')");
  ok("delete needs the app's name typed");

  // sign out from the user menu
  await page.goto(base + "/");
  await page.click("summary.ou-menu-trigger");
  await page.click("button:has-text('Sign out')");
  await page.waitForSelector("input[name=token]");
  ok("sign out");

  await browser.close();
  assert.deepStrictEqual(problems, [], "browser errors: " + problems.join("; "));
  console.log("browser check passed");
})().catch(e => { console.error(e); process.exit(1); });
