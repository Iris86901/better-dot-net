import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import vm from "node:vm";

const source = readFileSync(new URL("../web/app.js", import.meta.url), "utf-8").replace(/\r\n/g, "\n");
const actualHelper = source.slice(0, source.indexOf("(async () => {\n  const homeTabBtn"));
const makeReconnectCoordinator = vm.runInNewContext(actualHelper + "\nmakeReconnectCoordinator");
const normalizeTabSession = vm.runInNewContext(actualHelper + "\nnormalizeTabSession", { URL });
const destinationHistoryUrl = vm.runInNewContext(actualHelper + "\ndestinationHistoryUrl", { URL });

let phases = [];
let restored = [];
let errors = [];
let release;
let active = { url: "https://old.example/" };
let calls = 0;
const reconnect = makeReconnectCoordinator(
  () => { calls++; return new Promise(r => { release = r; }); },
  () => active,
  phase => phases.push(phase),
  tab => restored.push(tab.url),
  error => errors.push(error.message)
);
const first = reconnect();
const second = reconnect();
assert.equal(first, second, "simultaneous clicks should share one operation");
await Promise.resolve();
assert.equal(calls, 1);
assert.deepEqual(phases, ["connecting"]);
active = { url: "https://current.example/" };
release();
await first;
assert.deepEqual(phases, ["connecting", "restoring"], "transport readiness does not make a page ready");
assert.deepEqual(restored, ["https://current.example/"], "do not reload a background tab");
assert.deepEqual(errors, []);

phases = [];
const afterClose = reconnect();
await Promise.resolve();
active = null;
release();
await afterClose;
assert.deepEqual(phases, ["connecting", "ready"]);
assert.equal(calls, 2, "new click after reset must start a new operation");

phases = [];
const retry = makeReconnectCoordinator(
  async () => { throw Error("fixture failure"); },
  () => { throw Error("must not try to access an active tab"); },
  phase => phases.push(phase),
  () => { throw Error("must not restore a page"); },
  error => errors.push(error.message)
);
await retry();
await retry();
assert.deepEqual(phases, ["connecting", "connecting"]);
assert.deepEqual(errors, ["fixture failure", "fixture failure"]);
console.log("PASS: reconnect coalescing, current-tab recovery, startup failure and retry");

const article = "https://reader.example/articles?q=a#p2";
const later = "https://reader.example/articles/next";
const snapshot = normalizeTabSession({ active: 5, tabs: [
  { id: 5, url: article, title: "Article", historyIndex: 1,
    history: [{url: "javascript:alert(1)"}, {url: article, scrollY: 40}, {url: later}] },
  { id: 6, url: "https://private:password@reader.example/" },
  { id: 5, url: later },
  { id: 8, url: "https://background.example/" },
] });
assert.equal(snapshot.active, 5);
assert.equal(snapshot.tabs.length, 2);
assert.equal(snapshot.tabs[0].url, article);
assert.equal(snapshot.tabs[0].historyIndex, 0);
assert.equal(snapshot.tabs[0].history[0].scrollY, 40);
assert.equal(snapshot.tabs[0].history[1].url, later);
assert.equal(normalizeTabSession(null).active, null);
assert.equal(normalizeTabSession({active: 1, tabs: Array.from({length: 22}, (_, i) => ({
  id: i + 1, url: article
}))}).tabs.length, 16);
console.log("PASS: bounded local tab/session restore with safe URLs and back/forward history");

const shell = "https://proxy.example";
assert.equal(destinationHistoryUrl(shell + "/?key=fixture-only", shell), null);
assert.equal(destinationHistoryUrl(shell + "/p/encoded", shell), null);
assert.equal(destinationHistoryUrl("https://github.com/features", shell), "https://github.com/features");
const cleaned = normalizeTabSession({active: 4, tabs:[
  {id: 3, url: shell + "/"},
  {id: 4, url: later, historyIndex: 2, history:[
    {url: article}, {url: shell + "/?key=fixture-only"}, {url: later}, {url: shell + "/p/encoded"}
  ]}
]}, shell);
assert.equal(cleaned.tabs.length, 1);
assert.equal(cleaned.tabs[0].historyIndex, 1);
assert.deepEqual(Array.from(cleaned.tabs[0].history, entry => entry.url), [article, later]);

