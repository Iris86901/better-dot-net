"use strict";

// Coalesce manual reconnect clicks and resolve the current visible tab only
// after the replacement transport is ready. Page readiness is a separate event.
function makeReconnectCoordinator(transport, currentTab, showPhase, restore, showError) {
  let flight = null;
  return function reconnect() {
    if (flight) return flight;
    flight = Promise.resolve().then(async () => {
      showPhase("connecting");
      try {
        await transport();
        const tab = currentTab();
        if (tab?.url) {
          showPhase("restoring");
          restore(tab);
        } else {
          showPhase("ready");
        }
      } catch (error) {
        showError(error);
      }
    }).finally(() => { flight = null; });
    return flight;
  };
}

const TAB_SESSION_KEY = "bdn_tab_session_v1";

function destinationHistoryUrl(value, shellOrigin) {
  if (typeof value !== "string" || value.length > 4096) return null;
  try {
    const url = new URL(value);
    return /^https?:$/.test(url.protocol) && !url.username && !url.password &&
      url.origin !== shellOrigin && url.href.length <= 4096 ? url.href : null;
  } catch { return null; }
}

// Session storage belongs to this outer browser tab and survives reload only.
// Cache a bounded slice of navigation state; never serialize form or frame data.
function normalizeTabSession(data, shellOrigin) {
  const safeUrl = value => destinationHistoryUrl(value, shellOrigin);
  const restored = [];
  if (!Array.isArray(data?.tabs)) return { active: null, tabs: restored };
  const ids = new Set();
  for (const raw of data.tabs.slice(-16)) {
    if (!Number.isSafeInteger(raw?.id) || raw.id < 1 || ids.has(raw.id)) continue;
    const url = safeUrl(raw.url);
    if (!url) continue;
    const rawHistory = Array.isArray(raw.history) ? raw.history : [];
    const at = Number.isSafeInteger(raw.historyIndex) ? raw.historyIndex : -1;
    const start = Math.max(0, at - 12);
    const entries = [];
    let index = -1;
    for (let i = start; i < Math.min(rawHistory.length, at + 7); i++) {
      const entry = rawHistory[i];
      const href = safeUrl(entry?.url);
      if (!href) continue;
      if (i === at && href === url) index = entries.length;
      entries.push({
        url: href,
        scrollX: Number.isFinite(entry.scrollX) ? entry.scrollX : 0,
        scrollY: Number.isFinite(entry.scrollY) ? entry.scrollY : 0,
      });
    }
    ids.add(raw.id);
    restored.push({
      id: raw.id,
      url,
      title: typeof raw.title === "string" ? raw.title.slice(0, 48) : "",
      history: index < 0 ? [{ url, scrollX: 0, scrollY: 0 }] : entries,
      historyIndex: Math.max(0, index),
    });
  }
  return { active: ids.has(data.active) ? data.active : null, tabs: restored };
}

