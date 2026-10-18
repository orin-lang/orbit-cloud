// Drives the console in a real Chromium through every screen, with Playwright. Optional: the e2e script runs it when Playwright
// is available.
//
//   CONSOLE_URL=http://127.0.0.1:8081 CONSOLE_TOKEN=secret OCCTL=/path/to/occtl ORBIT_CLOUD_URL=... ORBIT_CLOUD_TOKEN=... \
//   PLAYWRIGHT=/path/to/node_modules/playwright CHROMIUM=/path/to/chrome node tools/browser_check.js [screenshot-dir]
//
// The apps `hello` (a job, two deployments, a KV namespace `cfg`) and the queue `orders` (with a consumer that fails, so a dead
// letter exists) must be there; tools/e2e.sh sets that up.
"use strict";
const { execFileSync } = require("child_process");
const { chromium } = require(process.env.PLAYWRIGHT || "playwright");
const assert = require("assert");

const base = process.env.CONSOLE_URL || "http://127.0.0.1:8081";
const shots = process.argv[2];
let step = 0;
async function shot(page, name) { if (shots) { await page.screenshot({ path: `${shots}/${String(++step).padStart(2, "0")}-${name}.png` }); } }
function occtl(...args) { return execFileSync(process.env.OCCTL || "occtl", args, { encoding: "utf8" }); }

(async () => {
  const browser = await chromium.launch({ executablePath: process.env.CHROMIUM || undefined, args: ["--no-sandbox"] });
  const page = await browser.newPage({ viewport: { width: 1100, height: 800 } });
  const problems = [];
  page.on("pageerror", e => problems.push("pageerror: " + e.message));
  page.on("console", m => { if (m.type() === "error" && !/favicon|404|401/.test(m.text())) { problems.push("console: " + m.text()); } });
  const ok = msg => console.log("ok: " + msg);

  // sign in through the form
  await page.goto(base + "/");
  assert.ok(page.url().endsWith("/login"), "redirected to the login form: " + page.url());
  await page.fill("input[name=token]", "wrong");
  await page.click("button:text('Sign in')");
  await page.waitForSelector("text=not right");
  await page.fill("input[name=token]", process.env.CONSOLE_TOKEN);
  await page.click("button:text('Sign in')");
  await page.waitForSelector("h2:text('Apps')");
  await shot(page, "apps");
  ok("login form, wrong token refused, apps list");

  // an app: deployments, run now, a live patch (no reload)
  await page.click("a:text-is('hello')");
  await page.waitForSelector("h2:text('Deployments')");
  const runsBefore = await page.locator("h2:text('Runs') + table tbody tr").count();
  await page.evaluate(() => { window.__marker = "same document"; });
  await page.click("button:text('Run now')");
  await page.waitForSelector(".flash:not(.hidden)");
  assert.match(await page.textContent(".flash"), /succeeded, exit 0/);
  await page.waitForFunction(n => document.querySelectorAll("h2 + table")[1].querySelectorAll("tbody tr").length === n, runsBefore + 1);
  assert.strictEqual(await page.evaluate(() => window.__marker), "same document", "the page was patched, not reloaded");
  await shot(page, "app-run-now");
  ok("run now patches the page in place");

  // activate the older deployment
  await page.click("button:text('Activate')");
  await page.waitForSelector("text=is active");
  await page.waitForFunction(() => document.querySelectorAll("button").length > 0 && [...document.querySelectorAll("button")].filter(b => b.textContent === "Activate").length === 1);
  ok("activate");

  // KV
  await page.click("button:has-text('cfg (')");
  await page.waitForSelector("td.mono:text('color')");
  await shot(page, "app-kv");
  await page.click("button:text('Close')");
  await page.waitForSelector("td.mono:text('color')", { state: "detached" });
  ok("kv namespace opens and closes");

  // a run with its trace and logs
  await page.click("a:text-matches('^[0-9a-f]{16}$') >> nth=0");
  await page.waitForSelector("h2:text('Trace')");
  assert.ok(await page.locator(".bar span").count() >= 3, "trace spans drawn");
  assert.ok(await page.locator(".logs .line").count() >= 1, "log lines");
  await shot(page, "run");
  ok("run page: waterfall and logs");

  // recent runs
  await page.click("nav a:text('Runs')");
  await page.waitForSelector("h2:text('Recent runs')");
  assert.ok(await page.locator("tbody tr").count() >= 2);
  ok("recent runs");

  // queues: dead letters, redrive, publish
  await page.click("nav a:text('Queues')");
  await page.waitForSelector("td:text-is('orders')");
  await page.click("button:text('Dead letters')");
  await page.waitForSelector("text=Dead letters of orders");
  await shot(page, "queues-dead");
  await page.fill("input[name=queue]", "orders");
  await page.fill("input[name=body]", '{"id": 99}');
  await page.click("button:text('Send')");
  await page.waitForSelector("text=Queued message");
  assert.strictEqual(await page.inputValue("input[name=body]"), "", "the form is cleared after sending");
  ok("dead letters and publish");
  await page.click("button:text('Redrive')");
  await page.waitForSelector("text=Redrove");
  ok("redrive");

  // live logs: lines produced after Follow arrive through the stream, without a reload
  await page.click("nav a:text('Logs')");
  await page.waitForSelector("button:text('Follow')");
  await page.click("button:text('Follow')");
  await page.waitForSelector(".logs.live .line");
  const before = await page.locator(".logs.live .line").count();
  occtl("run", "hello");
  await page.waitForFunction(n => document.querySelectorAll(".logs.live .line").length > n, before, { timeout: 10000 });
  await shot(page, "logs-live");
  ok("live logs: a new line arrived through the stream");
  // filters
  await page.fill("input[name=contains]", "no-such-text");
  await page.click("button:text('Apply')");
  await page.waitForFunction(() => document.querySelectorAll(".logs.live .line").length === 0);
  ok("log filter restarts the stream");

  // delete with confirmation
  await page.goto(base + "/apps/hello");
  await page.click("button:text('Delete app')");
  await page.waitForSelector("button:text('Yes, delete')");
  await page.click("button:text('Cancel')");
  await page.waitForSelector("button:text('Yes, delete')", { state: "detached" });
  await page.click("button:text('Delete app')");
  await page.click("button:text('Yes, delete')");
  await page.waitForSelector("h2:text('Deleted hello')");
  await shot(page, "deleted");
  ok("delete asks first, then deletes");

  // sign out
  await page.goto(base + "/");
  await page.click("button:text('Sign out')");
  await page.waitForSelector("input[name=token]");
  ok("sign out");

  await browser.close();
  assert.deepStrictEqual(problems, [], "browser errors: " + problems.join("; "));
  console.log("browser check passed");
})().catch(e => { console.error(e); process.exit(1); });