const stepSource = source.slice(source.indexOf("  function stepHistory"), source.indexOf("  function goBack"));
let pendingTimer;
let nativeBacks = 0;
const loaded = [];
const win = {location: {href: later}, history: {length: 2, back: () => { nativeBacks++; }}};
const liveTab = {id: 7, url: later, historyIndex: 1,
  history: [{url: article, scrollY: 65}, {url: later}],
  frame: {frame: {contentWindow: win}, go: url => loaded.push(url)}};
const stepHistory = vm.runInNewContext(actualHelper + "\n" + stepSource + "\nstepHistory", {
  URL, location: {origin: shell}, activeTabId: 7, tabs: new Map([[7, liveTab]]),
  addrInput: {}, hostLabel: url => new URL(url).hostname, saveTabScroll() {}, syncActiveView() {}, sameOrigin: () => true,
  decodeDestinationUrl: url => url, clearTimeout() { pendingTimer = null; },
  setTimeout(fn) { pendingTimer = fn; return 1; },
});
stepHistory(-1);
assert.equal(nativeBacks, 1);
win.location.href = shell + "/";
pendingTimer();
assert.deepEqual(loaded, [article], "escaping iframe to the proxy shell must fall back to the intended destination");
assert.equal(liveTab.restoreScroll.y, 65);
assert.equal(liveTab.expectedHistoryTarget, null);
console.log("PASS: toolbar back rejects proxy shell history and reloads the intended destination");

// Verify Service Worker escaped same-origin navigation/form recovery & orphan /p/ redirect
const swSource = readFileSync(new URL("../web/sw.js", import.meta.url), "utf-8").replace(/\r\n/g, "\n");
const swHelper = swSource.slice(
  swSource.indexOf("const SHELL_EXACT_PATHS"),
  swSource.indexOf("self.addEventListener('fetch'")
);
const { resolveEscapedTarget, resolveOrphanedProxyNavigation } = vm.runInNewContext(
  swHelper + "\n({ resolveEscapedTarget, resolveOrphanedProxyNavigation })",
  {
    URL,
    self: {
      location: { origin: shell },
      clients: {
        async get(id) {
          if (id === "gh-frame") return { url: shell + "/p/ctrl1/frm1/" + encodeURIComponent("https://github.com/") };
          if (id === "shell-top") return { url: shell + "/" };
          return null;
        },
      },
    },
  }
);

const ghRef = shell + "/p/ctrl1/frm1/" + encodeURIComponent("https://github.com/explore");
assert.equal(
  await resolveEscapedTarget(
    { request: { referrer: ghRef, mode: "navigate" }, clientId: "" },
    new URL(shell + "/search?q=dot&type=repositories")
  ),
  shell + "/p/ctrl1/frm1/" + encodeURIComponent("https://github.com/search?q=dot&type=repositories"),
  "GitHub /search form submit escaping /p/<cid>/<fid>/ via referrer must redirect back into the same frame"
);
assert.equal(
  await resolveEscapedTarget(
    { request: { referrer: "", mode: "navigate" }, clientId: "gh-frame" },
    new URL(shell + "/search?q=dot")
  ),
  shell + "/p/ctrl1/frm1/" + encodeURIComponent("https://github.com/search?q=dot"),
  "Escaped navigation must still recover via clientId even when Referrer-Policy strips referrer"
);
assert.equal(
  await resolveEscapedTarget(
    { request: { referrer: "", mode: "navigate" }, clientId: "gh-frame" },
    new URL(shell + "/")
  ),
  shell + "/p/ctrl1/frm1/" + encodeURIComponent("https://github.com/"),
  "Escaped root / navigation from a proxied iframe must stay inside target origin"
);
assert.equal(
  resolveOrphanedProxyNavigation(new URL(shell + "/p/oldctrl/oldfrm/" + encodeURIComponent("https://github.com/search?q=dot"))),
  shell + "/?goto=" + encodeURIComponent("https://github.com/search?q=dot"),
  "Orphaned /p/ top-level navigation must hand back to shell with ?goto="
);
assert.equal(
  await resolveEscapedTarget(
    { request: { referrer: shell + "/", mode: "navigate" }, clientId: "shell-top" },
    new URL(shell + "/api/routing")
  ),
  null,
  "Top-level shell requests and gateway APIs must never be hijacked"
);
assert.equal(
  await resolveEscapedTarget(
    { request: { referrer: ghRef, mode: "cors" }, clientId: "gh-frame" },
    new URL(shell + "/api/routing")
  ),
  null,
  "Gateway API paths must never be rewritten"
);
console.log("PASS: Service Worker recovers escaped root-relative navigations & orphaned /p/ links safely");
