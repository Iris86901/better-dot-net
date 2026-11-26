import puppeteer from "puppeteer-core";
import { getVerifyConfig } from "./load-env.mjs";

const { key: KEY, base: BASE, chromePath } = getVerifyConfig();

const browser = await puppeteer.launch({
  executablePath: chromePath,
  headless: "new",
  args: ["--no-sandbox", "--disable-gpu", "--disable-blink-features=AutomationControlled"],
});

try {
  const page = await browser.newPage();
  const ownErrors = [];
  page.on("pageerror", (e) => ownErrors.push("PAGEERR: " + e.message));
  page.on("response", (r) => {
    if (r.status() >= 400 && !r.url().includes("/p/")) {
      ownErrors.push("HTTP " + r.status() + " " + r.url());
    }
  });

  await page.goto(`${BASE}/?key=${KEY}`, { waitUntil: "domcontentloaded", timeout: 30000 });
  await page.waitForFunction(
    () => document.getElementById("meow-badge")?.textContent?.includes("v"),
    { timeout: 15000 }
  );
  const initial = await page.evaluate(() => ({
    meowBadge: document.getElementById("meow-badge").textContent,
    egressPill: document.getElementById("egress-pill-text").textContent,
    directActive: document.getElementById("btn-mode-direct").classList.contains("active"),
  }));
  console.log("1. ROUTING PANEL INITIAL:", JSON.stringify(initial));

  await page.click("#btn-check-exit");
  await page.waitForFunction(
    () => /\d+\.\d+\.\d+\.\d+/.test(document.getElementById("routing-exit-readout")?.textContent || ""),
    { timeout: 15000 }
  );
  const exitText = await page.evaluate(() => document.getElementById("routing-exit-readout").textContent);
  console.log("2. LIVE EXIT CHECK:", exitText);

  await page.click("#btn-toggle-drawer");
  const yamlSample = `proxies:
  - name: "Clash-SOCKS"
    type: socks5
    server: 1.2.3.4
    port: 1080
  - name: "Clash-Trojan"
    type: trojan
    server: 1.2.3.4
    port: 443
    password: "demo"
    sni: example.com`;
  await page.evaluate((val) => {
    document.getElementById("routing-config-input").value = val;
  }, yamlSample);
  await page.click("#btn-routing-apply");
  await page.waitForFunction(
    () => document.getElementById("btn-mode-upstream").classList.contains("active"),
    { timeout: 15000 }
  );
  const afterApply = await page.evaluate(() => ({
    egressPill: document.getElementById("egress-pill-text").textContent,
    nodeOptions: Array.from(document.querySelectorAll("#routing-node-select option")).map((o) => o.value),
    msg: document.getElementById("routing-msg").textContent,
  }));
  console.log("3. AFTER CLASH YAML APPLY:", JSON.stringify(afterApply));
  if (afterApply.nodeOptions.length !== 2) throw new Error("Expected 2 Clash nodes");

  await page.click("#btn-mode-direct");
  await page.waitForFunction(
    () => document.getElementById("btn-mode-direct").classList.contains("active"),
    { timeout: 15000 }
  );
  console.log("4. SWITCHED BACK TO DIRECT:", await page.evaluate(() => document.getElementById("egress-pill-text").textContent));

  await page.click("#btn-routing-clear");
  await page.waitForFunction(
    () => document.getElementById("routing-node-row").classList.contains("hidden"),
    { timeout: 15000 }
  );
  console.log("5. CLEARED UPSTREAM OK");
  console.log("OWN ERRORS:", JSON.stringify(ownErrors));
  if (ownErrors.length > 0) throw new Error("Own errors detected: " + ownErrors.join("; "));
} finally {
  await browser.close();
}
