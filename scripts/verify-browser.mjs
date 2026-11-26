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
  const ownErrors = [];
  page.on("pageerror", (e) => ownErrors.push("PAGEERR: " + e.message));
  page.on("response", (r) => {
    if (r.status() >= 400 && !r.url().includes("/p/")) {
      ownErrors.push("HTTP " + r.status() + " " + r.url());
    }
  });

  // 1. Open Home (pinned DOT PROXY tab active, 0 website tabs, Back/Fwd disabled)
  await page.goto(`${BASE}/?key=${KEY}`, { waitUntil: "domcontentloaded", timeout: 30000 });
  await page.waitForFunction(
    () => document.getElementById("status-text")?.textContent === "Ready",
    { timeout: 25000 }
  );
  const s1 = await page.evaluate(() => ({
    homeActive: document.getElementById("btn-home-tab").classList.contains("active"),
    tabs: document.querySelectorAll(".tab-item").length,
    backDisabled: document.getElementById("btn-back").disabled,
    fwdDisabled: document.getElementById("btn-fwd").disabled,
  }));
  console.log("1. PINNED HOME:", JSON.stringify(s1));

  // 2. From Home open example.com -> spawns Tab 1; then navigate inside Tab 1 to api.ipify.org
  await page.evaluate(() => {
    document.getElementById("hero-input").value = "https://example.com";
    document.getElementById("hero-form").dispatchEvent(new Event("submit", { cancelable: true }));
  });
  await page.waitForFunction(
    () => document.querySelector(".tab-frame-pane.active iframe")?.contentDocument?.title === "Example Domain",
    { timeout: 25000 }
  );
  await page.evaluate(() => {
    const f = document.querySelector(".tab-frame-pane.active iframe");
    f.contentDocument.cookie = "back_test=ok_777; path=/; max-age=86400";
    f.contentWindow.localStorage.setItem("back_ls", "ls_777");
  });

  // Navigate Tab 1 to api.ipify.org
  await page.evaluate(() => {
    document.getElementById("addr-input").value = "https://api.ipify.org";
    document.getElementById("addr-form").dispatchEvent(new Event("submit", { cancelable: true }));
  });
  await page.waitForFunction(
    () => /\d+\.\d+\.\d+\.\d+/.test(document.querySelector(".tab-frame-pane.active iframe")?.contentDocument?.body?.innerText || ""),
    { timeout: 25000 }
  );
  const ipNow = await page.evaluate(() => ({
    ip: document.querySelector(".tab-frame-pane.active iframe").contentDocument.body.innerText.trim(),
    backDisabled: document.getElementById("btn-back").disabled,
    fwdDisabled: document.getElementById("btn-fwd").disabled,
  }));
  console.log("2. TAB 1 NAVIGATED TO IP:", JSON.stringify(ipNow));

  // 3. Click Back -> MUST return cleanly to Example Domain (NO Connection interrupted!)
  await page.click("#btn-back");
  await page.waitForFunction(
    () => document.querySelector(".tab-frame-pane.active iframe")?.contentDocument?.title === "Example Domain",
    { timeout: 25000 }
  );
  const afterBack = await page.evaluate(() => {
    const f = document.querySelector(".tab-frame-pane.active iframe");
    return {
      title: f.contentDocument.title,
      hasFailure: Boolean(f.contentDocument.getElementById("dot-connection-failure")),
      cookie: f.contentDocument.cookie,
      ls: f.contentWindow.localStorage.getItem("back_ls"),
      backDisabled: document.getElementById("btn-back").disabled,
      fwdDisabled: document.getElementById("btn-fwd").disabled,
    };
  });
  console.log("3. AFTER BACK:", JSON.stringify(afterBack));

  // 4. Click Forward -> MUST return cleanly to api.ipify.org
  await page.click("#btn-fwd");
  await page.waitForFunction(
    () => /\d+\.\d+\.\d+\.\d+/.test(document.querySelector(".tab-frame-pane.active iframe")?.contentDocument?.body?.innerText || ""),
    { timeout: 25000 }
  );
  const afterFwd = await page.evaluate(() => ({
    ip: document.querySelector(".tab-frame-pane.active iframe").contentDocument.body.innerText.trim(),
    backDisabled: document.getElementById("btn-back").disabled,
    fwdDisabled: document.getElementById("btn-fwd").disabled,
  }));
  console.log("4. AFTER FORWARD:", JSON.stringify(afterFwd));

  // 5. Click pinned DOT PROXY Home -> open X from Home -> MUST create Tab 2 while keeping Tab 1 alive!
  await page.click("#btn-home-tab");
  await page.click('.quick-card[data-url="https://x.com"]');
  await new Promise((r) => setTimeout(r, 12000));
  const tab2Check = await page.evaluate(() => {
    const f = document.querySelector(".tab-frame-pane.active iframe");
    return {
      tabs: document.querySelectorAll(".tab-item").length,
      titles: [...document.querySelectorAll(".tab-title")].map((e) => e.textContent),
      xTitle: f?.contentDocument?.title || "",
    };
  });
  console.log("5. HOME -> NEW TAB (x.com):", JSON.stringify(tab2Check));

  // 6. Test REAL MULTI-WINDOW: open a SECOND real Chrome browser tab to BASE simultaneously!
  const page2 = await browser.newPage();
  await page2.setUserAgent(
    "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/131.0.0.0 Safari/537.36"
  );
  await page2.goto(`${BASE}/?goto=https://example.com`, { waitUntil: "domcontentloaded", timeout: 30000 });
  await page2.waitForFunction(
    () => document.querySelector(".tab-frame-pane.active iframe")?.contentDocument?.title === "Example Domain",
    { timeout: 25000 }
  );
  const win2 = await page2.evaluate(() => {
    const f = document.querySelector(".tab-frame-pane.active iframe");
    return {
      title: f.contentDocument.title,
      sharedCookie: f.contentDocument.cookie,
      sharedLs: f.contentWindow.localStorage.getItem("back_ls"),
    };
  });
  // Verify Window 1 STILL works immediately (NOT disconnected by Window 2!)
  await page.bringToFront();
  await page.click(".tab-item:first-child");
  await page.click("#btn-reload");
  await page.waitForFunction(
    () => /\d+\.\d+\.\d+\.\d+/.test(document.querySelector(".tab-frame-pane.active iframe")?.contentDocument?.body?.innerText || ""),
    { timeout: 25000 }
  );
  const win1StillAlive = await page.evaluate(() =>
    document.querySelector(".tab-frame-pane.active iframe").contentDocument.body.innerText.trim()
  );
  console.log("6. REAL MULTI-WINDOW:", JSON.stringify({ win2, win1StillAlive }));
  console.log("7. OWN ORIGIN ERRORS:", ownErrors);
} finally {
  await browser.close();
}