(async () => {
  const homeTabBtn = document.getElementById("btn-home-tab");
  const tabsList = document.getElementById("tabs-list");
  const newTabBtn = document.getElementById("btn-new-tab");
  const addrForm = document.getElementById("addr-form");
  const addrInput = document.getElementById("addr-input");
  const lockIcon = addrForm?.querySelector(".lock-icon");
  const heroForm = document.getElementById("hero-form");
  const heroInput = document.getElementById("hero-input");
  const statusPill = document.getElementById("status-pill");
  const statusText = document.getElementById("status-text");
  const loadingBar = document.getElementById("loading-bar");
  const homeView = document.getElementById("home-view");
  const errorBox = document.getElementById("error-box");
  const framesContainer = document.getElementById("frames-container");
  const quickGrid = document.getElementById("quick-grid");
  const addBmBtn = document.getElementById("btn-add-bm");
  const backBtn = document.getElementById("btn-back");
  const fwdBtn = document.getElementById("btn-fwd");
  const reloadBtn = document.getElementById("btn-reload");
  const reconnectBtn = document.getElementById("btn-reconnect");
  const zenBtn = document.getElementById("btn-zen");
  const zenExitBtn = document.getElementById("btn-zen-exit");
  const egressPill = document.getElementById("btn-egress-pill");
  const egressPillText = document.getElementById("egress-pill-text");
  const meowBadge = document.getElementById("meow-badge");
  const exitReadout = document.getElementById("routing-exit-readout");
  const modeDirectBtn = document.getElementById("btn-mode-direct");
  const modeUpstreamBtn = document.getElementById("btn-mode-upstream");
  const modeMeowBtn = document.getElementById("btn-mode-meow");
  const profileLabel = document.getElementById("routing-profile-label");
  const subscriptionRow = document.getElementById("routing-subscription");
  const subscriptionUrl = document.getElementById("routing-subscription-url");
  const importBtn = document.getElementById("btn-routing-import");
  let panelMode = "upstream";
  let subscriptionsEnabled = null;
  let routingBusy = false;
  const profileTitle = { direct: "Direct VPS", upstream: "Pasted Nodes", meow: "Remote Clash (meow-rs)" };
  const checkExitBtn = document.getElementById("btn-check-exit");
  const toggleDrawerBtn = document.getElementById("btn-toggle-drawer");
  const routingDrawer = document.getElementById("routing-drawer");
  const routingNodeRow = document.getElementById("routing-node-row");
  const routingNodeSelect = document.getElementById("routing-node-select");
  const routingClearBtn = document.getElementById("btn-routing-clear");
  const routingConfigInput = document.getElementById("routing-config-input");
  const routingMsg = document.getElementById("routing-msg");
  const routingApplyBtn = document.getElementById("btn-routing-apply");

  let routingState = {
    mode: "direct",
    active_node: null,
    nodes: [],
    has_upstream: false,
    backend: "direct",
  };

  const PROXY_PREFIX = "/p/";
  const WORKER_URL = "/sw.js?v=8";
  const BOOKMARKS_KEY = "dot_proxy_local_bookmarks_v1";
  const STATIC_CACHE_NAME = "bdn-http-cache-v2";
  const STATIC_CACHE_MAX_ENTRIES = 400;
  const STATIC_CACHE_MAX_BYTES = 5 * 1024 * 1024;
  const CACHEABLE_DESTS = new Set(["script", "style", "font", "image"]);
  const DEFAULT_BOOKMARKS = [
    { name: "X", url: "https://x.com", badge: "𝕏" },
    { name: "Google", url: "https://www.google.com", badge: "G" },
    { name: "GitHub", url: "https://github.com", badge: "GH" },
    { name: "YouTube", url: "https://www.youtube.com", badge: "▶" },
    { name: "Reddit", url: "https://www.reddit.com", badge: "R" },
    { name: "Exit IP", url: "https://api.ipify.org", badge: "IP" },
  ];

  const RELOAD_SVG =
    '<svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><path d="M21 12a9 9 0 1 1-2.64-6.36L21 8"/><path d="M21 3v5h-5"/></svg>';
  const STOP_SVG =
    '<svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2.2" stroke-linecap="round"><path d="M18 6 6 18M6 6l12 12"/></svg>';

  let controller = null;
  let transport = null;
  let initPromise = null;
  let runtimeReady = false;
  let swControllerListenerInstalled = false;
  let nextTabId = 1;
  // null means pinned Better Dot Net Home view is active
  let activeTabId = null;
  const tabs = new Map();
  const closedTabs = [];
  let saveSessionTimer;
  let staticCachePruneScheduled = false;

  // Lightweight browser tab context menu
  const tabMenu = document.createElement("div");
  tabMenu.className = "tab-context-menu hidden";
  tabMenu.setAttribute("role", "menu");
  document.body.appendChild(tabMenu);

  function hideTabMenu() {
    tabMenu.classList.add("hidden");
    tabMenu.replaceChildren();
  }

  document.addEventListener("click", (e) => {
    if (!tabMenu.contains(e.target)) hideTabMenu();
  });
  window.addEventListener("blur", hideTabMenu);
  window.addEventListener("resize", hideTabMenu);

  function showTabMenu(x, y, tabId) {
    const tab = tabs.get(tabId);
    if (!tab) return;
    tabMenu.replaceChildren();
    const items = [
      { label: "Reload Tab", action: () => reloadTab(tab) },
      { label: "Duplicate Tab", action: () => openInNewTab(tab.url).catch(showError) },
      { sep: true },
      { label: "Close Tab", action: () => closeTab(tab.id) },
      {
        label: "Close Other Tabs",
        disabled: tabs.size <= 1,
        action: () => {
          for (const otherId of Array.from(tabs.keys())) {
            if (otherId !== tab.id) closeTab(otherId);
          }
        },
      },
      {
        label: "Reopen Closed Tab",
        disabled: closedTabs.length === 0,
        action: () => reopenClosedTab(),
      },
    ];
    for (const item of items) {
      if (item.sep) {
        const hr = document.createElement("div");
        hr.className = "tab-context-sep";
        tabMenu.appendChild(hr);
        continue;
      }
      const btn = document.createElement("button");
      btn.type = "button";
      btn.className = "tab-context-item";
      btn.setAttribute("role", "menuitem");
      btn.textContent = item.label;
      btn.disabled = Boolean(item.disabled);
      btn.addEventListener("click", () => {
        hideTabMenu();
        item.action();
      });
      tabMenu.appendChild(btn);
    }
    tabMenu.classList.remove("hidden");
    const rect = tabMenu.getBoundingClientRect();
    const left = Math.min(x, Math.max(8, window.innerWidth - rect.width - 8));
    const top = Math.min(y, Math.max(8, window.innerHeight - rect.height - 8));
    tabMenu.style.left = `${left}px`;
    tabMenu.style.top = `${top}px`;
  }

  function saveTabSession() {
    clearTimeout(saveSessionTimer);
    const tabsToSave = [...tabs.values()];
    const activePosition = tabsToSave.findIndex(tab => tab.id === activeTabId);
    if (activePosition >= 0 && activePosition < tabsToSave.length - 16) {
      tabsToSave.splice(activePosition, 1);
      tabsToSave.splice(tabsToSave.length - 15, 0, tabs.get(activeTabId));
    }
    try {
      if (!tabsToSave.length) sessionStorage.removeItem(TAB_SESSION_KEY);
      else sessionStorage.setItem(TAB_SESSION_KEY, JSON.stringify(
        normalizeTabSession({ active: activeTabId, tabs: tabsToSave }, location.origin)
      ));
    } catch { /* Storage may be disabled or full. Live browsing still works. */ }
  }

  function saveSessionSoon() {
    clearTimeout(saveSessionTimer);
    saveSessionTimer = setTimeout(saveTabSession, 150);
  }

  window.addEventListener("pagehide", () => {
    saveTabScroll(tabs.get(activeTabId));
    saveTabSession();
  });

  function setStatus(text, kind) {
    statusText.textContent = text;
    statusPill.className = "status-pill" + (kind ? " " + kind : "");
  }

  function setLoading(active) {
    loadingBar.classList.toggle("active", Boolean(active));
  }

  function showError(err) {
    const msg = err instanceof Error ? err.message : String(err);
    errorBox.textContent = msg;
    errorBox.classList.add("visible");
    setStatus("Error", "err");
    setLoading(false);
  }

  function clearError() {
    errorBox.textContent = "";
    errorBox.classList.remove("visible");
  }

  function normalizeTarget(raw) {
    const value = (raw || "").trim();
    if (!value) return "https://x.com";
    if (/^https?:\/\//i.test(value)) return value;
    if (/^(\d{1,3}\.){3}\d{1,3}(:\d+)?([/?#].*)?$/.test(value)) {
      return "http://" + value;
    }
    if (/^[a-z0-9-]+(\.[a-z0-9-]+)+(:\d+)?([/?#].*)?$/i.test(value)) {
      return "https://" + value;
    }
    return "https://www.google.com/search?q=" + encodeURIComponent(value);
  }

  function hostLabel(urlStr) {
    try {
      return new URL(urlStr).hostname.replace(/^www\./, "");
    } catch {
      return urlStr || "Tab";
    }
  }

  function transportEndpoint() {
    return `${location.protocol === "https:" ? "wss:" : "ws:"}//${location.host}/ws/`;
  }

  function decodeProxyUrl(rawHref) {
    if (!rawHref) return null;
    try {
      const full = new URL(String(rawHref).trim(), location.origin);
      if (full.origin !== location.origin) return null;
      const v2 = full.pathname.match(/^\/p\/[^/]+\/[^/]+\/(.+)$/);
      if (v2) {
        const decoded = decodeURIComponent(v2[1]);
        const target = new URL(decoded);
        if (full.hash && !target.hash) {
          try { target.hash = decodeURIComponent(full.hash.slice(1)); } catch { target.hash = full.hash; }
        }
        return /^https?:$/.test(target.protocol) ? target.href : null;
      }
      if (full.pathname.startsWith(PROXY_PREFIX)) {
        const decoded = decodeURIComponent(full.pathname.slice(PROXY_PREFIX.length));
        const target = new URL(decoded);
        return /^https?:$/.test(target.protocol) ? target.href : null;
      }
    } catch {}
    return null;
  }

  function decodeDestinationUrl(rawHref, fallbackBase) {
    if (!rawHref) return null;
    const trimmed = String(rawHref).trim();
    if (!trimmed || /^(javascript|mailto|tel|blob|data|about):/i.test(trimmed)) {
      return null;
    }
    const fromProxy = decodeProxyUrl(trimmed);
    if (fromProxy) return fromProxy;
    try {
      if (/^https?:\/\//i.test(trimmed)) {
        const u = new URL(trimmed);
        return u.origin === location.origin ? null : u.href;
      }
      if (fallbackBase) {
        const u = new URL(trimmed, fallbackBase);
        return /^https?:$/.test(u.protocol) && u.origin !== location.origin ? u.href : null;
      }
    } catch {}
    return null;
  }

  function updateSecurityIndicator(urlStr) {
    if (!lockIcon) return;
    if (!urlStr) {
      lockIcon.classList.remove("insecure", "home");
      lockIcon.classList.add("home");
      lockIcon.setAttribute("title", "Better Dot Net");
      return;
    }
    try {
      const parsed = new URL(urlStr);
      const isHttps = parsed.protocol === "https:";
      lockIcon.classList.remove("home");
      lockIcon.classList.toggle("insecure", !isHttps);
      lockIcon.setAttribute("title", isHttps ? "Encrypted HTTPS via Better Dot Net" : "Unencrypted HTTP site");
    } catch {
      lockIcon.classList.remove("insecure", "home");
    }
  }

  // Local-only bookmarks (zero server sync)
  function loadBookmarks() {
    try {
      const raw = localStorage.getItem(BOOKMARKS_KEY);
      if (raw) {
        const parsed = JSON.parse(raw);
        if (Array.isArray(parsed) && parsed.length > 0) return parsed;
      }
    } catch {}
    return DEFAULT_BOOKMARKS.slice();
  }

  function saveBookmarks(list) {
    try {
      localStorage.setItem(BOOKMARKS_KEY, JSON.stringify(list));
    } catch {}
  }

  function renderBookmarks() {
    const list = loadBookmarks();
    quickGrid.innerHTML = "";
    list.forEach((bm, idx) => {
      const card = document.createElement("div");
      card.className = "quick-card";
      card.setAttribute("role", "button");
      card.setAttribute("tabindex", "0");
      card.dataset.url = bm.url;
      const badge = (bm.badge || (bm.name || "?").slice(0, 2)).toUpperCase();
      card.innerHTML = `
        <div class="quick-icon"></div>
        <div class="quick-meta">
          <div class="quick-name"></div>
          <div class="quick-host"></div>
        </div>
        <button class="quick-del" type="button" title="Remove bookmark">&times;</button>
      `;
      card.querySelector(".quick-icon").textContent = badge;
      card.querySelector(".quick-name").textContent = bm.name;
      card.querySelector(".quick-host").textContent = hostLabel(bm.url);
      card.addEventListener("click", (e) => {
        if (e.target.closest(".quick-del")) {
          e.stopPropagation();
          const next = loadBookmarks().filter((_, i) => i !== idx);
          saveBookmarks(next.length ? next : DEFAULT_BOOKMARKS.slice());
          renderBookmarks();
          return;
        }
        openInNewTab(bm.url).catch(showError);
      });
      card.addEventListener("keydown", (e) => {
        if (e.target.closest(".quick-del")) return;
        if (e.key === "Enter" || e.key === " ") {
          e.preventDefault();
          openInNewTab(bm.url).catch(showError);
        }
      });
      quickGrid.appendChild(card);
    });
  }

  addBmBtn.addEventListener("click", () => {
    const tab = activeTabId ? tabs.get(activeTabId) : null;
    const defaultUrl = tab?.url || "https://";
    const urlInput = prompt("Bookmark URL:", defaultUrl);
    if (!urlInput) return;
    const target = normalizeTarget(urlInput);
    const nameInput = prompt("Bookmark name:", hostLabel(target));
    if (!nameInput) return;
    const list = loadBookmarks();
    list.push({ name: nameInput.trim(), url: target, badge: nameInput.trim().slice(0, 2) });
    saveBookmarks(list);
    renderBookmarks();
  });

  async function createTransport() {
    const LibcurlClient =
      globalThis.LibcurlTransport?.default || globalThis.LibcurlTransport?.LibcurlClient;
    if (typeof LibcurlClient !== "function") {
      throw new Error("Proxy transport runtime failed to load. Reload to try again.");
    }
    const next = new LibcurlClient({ wisp: transportEndpoint() });
    await runtimeDeadline(
      () => next.init(),
      "Transport startup timed out. Click Reconnect to try again."
    );
    return next;
  }

  async function setupTransport() {
    const next = await createTransport();
    transport = next;
    if (controller) {
      controller.setTransport(next);
    }
  }

  async function ensureRuntime() {
    if (runtimeReady) return;
    if (initPromise) return initPromise;

    initPromise = (async () => {
      setStatus("Starting…", "");
      if (
        !navigator.serviceWorker ||
        typeof WebAssembly !== "object" ||
        !globalThis.$scramjet ||
        !globalThis.$scramjetController
      ) {
        throw new Error("Browser lacks ServiceWorker / WebAssembly support required for proxy browsing.");
      }

      if (navigator.storage && typeof navigator.storage.persist === "function") {
        navigator.storage.persist().catch(() => {});
      }

      await runtimeDeadline(
        () => navigator.serviceWorker.register(WORKER_URL, { scope: "/" }),
        "Could not register Service Worker. HTTPS is required."
      );
      await waitForRuntimeWorker(WORKER_URL);

      if (!transport) {
        transport = await createTransport();
      }

      const { Controller } = globalThis.$scramjetController;
      controller = new Controller({
        serviceworker: navigator.serviceWorker.controller,
        transport,
        config: {
          prefix: PROXY_PREFIX,
          scramjetPath: "/assets/rt/engine.js?v=3",
          injectPath: "/assets/rt/controller.inject.js?v=3",
          wasmPath: "/assets/rt/engine.wasm?v=2",
        },
        scramjetConfig: {
          flags: {
            syncxhr: false,
            sourcemaps: false,
            allowInvalidJs: true,
            allowFailedIntercepts: true,
          },
        },
      });

      await runtimeDeadline(
        () => controller.wait(),
        "Proxy controller startup timed out. Reload to try again."
      );

      if (!swControllerListenerInstalled) {
        swControllerListenerInstalled = true;
        navigator.serviceWorker.addEventListener("controllerchange", () => {
          if (!controller || !navigator.serviceWorker.controller) return;
          controller.serviceWorkerController = navigator.serviceWorker.controller;
          controller.guardServiceWorkerRevive = false;
          if (typeof controller.setupMessagePort === "function") {
            controller.setupMessagePort();
          }
        });
      }

      runtimeReady = true;
      if (activeTabId && tabs.get(activeTabId)?.loading) setStatus("Loading page…", "");
      else setStatus("Ready", "ready");
    })();

    try {
      await initPromise;
    } catch (err) {
      initPromise = null;
      throw err;
    }
  }

  // =========================================================================
  // Scramjet v2 Managed Plugins (Real-Browser Frame Lifecycle & Caching)
  // =========================================================================
  function isCacheableStaticResponse(method, destination, status, headers) {
    if (method !== "GET" || !CACHEABLE_DESTS.has(destination)) return false;
    if (status !== 200) return false;
    let cc = "";
    let contentLen = 0;
    for (const [k, v] of headers) {
      const lk = String(k).toLowerCase();
      if (lk === "set-cookie") return false;
      if (lk === "cache-control") cc = String(v).toLowerCase();
      if (lk === "content-length") contentLen = Number(v) || 0;
    }
    if (!cc || /\b(no-store|no-cache|private|max-age=0)\b/.test(cc)) return false;
    if (contentLen > STATIC_CACHE_MAX_BYTES) return false;
    if (cc.includes("immutable")) return true;
    const match = cc.match(/\bmax-age=(\d+)\b/);
    return Boolean(match && Number(match[1]) >= 300);
  }

  function scheduleStaticCachePrune() {
    if (staticCachePruneScheduled) return;
    staticCachePruneScheduled = true;
    setTimeout(async () => {
      staticCachePruneScheduled = false;
      try {
        const cache = await caches.open(STATIC_CACHE_NAME);
        const keys = await cache.keys();
        if (keys.length > STATIC_CACHE_MAX_ENTRIES) {
          await Promise.all(
            keys.slice(0, keys.length - STATIC_CACHE_MAX_ENTRIES + 40).map((k) => cache.delete(k))
          );
        }
      } catch {}
    }, 5000);
  }

  function makeStaticHttpCachePlugin() {
    const { ManagedPlugin } = globalThis.$scramjetController;
    const fromCache = new WeakSet();
    return new (class StaticHttpCachePlugin extends ManagedPlugin {
      constructor() {
        super("bdn-static-cache", []);
      }
      install(frame) {
        super.install(frame);
        this.tap(frame.hooks.fetch.request, async (context, props) => {
          if (
            props.earlyResponse ||
            context.request.method !== "GET" ||
            context.request.cache === "no-store" ||
            context.request.cache === "reload" ||
            !CACHEABLE_DESTS.has(context.parsed.destination) ||
            typeof caches === "undefined"
          ) return;
          try {
            const cache = await caches.open(STATIC_CACHE_NAME);
            const hit = await cache.match(props.url.href);
            if (!hit) return;
            const buf = await hit.arrayBuffer();
            const headers = new Headers(hit.headers);
            fromCache.add(context.request);
            // Hand a clean native Response so Scramjet wraps it via BareResponse.fromNativeResponse.
            props.earlyResponse = new Response(buf, {
              status: hit.status,
              statusText: hit.statusText,
              headers,
            });
          } catch {}
        });

        this.tap(frame.hooks.fetch.preresponse, (context, props) => {
          if (fromCache.has(context.request)) {
            fromCache.delete(context.request);
            return;
          }
          if (
            context.request.method !== "GET" ||
            context.request.cache === "no-store" ||
            !CACHEABLE_DESTS.has(context.parsed.destination) ||
            typeof caches === "undefined"
          ) return;
          const resp = props.response;
          if (!resp || typeof resp.clone !== "function") return;
          const rawHeaders = Array.isArray(resp.rawHeaders) ? resp.rawHeaders : [];
          if (!isCacheableStaticResponse(context.request.method, context.parsed.destination, resp.status, rawHeaders)) {
            return;
          }
          try {
            // Clone synchronously before Scramjet reads the original stream so the live
            // response is never consumed or delayed by background Cache API writes.
            const cloned = resp.clone();
            const cacheKey = context.parsed.url.href;
            cloned
              .arrayBuffer()
              .then(async (buf) => {
                if (buf.byteLength > STATIC_CACHE_MAX_BYTES) return;
                const headers = new Headers();
                for (const [k, v] of rawHeaders) {
                  try { headers.append(k, v); } catch {}
                }
                const cache = await caches.open(STATIC_CACHE_NAME);
                await cache.put(
                  cacheKey,
                  new Response(buf, { status: resp.status, statusText: resp.statusText, headers })
                );
                scheduleStaticCachePrune();
              })
              .catch(() => {});
          } catch {}
        });
      }
    })();
  }

  function makeCatchEscapedLinksPlugin() {
    const { ManagedPlugin } = globalThis.$scramjetController;
    const { ScramjetHeaders } = globalThis.$scramjet;
    return new (class CatchEscapedLinksPlugin extends ManagedPlugin {
      constructor() {
        super("bdn-catch-escaped", []);
      }
      install(frame) {
        super.install(frame);
        this.tap(frame.hooks.fetch.intercept, (context, props) => {
          if (context.parsed.destination !== "document") return;
          const target = destinationHistoryUrl(context.parsed.url.href, location.origin);
          if (!target) return;
          const shellUrl = new URL("/?goto=" + encodeURIComponent(target), location.origin);
          props.response = {
            body: "",
            status: 302,
            statusText: "Found",
            headers: ScramjetHeaders.fromRawHeaders([["Location", shellUrl.href]]),
          };
        });
      }
    })();
  }

  function escapeHtml(text) {
    return String(text || "").replace(/[&<>"']/g, (ch) => ({
      "&": "&amp;", "<": "&lt;", ">": "&gt;", '"': "&quot;", "'": "&#39;",
    })[ch]);
  }

  function renderFrameErrorHtml(title, detail, showRetry) {
    return `<!doctype html><html><head><meta name="robots" content="noindex"><meta charset="utf-8"><title>${escapeHtml(title)}</title></head><body style="background:#080b12;color:#9fb0c9;font:14px -apple-system,BlinkMacSystemFont,'Segoe UI',system-ui,sans-serif;display:grid;place-items:center;min-height:100vh;margin:0;padding:20px;box-sizing:border-box"><div style="max-width:460px;width:100%;padding:22px 24px;border:1px solid rgba(148,163,184,.18);border-radius:14px;background:#0f1420;box-shadow:0 16px 40px rgba(0,0,0,.4)"><p id="dot-connection-failure" style="margin:0 0 8px;color:#eef2f9;font-size:16px;font-weight:650">${escapeHtml(title)}</p><p style="margin:0 0 14px;font-size:12.5px;line-height:1.5;color:#7e91ae;word-break:break-word">${escapeHtml(detail)}</p>${showRetry ? '<button type="button" onclick="location.reload()" style="height:34px;padding:0 16px;border:0;border-radius:8px;background:linear-gradient(135deg,#4f8cff,#7c5cff);color:#fff;font-size:13px;font-weight:600;cursor:pointer">Retry</button>' : ""}</div></body></html>`;
  }

  function makeTransportRecoveryPlugin(tab, isCurrent) {
    const { ManagedPlugin } = globalThis.$scramjetController;
    return new (class TransportRecoveryPlugin extends ManagedPlugin {
      constructor() {
        super("bdn-transport-recovery", []);
      }
      install(frame) {
        super.install(frame);
        this.tap(frame.hooks.error.request, async (context, props) => {
          const dest = context.rawrequest?.destination;
          const isNav = dest === "iframe" || dest === "document" || context.rawrequest?.mode === "navigate";
          if (!isNav) return;

          const errText = String(context.error?.message || context.error || "Network error");
          const isTransportErr = /WebSocket|wisp|closed|connect|EOF|reset|broken|transport|transfer failed|aborted|session/i.test(errText);
          const targetUrl =
            destinationHistoryUrl(decodeProxyUrl(context.rawrequest?.rawUrl), location.origin) || tab.url;

          if (isTransportErr && isCurrent() && tab.autoRecoverCount < 2) {
            tab.autoRecoverCount++;
            props.suppressError = true;
            props.setResponse = {
              body: renderFrameErrorHtml("Reconnecting to website…", errText.slice(0, 180), false),
              status: 503,
              statusText: "Service Unavailable",
              headers: [
                ["Content-Type", "text/html; charset=utf-8"],
                ["Cache-Control", "no-store"],
              ],
            };
            if (tab.id === activeTabId) setStatus("Reconnecting…", "");
            setupTransport()
              .then(() => {
                if (!isCurrent() || !tabs.has(tab.id)) return;
                tab.navigatingHistory = true;
                tab.loading = true;
                if (tab.id === activeTabId) {
                  setStatus("Restoring page…", "");
                  setLoading(true);
                }
                frame.go(targetUrl);
              })
              .catch((err) => {
                if (!isCurrent()) return;
                tab.loading = false;
                if (tab.id === activeTabId) showError(err);
                renderTabs();
              });
            return;
          }

          tab.loading = false;
          if (tab.id === activeTabId) {
            setStatus("Interrupted", "err");
            setLoading(false);
          }
          renderTabs();
          props.suppressError = true;
          props.setResponse = {
            body: renderFrameErrorHtml("Unable to load page", errText.slice(0, 220), true),
            status: 502,
            statusText: "Bad Gateway",
            headers: [
              ["Content-Type", "text/html; charset=utf-8"],
              ["Cache-Control", "no-store"],
            ],
          };
        });
      }
    })();
  }

  function makeBrowserSyncPlugin(tab, isCurrent) {
    const { ManagedPlugin } = globalThis.$scramjetController;
    return new (class BrowserSyncPlugin extends ManagedPlugin {
      constructor() {
        super("bdn-browser-sync", []);
      }
      install(frame) {
        super.install(frame);
        this.tap(frame.hooks.init.post, (context) => {
          if (!isCurrent() || !context.isTopLevel) return;
          const win = context.window;
          const doc = win?.document;

          const syncUrl = (rawUrl) => {
            if (!isCurrent()) return;
            const nextUrl = destinationHistoryUrl(String(rawUrl || ""), location.origin);
            if (!nextUrl || (tab.expectedHistoryTarget && nextUrl !== tab.expectedHistoryTarget)) return;
            if (tab.pendingHistoryFallback) {
              clearTimeout(tab.pendingHistoryFallback);
              tab.pendingHistoryFallback = null;
            }
            tab.expectedHistoryTarget = null;
            if (tab.navigatingHistory) {
              tab.navigatingHistory = false;
              tab.url = nextUrl;
              if (tab.history[tab.historyIndex]) {
                tab.history[tab.historyIndex].url = nextUrl;
              }
            } else if (tab.historyIndex > 0 && tab.history[tab.historyIndex - 1]?.url === nextUrl) {
              tab.historyIndex--;
              tab.url = nextUrl;
            } else if (
              tab.historyIndex < tab.history.length - 1 &&
              tab.history[tab.historyIndex + 1]?.url === nextUrl
            ) {
              tab.historyIndex++;
              tab.url = nextUrl;
            } else {
              recordHistory(tab, nextUrl);
            }
            if (!tab.title || tab.title === hostLabel(tab.url)) {
              tab.title = hostLabel(nextUrl);
            }
            if (tab.id === activeTabId) {
              if (document.activeElement !== addrInput) addrInput.value = tab.url;
              updateSecurityIndicator(tab.url);
            }
            updateNavButtons();
            renderTabs();
            saveSessionSoon();
          };

          const syncTitleAndIcon = () => {
            if (!isCurrent() || !doc) return;
            const nextTitle = doc.title && doc.title.trim() ? doc.title.trim().slice(0, 48) : hostLabel(tab.url);
            let changed = false;
            if (nextTitle !== tab.title) {
              tab.title = nextTitle;
              changed = true;
            }
            try {
              const iconEl = doc.querySelector('link[rel~="icon"][href], link[rel="shortcut icon"][href]');
              const rawIcon = iconEl?.getAttribute("href");
              if (rawIcon && !rawIcon.startsWith("javascript:")) {
                const resolvedIcon = rawIcon.startsWith("data:")
                  ? rawIcon
                  : rawIcon.startsWith(frame.prefix)
                    ? new URL(rawIcon, location.origin).href
                    : null;
                if (resolvedIcon && resolvedIcon !== tab.favicon) {
                  tab.favicon = resolvedIcon;
                  changed = true;
                }
              }
            } catch {}
            if (changed) {
              renderTabs();
              saveSessionSoon();
            }
          };

          syncUrl(context.client.url.href);

          this.tap(context.client.hooks.lifecycle.navigate, (_navCtx, props) => {
            syncUrl(props.url);
          });

          try {
            win.addEventListener("hashchange", () => syncUrl(context.client.url.href), { capture: true });
            win.addEventListener("popstate", () => syncUrl(context.client.url.href), { capture: true });

            // Keep tab title in sync across SPA transitions (GitHub, X, YouTube, Reddit)
            if (doc && typeof MutationObserver !== "undefined") {
              if (tab.titleObserver) {
                try { tab.titleObserver.disconnect(); } catch {}
              }
              const observer = new MutationObserver(() => syncTitleAndIcon());
              const observeTarget = doc.head || doc.documentElement;
              if (observeTarget) {
                observer.observe(observeTarget, { subtree: true, childList: true, characterData: true });
                tab.titleObserver = observer;
              }
            }

            // Browser-like new-tab link handling (never blocks normal same-tab clicks)
            if (doc && !doc.__dotLinksHooked) {
              doc.__dotLinksHooked = true;
              doc.addEventListener(
                "click",
                (e) => {
                  if (e.defaultPrevented || e.button !== 0) return;
                  const anchor = e.target?.closest?.("a[href]");
                  if (!anchor) return;
                  const targetAttr = (anchor.getAttribute("target") || "").toLowerCase();
                  const wantsNewTab = targetAttr === "_blank" || e.ctrlKey || e.metaKey || e.shiftKey;
                  if (!wantsNewTab) return;
                  const dest = decodeDestinationUrl(anchor.href || anchor.getAttribute("href"), tab.url);
                  if (!dest) return;
                  e.preventDefault();
                  e.stopPropagation();
                  openInNewTab(dest).catch(showError);
                },
                true
              );
              doc.addEventListener(
                "auxclick",
                (e) => {
                  if (e.defaultPrevented || e.button !== 1) return;
                  const anchor = e.target?.closest?.("a[href]");
                  if (!anchor) return;
                  const dest = decodeDestinationUrl(anchor.href || anchor.getAttribute("href"), tab.url);
                  if (!dest) return;
                  e.preventDefault();
                  e.stopPropagation();
                  openInNewTab(dest).catch(showError);
                },
                true
              );
            }

            // Route window.open(..., "_blank") into a new browser tab inside our shell
            if (win && !win.__dotOpenHooked && typeof win.open === "function") {
              win.__dotOpenHooked = true;
              const origOpen = win.open.bind(win);
              win.open = function (url, target, features) {
                const dest = decodeDestinationUrl(url, tab.url);
                if (dest && (!target || target === "_blank")) {
                  openInNewTab(dest).catch(showError);
                  return null;
                }
                return origOpen(url, target, features);
              };
            }

            // Forward navigation/zen shortcuts when focus is inside the proxied page
            if (win && !win.__dotKeysHooked) {
              win.__dotKeysHooked = true;
              win.addEventListener("keydown", (e) => {
                if (e.altKey && e.key === "ArrowLeft") {
                  e.preventDefault();
                  goBack();
                } else if (e.altKey && e.key === "ArrowRight") {
                  e.preventDefault();
                  goForward();
                } else if (e.key === "Escape" && document.body.classList.contains("zen-mode")) {
                  document.body.classList.remove("zen-mode");
                }
              });
            }
          } catch {}
        });
      }
    })();
  }

  function sameOrigin(urlA, urlB) {
    try {
      return new URL(urlA).origin === new URL(urlB).origin;
    } catch {
      return false;
    }
  }

  function saveTabScroll(tab) {
    if (!tab || tab.historyIndex < 0 || !tab.history[tab.historyIndex]) return;
    try {
      const win = tab.frame?.element?.contentWindow;
      if (win) {
        tab.history[tab.historyIndex].scrollX = win.scrollX || 0;
        tab.history[tab.historyIndex].scrollY = win.scrollY || 0;
      }
    } catch {}
  }

  function recordHistory(tab, rawUrl) {
    const clean = destinationHistoryUrl(rawUrl, location.origin);
    if (!clean || (tab.expectedHistoryTarget && clean !== tab.expectedHistoryTarget)) return;
    if (tab.navigatingHistory) {
      tab.navigatingHistory = false;
      tab.url = clean;
      if (tab.history[tab.historyIndex]) {
        tab.history[tab.historyIndex].url = clean;
      }
      updateNavButtons();
      saveSessionSoon();
      return;
    }
    const curEntry = tab.history[tab.historyIndex];
    if (curEntry && curEntry.url === clean) {
      tab.url = clean;
      updateNavButtons();
      return;
    }
    saveTabScroll(tab);
    if (tab.historyIndex < tab.history.length - 1) {
      tab.history = tab.history.slice(0, tab.historyIndex + 1);
    }
    tab.history.push({ url: clean, scrollX: 0, scrollY: 0 });
    if (tab.history.length > 100) {
      tab.history.shift();
    }
    tab.historyIndex = tab.history.length - 1;
    tab.url = clean;
    updateNavButtons();
    saveSessionSoon();
  }

  function updateNavButtons() {
    const tab = activeTabId ? tabs.get(activeTabId) : null;
    backBtn.disabled = !(tab && tab.historyIndex > 0);
    fwdBtn.disabled = !(tab && tab.historyIndex < tab.history.length - 1);
    reloadBtn.disabled = !tab;
    const isStop = Boolean(tab && tab.loading);
    reloadBtn.classList.toggle("is-stop", isStop);
    reloadBtn.title = isStop ? "Stop loading" : "Reload (Ctrl+R)";
    reloadBtn.innerHTML = isStop ? STOP_SVG : RELOAD_SVG;
  }

  function renderTabs() {
    homeTabBtn.classList.toggle("active", activeTabId === null);
    tabsList.innerHTML = "";
    for (const tab of tabs.values()) {
      const el = document.createElement("div");
      el.className =
        "tab-item" +
        (tab.id === activeTabId ? " active" : "") +
        (tab.loading ? " loading" : "");
      el.dataset.tabId = String(tab.id);
      el.title = tab.title ? `${tab.title} — ${tab.url}` : tab.url;
      el.innerHTML = `
        <span class="tab-spinner"></span>
        ${
          tab.favicon
            ? '<img class="tab-favicon" alt="" width="14" height="14" />'
            : `<svg class="tab-globe" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2">
          <circle cx="12" cy="12" r="10"/><path d="M2 12h20M12 2a15.3 15.3 0 0 1 4 10 15.3 15.3 0 0 1-4 10 15.3 15.3 0 0 1-4-10 15.3 15.3 0 0 1 4-10z"/>
        </svg>`
        }
        <span class="tab-title"></span>
        <button class="tab-close" type="button" title="Close tab (Ctrl+W)">&times;</button>
      `;
      if (tab.favicon) {
        const img = el.querySelector(".tab-favicon");
        img.src = tab.favicon;
        img.addEventListener("error", () => {
          tab.favicon = null;
          renderTabs();
        });
      }
      el.querySelector(".tab-title").textContent = tab.title || hostLabel(tab.url);
      el.addEventListener("click", (e) => {
        if (e.target.closest(".tab-close")) {
          e.stopPropagation();
          closeTab(tab.id);
          return;
        }
        activateTab(tab.id);
      });
      el.addEventListener("auxclick", (e) => {
        if (e.button === 1) {
          e.preventDefault();
          closeTab(tab.id);
        }
      });
      el.addEventListener("contextmenu", (e) => {
        e.preventDefault();
        showTabMenu(e.clientX, e.clientY, tab.id);
      });
      tabsList.appendChild(el);
    }
  }

  function syncActiveView() {
    const tab = activeTabId ? tabs.get(activeTabId) : null;
    for (const t of tabs.values()) {
      t.pane.classList.toggle("active", t.id === activeTabId);
    }
    if (!tab) {
      framesContainer.classList.remove("active");
      homeView.classList.remove("hidden");
      if (document.activeElement !== addrInput) {
        addrInput.value = "";
      }
      updateSecurityIndicator("");
      setLoading(false);
    } else {
      homeView.classList.add("hidden");
      framesContainer.classList.add("active");
      if (document.activeElement !== addrInput) {
        addrInput.value = tab.url || "";
      }
      updateSecurityIndicator(tab.url);
      setLoading(tab.loading);
    }
    updateNavButtons();
    renderTabs();
    saveSessionSoon();
  }

  function showHome(focusSearch = false) {
    activeTabId = null;
    clearError();
    syncActiveView();
    if (focusSearch) {
      heroInput.value = "";
      heroInput.focus();
    }
  }

  function activateTab(id) {
    if (!tabs.has(id)) return;
    activeTabId = id;
    clearError();
    syncActiveView();
    const tab = tabs.get(id);
    if (!tab.frame) loadDetachedTab(tab).catch(showActiveError(tab));
  }

  function disposeTabFrame(tab) {
    if (tab.pendingHistoryFallback) {
      clearTimeout(tab.pendingHistoryFallback);
      tab.pendingHistoryFallback = null;
    }
    tab.expectedHistoryTarget = null;
    if (tab.titleObserver) {
      try { tab.titleObserver.disconnect(); } catch {}
      tab.titleObserver = null;
    }
    if (tab.frame) {
      const oldFrame = tab.frame;
      tab.frame = null;
      if (controller && Array.isArray(controller.frames)) {
        const idx = controller.frames.indexOf(oldFrame);
        if (idx >= 0) controller.frames.splice(idx, 1);
      }
      try { oldFrame.element.src = "about:blank"; } catch {}
      oldFrame.element.remove();
    }
  }

  function closeTab(id) {
    const tab = tabs.get(id);
    if (!tab) return;
    saveTabScroll(tab);
    if (tab.url) {
      closedTabs.push({
        url: tab.url,
        title: tab.title,
        history: tab.history.slice(-16),
        historyIndex: Math.min(tab.historyIndex, Math.max(0, tab.history.slice(-16).length - 1)),
      });
      if (closedTabs.length > 10) closedTabs.shift();
    }
    const ids = Array.from(tabs.keys());
    const idx = ids.indexOf(id);
    disposeTabFrame(tab);
    tab.pane.remove();
    tabs.delete(id);
    if (activeTabId === id) {
      const remaining = Array.from(tabs.keys());
      if (remaining.length === 0) {
        activeTabId = null;
      } else {
        activeTabId = remaining[Math.min(idx, remaining.length - 1)];
      }
    }
    syncActiveView();
    if (activeTabId !== null) {
      const nextTab = tabs.get(activeTabId);
      if (nextTab && !nextTab.frame) {
        loadDetachedTab(nextTab).catch(showActiveError(nextTab));
      }
    }
  }

  function reopenClosedTab() {
    const saved = closedTabs.pop();
    if (!saved) return;
    const tab = makeTab(saved.url, saved);
    activeTabId = tab.id;
    addrInput.value = tab.url;
    loadDetachedTab(tab).catch(showActiveError(tab));
  }

  function attachFrameToTab(tab) {
    disposeTabFrame(tab);
    const iframe = document.createElement("iframe");
    iframe.title = "Destination website";
    iframe.setAttribute(
      "sandbox",
      "allow-scripts allow-same-origin allow-forms allow-popups allow-modals allow-downloads allow-presentation"
    );
    iframe.allow = "fullscreen; clipboard-read; clipboard-write";

    let created = null;
    const isCurrent = () => tabs.get(tab.id) === tab && tab.frame === created;

    created = controller.createFrame(iframe, {
      plugins: [
        makeStaticHttpCachePlugin(),
        makeCatchEscapedLinksPlugin(),
        makeTransportRecoveryPlugin(tab, isCurrent),
        makeBrowserSyncPlugin(tab, isCurrent),
      ],
    });
    tab.frame = created;
    tab.pane.innerHTML = "";
    tab.pane.appendChild(created.element);

    created.element.addEventListener("load", () => {
      if (!isCurrent()) return;
      const win = created.element.contentWindow;
      try {
        if (win.location.href === "about:blank") return;
        if (tab.expectedHistoryTarget) {
          const actual = destinationHistoryUrl(decodeDestinationUrl(win.location.href, tab.url), location.origin);
          if (actual !== tab.expectedHistoryTarget) return;
          tab.expectedHistoryTarget = null;
        }
      } catch {}

      const doc = created.element.contentDocument;
      if (!doc) {
        tab.loading = false;
        if (tab.id === activeTabId) setLoading(false);
        updateNavButtons();
        renderTabs();
        return;
      }

      if (doc.getElementById("dot-connection-failure")) {
        // Error/reconnecting UI rendered by TransportRecoveryPlugin
        return;
      }

      if (tab.pendingHistoryFallback) {
        clearTimeout(tab.pendingHistoryFallback);
        tab.pendingHistoryFallback = null;
      }
      tab.autoRecoverCount = 0;
      tab.loading = false;

      if (tab.restoreScroll) {
        const { x, y } = tab.restoreScroll;
        tab.restoreScroll = null;
        requestAnimationFrame(() => {
          try { win.scrollTo(x, y); } catch {}
        });
      }

      if (doc.title && doc.title.trim()) {
        tab.title = doc.title.trim().slice(0, 48);
      } else {
        tab.title = hostLabel(tab.url);
      }

      if (tab.id === activeTabId) {
        setStatus("Ready", "ready");
        setLoading(false);
        if (document.activeElement !== addrInput) addrInput.value = tab.url;
        updateSecurityIndicator(tab.url);
      }
      updateNavButtons();
      renderTabs();
      saveSessionSoon();

      try {
        win.addEventListener(
          "pagehide",
          () => {
            if (!isCurrent()) return;
            tab.loading = true;
            if (tab.id === activeTabId) setLoading(true);
            updateNavButtons();
            renderTabs();
          },
          { once: true }
        );
        win.addEventListener("pageshow", (ev) => {
          if (!isCurrent() || !ev.persisted) return;
          tab.loading = false;
          if (tab.id === activeTabId) setLoading(false);
          updateNavButtons();
          renderTabs();
        });
      } catch {}
    });

    return created;
  }

  function makeTab(target, saved = null) {
    const id = nextTabId++;
    const pane = document.createElement("div");
    pane.className = "tab-frame-pane";
    framesContainer.appendChild(pane);
    const tab = {
      id,
      url: target,
      title: saved?.title || hostLabel(target),
      favicon: null,
      loading: false,
      frame: null,
      pane,
      history: saved?.history || [{ url: target, scrollX: 0, scrollY: 0 }],
      historyIndex: saved?.historyIndex ?? 0,
      navigatingHistory: false,
      pendingHistoryFallback: null,
      expectedHistoryTarget: null,
      restoreScroll: null,
      autoRecoverCount: 0,
      pendingNavigation: null,
      titleObserver: null,
    };
    tabs.set(id, tab);
    return tab;
  }

  function showActiveError(tab) {
    return error => {
      if (tabs.get(tab.id) !== tab) return;
      tab.loading = false;
      if (tab.id === activeTabId) showError(error);
      updateNavButtons();
      renderTabs();
    };
  }

  async function loadDetachedTab(tab) {
    if (tab.frame || tab.loading) return;
    tab.loading = true;
    const navigation = Symbol();
    tab.pendingNavigation = navigation;
    syncActiveView();
    if (runtimeReady && tab.id === activeTabId) setStatus("Loading page…", "");
    await ensureRuntime();
    if (tabs.get(tab.id) !== tab || tab.pendingNavigation !== navigation) return;
    const entry = tab.history[tab.historyIndex];
    if (entry) tab.restoreScroll = { x: entry.scrollX, y: entry.scrollY };
    attachFrameToTab(tab);
    tab.frame.go(tab.url);
  }

  async function openInNewTab(rawUrl) {
    clearError();
    const target = destinationHistoryUrl(normalizeTarget(rawUrl), location.origin);
    if (!target) {
      showError("Cannot open the proxy gateway inside itself.");
      return;
    }
    const tab = makeTab(target);
    const { id } = tab;
    activeTabId = id;
    addrInput.value = target;
    await loadDetachedTab(tab).catch(showActiveError(tab));
  }

  async function navigateExistingTab(tab, rawUrl) {
    clearError();
    const target = destinationHistoryUrl(normalizeTarget(rawUrl), location.origin);
    if (!target) {
      addrInput.value = tab.url || "";
      return;
    }
    if (tab.pendingHistoryFallback) clearTimeout(tab.pendingHistoryFallback);
    tab.pendingHistoryFallback = null;
    tab.expectedHistoryTarget = null;
    tab.navigatingHistory = false;
    tab.favicon = null;
    recordHistory(tab, target);
    tab.title = hostLabel(target);
    tab.loading = true;
    addrInput.value = target;
    syncActiveView();
    const navigation = Symbol();
    tab.pendingNavigation = navigation;
    await ensureRuntime();
    if (tabs.get(tab.id) !== tab || tab.pendingNavigation !== navigation) return;
    if (!tab.frame) {
      attachFrameToTab(tab);
    }
    tab.frame.go(target);
  }

  function reloadTab(tab) {
    if (!tab) return;
    if (!tab.frame) {
      loadDetachedTab(tab).catch(showActiveError(tab));
      return;
    }
    tab.loading = true;
    tab.navigatingHistory = true;
    if (tab.id === activeTabId) setLoading(true);
    tab.frame.go(tab.url);
    updateNavButtons();
    renderTabs();
  }

  function stopTab(tab) {
    if (!tab) return;
    try { tab.frame?.element?.contentWindow?.stop(); } catch {}
    tab.loading = false;
    if (tab.id === activeTabId) {
      setLoading(false);
      setStatus("Ready", "ready");
    }
    updateNavButtons();
    renderTabs();
  }

  function stepHistory(delta) {
    const tab = activeTabId ? tabs.get(activeTabId) : null;
    if (!tab || !tab.frame) return;
    const nextIdx = tab.historyIndex + delta;
    if (nextIdx < 0 || nextIdx >= tab.history.length) return;

    saveTabScroll(tab);
    const prevUrl = tab.url;
    const targetEntry = tab.history[nextIdx];
    const target = destinationHistoryUrl(targetEntry.url, location.origin);
    if (!target) return;
    tab.historyIndex = nextIdx;
    tab.navigatingHistory = true;
    tab.url = target;
    tab.title = hostLabel(target);
    addrInput.value = target;
    if (tab.pendingHistoryFallback) {
      clearTimeout(tab.pendingHistoryFallback);
      tab.pendingHistoryFallback = null;
    }
    tab.expectedHistoryTarget = null;

    // For same-origin SPA navigation (X, GitHub, Reddit), try native in-iframe
    // history.back()/forward() first (0ms instant, preserves SPA state & scroll).
    // Fall back to frame.go() if the iframe escapes to the proxy shell or does not transition.
    const win = (tab.frame.element || tab.frame.frame)?.contentWindow;
    if (win && sameOrigin(prevUrl, target) && win.history && win.history.length > 1) {
      tab.expectedHistoryTarget = target;
      let beforeHref = "";
      try { beforeHref = win.location.href; } catch {}
      tab.loading = false;
      syncActiveView();
      try {
        if (delta < 0) win.history.back();
        else win.history.forward();
      } catch {
        tab.expectedHistoryTarget = null;
        tab.loading = true;
        tab.restoreScroll = { x: targetEntry.scrollX || 0, y: targetEntry.scrollY || 0 };
        syncActiveView();
        tab.frame.go(target);
        return;
      }
      tab.pendingHistoryFallback = setTimeout(() => {
        tab.pendingHistoryFallback = null;
        if (tab.expectedHistoryTarget !== target || tabs.get(tab.id) !== tab) return;
        tab.expectedHistoryTarget = null;
        let nowHref = "";
        try { nowHref = win.location.href; } catch {}
        const decoded = destinationHistoryUrl(decodeDestinationUrl(nowHref, target), location.origin);
        if (nowHref === beforeHref || decoded !== target) {
          tab.navigatingHistory = true;
          tab.loading = true;
          tab.restoreScroll = { x: targetEntry.scrollX || 0, y: targetEntry.scrollY || 0 };
          syncActiveView();
          tab.frame.go(target);
        }
      }, 800);
      return;
    }

    tab.loading = true;
    tab.restoreScroll = { x: targetEntry.scrollX || 0, y: targetEntry.scrollY || 0 };
    syncActiveView();
    tab.frame.go(target);
  }

  function goBack() {
    stepHistory(-1);
  }

  function goForward() {
    stepHistory(1);
  }

  const reconnectTransport = makeReconnectCoordinator(
    async () => {
      if (controller) await setupTransport();
      else await ensureRuntime();
    },
    () => activeTabId ? tabs.get(activeTabId) : null,
    phase => {
      if (phase === "connecting") {
        clearError();
        setStatus("Reconnecting…", "");
        setLoading(true);
      } else if (phase === "restoring") {
        setStatus("Restoring page…", "");
        setLoading(true);
      } else {
        setStatus("Ready", "ready");
        setLoading(false);
      }
    },
    tab => {
      tab.loading = true;
      tab.navigatingHistory = true;
      attachFrameToTab(tab);
      tab.frame.go(tab.url);
      syncActiveView();
    },
    showError,
  );

  homeTabBtn.addEventListener("click", () => showHome(false));
  newTabBtn.addEventListener("click", () => showHome(true));

  addrForm.addEventListener("submit", (e) => {
    e.preventDefault();
    addrInput.blur();
    const tab = activeTabId ? tabs.get(activeTabId) : null;
    if (!tab) {
      openInNewTab(addrInput.value).catch(showError);
    } else {
      navigateExistingTab(tab, addrInput.value).catch(showError);
    }
  });

  addrInput.addEventListener("keydown", (e) => {
    if (e.key === "Escape") {
      const tab = activeTabId ? tabs.get(activeTabId) : null;
      addrInput.value = tab?.url || "";
      addrInput.blur();
    }
  });

  heroForm.addEventListener("submit", (e) => {
    e.preventDefault();
    openInNewTab(heroInput.value).catch(showError);
  });

  backBtn.addEventListener("click", goBack);
  fwdBtn.addEventListener("click", goForward);
  reloadBtn.addEventListener("click", () => {
    const tab = activeTabId ? tabs.get(activeTabId) : null;
    if (!tab) return;
    if (tab.loading) stopTab(tab);
    else reloadTab(tab);
  });
  reconnectBtn.addEventListener("click", () => reconnectTransport());
  zenBtn.addEventListener("click", () => document.body.classList.add("zen-mode"));
  zenExitBtn.addEventListener("click", () => document.body.classList.remove("zen-mode"));

  // Single-click unfocused address bar selects entire URL (Chrome/Safari/Arc behavior)
  addrInput.addEventListener("focus", () => {
    requestAnimationFrame(() => addrInput.select());
  });

  // Double-click empty area on tab strip opens Home (New Tab)
  tabsList.addEventListener("dblclick", (e) => {
    if (e.target === tabsList) showHome(true);
  });

  // Real browser keyboard shortcuts
  window.addEventListener("keydown", (e) => {
    const mod = e.ctrlKey || e.metaKey;
    const key = e.key.toLowerCase();
    if (mod && key === "l") {
      e.preventDefault();
      document.body.classList.remove("zen-mode");
      addrInput.focus();
      addrInput.select();
    } else if (mod && e.shiftKey && key === "t") {
      e.preventDefault();
      document.body.classList.remove("zen-mode");
      reopenClosedTab();
    } else if (mod && key === "t") {
      e.preventDefault();
      document.body.classList.remove("zen-mode");
      showHome(true);
    } else if (mod && key === "w" && activeTabId !== null) {
      e.preventDefault();
      closeTab(activeTabId);
    } else if (((mod && !e.shiftKey && key === "r") || e.key === "F5") && activeTabId !== null) {
      e.preventDefault();
      reloadTab(tabs.get(activeTabId));
    } else if (mod && /^[0-9]$/.test(e.key)) {
      e.preventDefault();
      if (e.key === "0") {
        showHome(false);
      } else {
        const ids = Array.from(tabs.keys());
        const targetId = ids[Number(e.key) - 1];
        if (targetId !== undefined) activateTab(targetId);
      }
    } else if (e.altKey && e.key === "ArrowLeft") {
      e.preventDefault();
      goBack();
    } else if (e.altKey && e.key === "ArrowRight") {
      e.preventDefault();
      goForward();
    } else if (e.key === "Escape") {
      if (!tabMenu.classList.contains("hidden")) {
        hideTabMenu();
      } else if (document.body.classList.contains("zen-mode")) {
        document.body.classList.remove("zen-mode");
      }
    }
  });

  // =========================================================================
  // Home Egress Routing Control Panel (Direct VPS <-> Upstream Proxy / meow-rs)
  // =========================================================================
  function setRoutingMessage(text, kind) {
    if (!routingMsg) return;
    routingMsg.textContent = text;
    routingMsg.className = "routing-msg" + (kind ? " " + kind : "");
  }

  function currentProfile(mode = panelMode) {
    return routingState[mode] || (mode === "upstream" ? routingState : { nodes: [] });
  }

  function renderRoutingUI() {
    const mode = routingState.mode;
    const selected = !routingDrawer.classList.contains("hidden") ? panelMode : mode;
    for (const [key, button] of [["direct", modeDirectBtn], ["upstream", modeUpstreamBtn], ["meow", modeMeowBtn]]) {
      if (!button) continue;
      button.classList.toggle("active", key === selected);
      button.setAttribute("aria-pressed", String(key === selected));
      button.disabled = routingBusy;
    }
    if (egressPill && egressPillText) {
      egressPill.classList.toggle("upstream", mode !== "direct");
      egressPill.classList.toggle("direct", mode === "direct");
      const label = mode === "direct" ? "Direct" : (routingState.active_node || profileTitle[mode] || String(mode || ""));
      egressPillText.textContent = label.length > 16 ? label.slice(0, 15) + "…" : label;
      egressPill.title = profileTitle[mode] + " (click to configure)";
    }
    if (meowBadge) meowBadge.textContent = profileTitle[mode] || mode;
    const remote = panelMode === "meow";
    if (profileLabel) profileLabel.textContent = profileTitle[panelMode];
    if (subscriptionRow) subscriptionRow.classList.toggle("hidden", !remote);
    if (importBtn) importBtn.disabled = routingBusy || subscriptionsEnabled === false;
    if (subscriptionUrl) subscriptionUrl.disabled = routingBusy || subscriptionsEnabled === false;
    routingConfigInput.classList.toggle("hidden", remote);
    routingConfigInput.disabled = routingBusy;
    const profile = currentProfile();
    const nodes = profile.nodes || [];
    routingNodeRow.classList.toggle("hidden", !nodes.length);
    routingNodeSelect.replaceChildren();
    routingNodeSelect.disabled = routingBusy;
    for (const n of nodes) {
      const option = document.createElement("option");
      option.value = n.name;
      option.textContent = `${n.name} (${n.type} · ${n.server}:${n.port})`;
      option.selected = n.name === profile.active_node;
      routingNodeSelect.appendChild(option);
    }
    routingClearBtn.classList.toggle("hidden", !nodes.length);
    routingClearBtn.disabled = routingBusy;
    routingApplyBtn.disabled = routingBusy || (remote && !nodes.length);
    routingApplyBtn.textContent = remote ? "Use Selected Node" : "Apply & Use Node";
  }

  function openRoutingProfile(mode) {
    if (panelMode !== mode) {
      if (subscriptionUrl) subscriptionUrl.value = "";
    }
    panelMode = mode;
    routingDrawer.classList.remove("hidden");
    renderRoutingUI();
    if (mode === "meow" && subscriptionsEnabled === false) {
      setRoutingMessage("Remote import is disabled until the operator approves a subscription host.", "");
    } else {
      setRoutingMessage(mode === "meow" ? "Import a Clash subscription, then choose a saved node to use." : "Paste one or more nodes, or choose a saved node.", "");
    }
  }

  async function refreshRoutingStatus() {
    try {
      const res = await fetch("/api/routing", { cache: "no-store" });
      if (!res.ok) return;
      routingState = await res.json();
      subscriptionsEnabled = routingState.subscriptions_enabled ?? null;
      if (routingDrawer.classList.contains("hidden") && routingState.mode !== "direct") panelMode = routingState.mode;
      renderRoutingUI();
    } catch {}
  }

  async function applyRoutingUpdate(payload, success) {
    if (routingBusy) return false;
    routingBusy = true; renderRoutingUI();
    setRoutingMessage("Applying route…", "");
    try {
      const res = await fetch("/api/routing", { method: "POST", headers: { "Content-Type": "application/json" }, body: JSON.stringify(payload) });
      const data = await res.json().catch(() => ({}));
      if (!res.ok) {
        setRoutingMessage(data.error || `Failed (HTTP ${res.status})`, "err");
        return false;
      }
      routingState = data;
      if (payload.config) routingConfigInput.value = "";
      setRoutingMessage(success, "ok");
      return true;
    } catch { setRoutingMessage("Unable to apply the route.", "err"); return false; }
    finally { routingBusy = false; renderRoutingUI(); }
  }

  async function checkExitIp() {
    checkExitBtn.disabled = true;
    exitReadout.className = "exit-readout";
    exitReadout.textContent = "Checking…";
    try {
      const res = await fetch("/api/routing/check", { method: "POST" });
      const data = await res.json().catch(() => ({}));
      if (data.ok && data.exit_ip) {
        exitReadout.textContent = `${data.exit_ip} · ${data.latency_ms} ms`;
      } else {
        exitReadout.className = "exit-readout err";
        exitReadout.textContent = data.error || "Check failed";
      }
    } catch {
      exitReadout.className = "exit-readout err";
      exitReadout.textContent = "Check error";
    } finally {
      checkExitBtn.disabled = false;
    }
  }

  egressPill?.addEventListener("click", () => {
    if (routingBusy) return;
    showHome(false);
    openRoutingProfile(routingState.mode === "direct" ? panelMode : routingState.mode);
  });

  toggleDrawerBtn?.addEventListener("click", () => {
    if (routingDrawer.classList.contains("hidden")) openRoutingProfile(panelMode);
    else { routingDrawer.classList.add("hidden"); renderRoutingUI(); }
  });

  modeDirectBtn?.addEventListener("click", () => {
    routingDrawer.classList.add("hidden");
    if (routingState.mode !== "direct") {
      applyRoutingUpdate({ mode: "direct" }, "Direct VPS selected. Saved nodes are kept.");
    } else {
      renderRoutingUI();
    }
  });
  modeUpstreamBtn?.addEventListener("click", () => {
    if (!routingDrawer.classList.contains("hidden") && panelMode === "upstream") {
      routingDrawer.classList.add("hidden");
      renderRoutingUI();
      return;
    }
    openRoutingProfile("upstream");
  });
  modeMeowBtn?.addEventListener("click", () => {
    if (!routingDrawer.classList.contains("hidden") && panelMode === "meow") {
      routingDrawer.classList.add("hidden");
      renderRoutingUI();
      return;
    }
    openRoutingProfile("meow");
  });

  routingNodeSelect?.addEventListener("change", () => {
    if (panelMode !== "meow" && routingNodeSelect.value) {
      applyRoutingUpdate({ mode: panelMode, selected_node: routingNodeSelect.value }, "Node selected for new connections.");
    }
  });

  async function importSubscription() {
    if (routingBusy || subscriptionsEnabled === false) return;
    const url = subscriptionUrl.value.trim();
    let parsed;
    try { parsed = new URL(url); } catch {}
    if (!parsed || parsed.protocol !== "https:" || parsed.port || parsed.username || parsed.password || parsed.hash) {
      setRoutingMessage("Enter an HTTPS Clash subscription URL on port 443.", "err");
      subscriptionUrl.focus();
      return;
    }
    routingBusy = true;
    renderRoutingUI();
    setRoutingMessage("Importing subscription…", "");
    try {
      const res = await fetch("/api/routing/subscription", {
        method: "POST",
        headers: { "Content-Type": "application/json" },
        body: JSON.stringify({ url }),
      });
      const data = await res.json().catch(() => ({}));
      if (!res.ok) {
        setRoutingMessage(data.error || `Import failed (HTTP ${res.status}). The existing profile was kept.`, "err");
        return;
      }
      routingState = data;
      subscriptionUrl.value = "";
      const selected = data.mode === "meow" ? ` Using ${data.meow.active_node}.` : " Choose a node to switch routes.";
      setRoutingMessage(`Imported ${data.meow.nodes.length} nodes.${selected}`, "ok");
    } catch {
      setRoutingMessage("Import response unavailable. Check the saved nodes before retrying.", "err");
    } finally {
      routingBusy = false;
      renderRoutingUI();
    }
  }
  importBtn?.addEventListener("click", importSubscription);
  subscriptionUrl?.addEventListener("keydown", (e) => {
    if (e.key === "Enter") {
      e.preventDefault();
      importSubscription();
    }
  });

  routingApplyBtn?.addEventListener("click", () => {
    const selected = routingNodeSelect.value || undefined;
    if (panelMode === "meow") {
      if (selected) applyRoutingUpdate({ mode: "meow", selected_node: selected }, "Remote node selected for new connections.");
      return;
    }
    const raw = routingConfigInput.value.trim();
    if (!raw && !selected) {
      setRoutingMessage("Paste a node or choose one already saved.", "err");
      routingConfigInput.focus(); return;
    }
    applyRoutingUpdate(raw ? { mode: "upstream", config: raw } : { mode: "upstream", selected_node: selected },
      "Pasted node selected. Replacing a profile may interrupt streams using that profile.");
  });

  routingClearBtn?.addEventListener("click", async () => {
    if (routingBusy) return;
    const deleting = panelMode;
    const wasActive = routingState.mode === deleting;
    routingBusy = true; renderRoutingUI();
    try {
      const res = await fetch("/api/routing?mode=" + deleting, { method: "DELETE" });
      if (!res.ok) { setRoutingMessage("Unable to clear this profile.", "err"); return; }
      routingState = await res.json();
      if (deleting === "meow") subscriptionUrl.value = "";
      else routingConfigInput.value = "";
      exitReadout.textContent = "";
      setRoutingMessage(wasActive ? "Profile deleted. New connections use Direct VPS." : "Profile deleted. The active route is unchanged.", "ok");
    } catch { setRoutingMessage("Unable to clear this profile.", "err"); }
    finally { routingBusy = false; renderRoutingUI(); }
  });

  checkExitBtn?.addEventListener("click", () => checkExitIp());

  renderBookmarks();
  syncActiveView();
  refreshRoutingStatus();

  const params = new URLSearchParams(location.search);
  const initialTarget = params.get("goto") || params.get("url");
  let restoreActive = null;
  try {
    const restored = normalizeTabSession(JSON.parse(sessionStorage.getItem(TAB_SESSION_KEY)), location.origin);
    for (const saved of restored.tabs) {
      const tab = makeTab(saved.url, saved);
      if (saved.id === restored.active) restoreActive = tab.id;
    }
  } catch { /* A bad or inaccessible snapshot should not block startup. */ }
  if (initialTarget) {
    history.replaceState(null, "", "/");
    openInNewTab(initialTarget).catch(showError);
  } else if (restoreActive !== null) {
    activateTab(restoreActive);
  } else {
    syncActiveView();
    ensureRuntime().catch(showError);
  }
})();
