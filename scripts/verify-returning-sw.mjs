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
  await page.setUserAgent(
    "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/131.0.0.0 Safari/537.36"
  );

  // 1. First visit to register SW
  await page.goto(`${BASE}/?key=${KEY}`, { waitUntil: "domcontentloaded", timeout: 30000 });
  await page.waitForFunction(
    () => document.getElementById("status-text")?.textContent === "Ready",
    { timeout: 25000 }
  );

  // 2. Reload page while SW is ALREADY active (triggers ScramjetController.init -> postMessage loadConfig!)
  await page.reload({ waitUntil: "domcontentloaded", timeout: 30000 });
  await page.waitForFunction(
    () => document.getElementById("status-text")?.textContent === "Ready",
    { timeout: 25000 }
  );

  // 3. Click Google directly from Home (exact screenshot repro!)
  await page.click('.quick-card[data-url="https://www.google.com"]');
  await page.waitForFunction(
    () => {
      const f = document.querySelector(".tab-frame-pane.active iframe");
      try {
        return f?.contentDocument?.title && f.contentDocument.title !== "Reconnecting";
      } catch {
        return false;
      }
    },
    { timeout: 25000 }
  );
  const gRes = await page.evaluate(() => {
    const f = document.querySelector(".tab-frame-pane.active iframe");
    return {
      status: document.getElementById("status-text")?.textContent,
      title: f?.contentDocument?.title,
      hasFail: Boolean(f?.contentDocument?.getElementById("dot-connection-failure")),
    };
  });
  console.log("1. RETURNING-SW GOOGLE:", JSON.stringify(gRes));

  // 4. Back to DOT PROXY Home -> click GitHub -> opens 2nd tab immediately!
  await page.click("#btn-home-tab");
  await page.click('.quick-card[data-url="https://github.com"]');
  await page.waitForFunction(
    () => {
      const f = document.querySelector(".tab-frame-pane.active iframe");
      try {
        return f?.contentDocument?.title && f.contentDocument.title.toLowerCase().includes("github");
      } catch {
        return false;
      }
    },
    { timeout: 25000 }
  );
  const ghRes = await page.evaluate(() => {
    const f = document.querySelector(".tab-frame-pane.active iframe");
    return {
      tabs: document.querySelectorAll(".tab-item").length,
      status: document.getElementById("status-text")?.textContent,
      title: f?.contentDocument?.title,
      hasFail: Boolean(f?.contentDocument?.getElementById("dot-connection-failure")),
    };
  });
  console.log("2. RETURNING-SW GITHUB:", JSON.stringify(ghRes));
} finally {
  await browser.close();
}
