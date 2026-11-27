"""Loopback Chromium regression test for Scramjet runtime (engine.js) iframe messaging & URL handling.

Runs real web/assets/rt/engine.js + engine.wasm + controller.*.js + web/sw.js
inside headless Chrome against synthetic local loopback fixtures (zero external network).
"""
import asyncio
import json
import os
import socket
import sys
from pathlib import Path

from aiohttp import web
from playwright.async_api import async_playwright

ROOT = Path(__file__).resolve().parents[1]
WEB_DIR = ROOT / "web"

HARNESS_HTML = r"""<!doctype html>
<html>
<head>
  <meta charset="utf-8" />
  <title>Scramjet Runtime Fixture Harness</title>
  <script src="/assets/rt/engine.js"></script>
  <script src="/assets/rt/controller.api.js"></script>
</head>
<body>
  <div id="mount"></div>
  <script>
  window.__harnessErrors = [];
  window.addEventListener("error", (e) => {
    window.__harnessErrors.push({
      where: "shell",
      message: String(e.message || e.error),
      filename: e.filename || "",
      lineno: e.lineno || 0,
      colno: e.colno || 0,
    });
  });
  window.addEventListener("unhandledrejection", (e) => {
    window.__harnessErrors.push({
      where: "shell-rejection",
      message: String(e.reason?.stack || e.reason),
    });
  });

  class LoopbackBareTransport {
    async init() { this.ready = true; }
    async request(remote, method, body, headers) {
      const target = typeof remote === "string" ? remote : remote.href;
      const proxyUrl = "/__loopback_fetch?target=" + encodeURIComponent(target);
      const reqHeaders = new Headers();
      if (Array.isArray(headers)) {
        for (const [k, v] of headers) {
          const lk = String(k).toLowerCase();
          if (["host", "origin", "referer", "sec-fetch-site", "sec-fetch-mode", "sec-fetch-dest", "sec-fetch-user", "cookie", "user-agent"].includes(lk)) {
            reqHeaders.set("x-orig-" + lk, String(v));
          } else {
            try { reqHeaders.set(k, String(v)); } catch {}
          }
        }
      } else if (headers && typeof headers === "object") {
        for (const [k, v] of Object.entries(headers)) {
          try { reqHeaders.set(k, String(v)); } catch {}
        }
      }
      const res = await fetch(proxyUrl, {
        method: method || "GET",
        headers: reqHeaders,
        body: method === "GET" || method === "HEAD" ? undefined : body,
        redirect: "manual",
      });
      const rawHeaders = [];
      const encodedStatus = Number(res.headers.get("x-fixture-status") || res.status);
      const encodedStatusText = res.headers.get("x-fixture-statustext") || res.statusText || "OK";
      for (const [k, v] of res.headers.entries()) {
        if (k.startsWith("x-fixture-")) continue;
        rawHeaders.push([k, v]);
      }
      const noBody = [101, 204, 205, 304].includes(encodedStatus);
      const buf = noBody ? null : await res.arrayBuffer();
      return {
        body: buf,
        headers: rawHeaders,
        status: encodedStatus,
        statusText: encodedStatusText,
      };
    }
    connect() {
      throw new Error("WebSocket not used in fixture");
    }
  }

  window.__startFixture = async function(targetUrl) {
    const reg = await navigator.serviceWorker.register("/sw.js", { scope: "/" });
    await navigator.serviceWorker.ready;
    if (!navigator.serviceWorker.controller) {
      await new Promise((resolve) => {
        navigator.serviceWorker.addEventListener("controllerchange", resolve, { once: true });
      });
    }
    const transport = new LoopbackBareTransport();
    await transport.init();
    const { Controller } = globalThis.$scramjetController;
    const controller = new Controller({
      serviceworker: navigator.serviceWorker.controller,
      transport,
      config: {
        prefix: "/p/",
        scramjetPath: "/assets/rt/engine.js",
        injectPath: "/assets/rt/controller.inject.js",
        wasmPath: "/assets/rt/engine.wasm",
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
    await controller.wait();
    const iframe = document.createElement("iframe");
    iframe.style.width = "800px";
    iframe.style.height = "600px";
    iframe.setAttribute(
      "sandbox",
      "allow-scripts allow-same-origin allow-forms allow-popups allow-modals allow-downloads allow-presentation"
    );
    document.getElementById("mount").appendChild(iframe);
    const frame = controller.createFrame(iframe);
    frame.go(targetUrl);
    window.__controller = controller;
    window.__frame = frame;
    return true;
  };
  </script>
</body>
</html>
"""

PARENT_HTML = r"""<!doctype html>
<html>
<head><meta charset="utf-8"><title>Parent Fixture</title></head>
<body>
<h1>Parent Fixture</h1>
<div id="status">running</div>
<script>
(async () => {
  const results = {
    errors: [],
    checks: {},
  };
  window.__fixtureResults = results;
  window.addEventListener("error", (e) => {
    results.errors.push({
      type: "error",
      message: String(e.message || e.error),
      filename: e.filename || "",
      lineno: e.lineno || 0,
      colno: e.colno || 0,
    });
  });
  window.addEventListener("unhandledrejection", (e) => {
    results.errors.push({
      type: "rejection",
      message: String(e.reason?.stack || e.reason),
    });
  });

  function waitForMessage(predicate, timeoutMs = 4000) {
    return new Promise((resolve, reject) => {
      const timer = setTimeout(() => {
        window.removeEventListener("message", onMsg);
        reject(new Error("timeout waiting for message"));
      }, timeoutMs);
      function onMsg(ev) {
        try {
          if (predicate(ev)) {
            clearTimeout(timer);
            window.removeEventListener("message", onMsg);
            resolve(ev);
          }
        } catch (err) {
          clearTimeout(timer);
          window.removeEventListener("message", onMsg);
          reject(err);
        }
      }
      window.addEventListener("message", onMsg);
    });
  }

  // 1. Cross-origin child iframe: string postMessage handshake + source/origin + MessageEvent.prototype.source getter
  const widget = document.createElement("iframe");
  widget.src = "https://widget.fixture.test/widget";
  document.body.appendChild(widget);

  try {
    const readyEv = await waitForMessage((e) => e.data === "widget-ready");
    results.checks.widgetReadyOrigin = readyEv.origin;
    results.checks.widgetReadySourceMatches = readyEv.source === widget.contentWindow;
    try {
      const getter = Object.getOwnPropertyDescriptor(MessageEvent.prototype, "source").get;
      results.checks.protoSourceGetterOk = getter.call(readyEv) === widget.contentWindow;
    } catch (err) {
      results.checks.protoSourceGetterOk = "ERR: " + err.message;
    }
  } catch (err) {
    results.checks.widgetReady = "ERR: " + err.message;
  }

  // 2. Parent -> Child string postMessage -> Child replies via e.source.postMessage(..., e.origin)
  try {
    const pongPromise = waitForMessage((e) => typeof e.data === "string" && e.data.startsWith("pong:"));
    widget.contentWindow.postMessage("ping-string", "https://widget.fixture.test");
    const pongEv = await pongPromise;
    results.checks.stringHandshakeData = pongEv.data;
    results.checks.stringHandshakeOrigin = pongEv.origin;
    results.checks.stringHandshakeSourceMatches = pongEv.source === widget.contentWindow;
  } catch (err) {
    results.checks.stringHandshake = "ERR: " + err.message;
  }

  // 3. Object event listener { handleEvent(e) }
  try {
    const objListenerResult = await new Promise((resolve, reject) => {
      const timer = setTimeout(() => reject(new Error("timeout objListener")), 3000);
      const handlerObj = {
        handleEvent(e) {
          if (e.data && e.data.kind === "obj-listener-reply") {
            clearTimeout(timer);
            window.removeEventListener("message", handlerObj);
            resolve({ origin: e.origin, sourceMatches: e.source === widget.contentWindow, sourceField: e.data.source });
          }
        },
      };
      window.addEventListener("message", handlerObj);
      widget.contentWindow.postMessage({ cmd: "echo-obj-listener" }, "https://widget.fixture.test");
    });
    results.checks.objectListener = objListenerResult;
  } catch (err) {
    results.checks.objectListener = "ERR: " + err.message;
  }

  // 4. MessageEvent with data === null
  try {
    const nullPromise = waitForMessage((e) => e.data === null);
    widget.contentWindow.postMessage({ cmd: "send-null" }, "https://widget.fixture.test");
    const nullEv = await nullPromise;
    results.checks.nullDataOrigin = nullEv.origin;
    results.checks.nullDataValue = nullEv.data;
    results.checks.nullDataSourceMatches = nullEv.source === widget.contentWindow;
  } catch (err) {
    results.checks.nullData = "ERR: " + err.message;
  }

  // 5. Null-prototype object postMessage (Object.create(null))
  try {
    const npPromise = waitForMessage((e) => e.data && e.data.kind === "null-proto-reply");
    const nullProtoPayload = Object.create(null);
    nullProtoPayload.cmd = "echo-null-proto";
    nullProtoPayload.source = "parent-null-proto";
    widget.contentWindow.postMessage(nullProtoPayload, "https://widget.fixture.test");
    const npEv = await npPromise;
    results.checks.nullProtoReply = { origin: npEv.origin, echoed: npEv.data.echoed };
  } catch (err) {
    results.checks.nullProtoReply = "ERR: " + err.message;
  }

  // 6. MessageChannel transferred across iframes (string + object + null)
  try {
    const mc = new MessageChannel();
    const mcResults = await new Promise((resolve, reject) => {
      const timer = setTimeout(() => reject(new Error("timeout mc")), 3000);
      const got = [];
      mc.port1.onmessage = (e) => {
        got.push(e.data);
        if (got.length === 3) {
          clearTimeout(timer);
          resolve(got);
        }
      };
      widget.contentWindow.postMessage({ cmd: "use-port" }, "https://widget.fixture.test", [mc.port2]);
    });
    results.checks.messageChannel = mcResults;
  } catch (err) {
    results.checks.messageChannel = "ERR: " + err.message;
  }

  // 7. targetOrigin mismatch (parent->child, omitted targetOrigin, empty-string SyntaxError, and child->parent)
  try {
    let childToParentWrongCount = 0;
    const onBadChildMsg = (ev) => {
      if (ev.data === "bad-child-to-parent") childToParentWrongCount++;
    };
    window.addEventListener("message", onBadChildMsg);

    // 7a. empty string targetOrigin must throw SyntaxError
    let emptyOriginErr = null;
    try {
      widget.contentWindow.postMessage("x", "");
    } catch (err) {
      emptyOriginErr = err.name;
    }
    results.checks.emptyTargetOriginThrowsSyntaxError = emptyOriginErr === "SyntaxError";

    // 7b. same-origin omitted targetOrigin (defaults to "/") MUST deliver
    const selfDefaultP = waitForMessage((e) => e.data === "self-default-origin");
    window.postMessage("self-default-origin");
    const selfDefaultEv = await selfDefaultP;
    results.checks.sameOriginDefaultTargetOk =
      selfDefaultEv.origin === "https://parent.fixture.test" && selfDefaultEv.source === window;

    // 7c. cross-origin wrong targetOrigin + omitted targetOrigin + empty options object MUST NOT deliver
    const mismatchRes = await new Promise((resolve) => {
      widget.contentWindow.postMessage({ cmd: "should-not-deliver" }, "https://wrong.fixture.test");
      widget.contentWindow.postMessage({ cmd: "should-not-deliver" });
      widget.contentWindow.postMessage({ cmd: "should-not-deliver" }, {});
      widget.contentWindow.postMessage({ cmd: "send-wrong-to-parent" }, "https://widget.fixture.test");
      setTimeout(async () => {
        const checkP = waitForMessage((e) => e.data && e.data.kind === "mismatch-count");
        widget.contentWindow.postMessage({ cmd: "get-mismatch-count" }, "https://widget.fixture.test");
        const ev = await checkP;
        resolve(ev.data.count);
      }, 250);
    });
    window.removeEventListener("message", onBadChildMsg);
    results.checks.targetOriginMismatchBlocked = mismatchRes === 0;
    results.checks.childToParentMismatchBlocked = childToParentWrongCount === 0;
  } catch (err) {
    results.checks.targetOriginMismatchBlocked = "ERR: " + err.message;
  }

  // 8. about:blank iframe: postMessage + blob URL + Worker + revokeObjectURL + nested srcdoc + <base>
  try {
    const blank = document.createElement("iframe");
    document.body.appendChild(blank);
    const bWin = blank.contentWindow;
    const bDoc = blank.contentDocument;

    // 8a. postMessage from script running INSIDE about:blank
    const blankMsgP = waitForMessage((e) => e.data === "from-about-blank");
    const bScript = bDoc.createElement("script");
    bScript.textContent = "window.parent.postMessage('from-about-blank', '*');";
    bDoc.body.appendChild(bScript);
    const blankMsgEv = await blankMsgP;
    results.checks.aboutBlankPostMessage = {
      origin: blankMsgEv.origin,
      sourceMatches: blankMsgEv.source === bWin,
    };

    // 8b. blob URL + Worker + revokeObjectURL inside about:blank
    const blobCode = "self.onmessage = (e) => self.postMessage('worker:' + e.data);";
    const blobUrl = bWin.URL.createObjectURL(new bWin.Blob([blobCode], { type: "application/javascript" }));
    results.checks.aboutBlankBlobUrl = blobUrl;
    const workerReply = await new Promise((resolve, reject) => {
      const timer = setTimeout(() => reject(new Error("worker timeout")), 4000);
      try {
        const w = new bWin.Worker(blobUrl);
        w.onerror = (err) => { clearTimeout(timer); reject(new Error(err.message || "worker error")); };
        w.onmessage = (e) => { clearTimeout(timer); w.terminate(); resolve(e.data); };
        w.postMessage("hello");
      } catch (err) {
        clearTimeout(timer);
        reject(err);
      }
    });
    results.checks.aboutBlankWorkerReply = workerReply;
    bWin.URL.revokeObjectURL(blobUrl);
    bWin.URL.revokeObjectURL("");
    bWin.URL.revokeObjectURL("not-a-blob");

    // 8c. nested srcdoc inside about:blank
    const nested = bDoc.createElement("iframe");
    nested.srcdoc = "<!doctype html><p>nested ok</p>";
    bDoc.body.appendChild(nested);
    results.checks.aboutBlankSrcdocOk = true;

    // 8d. <base href="/sub/"> inside about:blank -> baseURI and meta.base
    const baseEl = bDoc.createElement("base");
    baseEl.setAttribute("href", "/sub/");
    bDoc.head.appendChild(baseEl);
    results.checks.aboutBlankBaseUri = bDoc.baseURI;
  } catch (err) {
    results.checks.aboutBlankSuite = "ERR: " + err.message;
  }

  // 9. HTMLAnchorElement URL property getters on empty/hash/javascript/invalid href
  try {
    const a1 = document.createElement("a");
    a1.setAttribute("href", "");
    const a2 = document.createElement("a");
    a2.setAttribute("href", "#section");
    const a3 = document.createElement("a");
    a3.setAttribute("href", "javascript:void(0)");
    const a4 = document.createElement("a");
    a4.setAttribute("href", "http://[::1");
    results.checks.anchorPropsOk = {
      a1Host: a1.hostname,
      a2Hash: a2.hash,
      a3Proto: a3.protocol,
      a4Host: a4.hostname,
    };
  } catch (err) {
    results.checks.anchorPropsOk = "ERR: " + err.message;
  }

  // 10. HTTP 304 Not Modified without Location header
  try {
    const r304 = await fetch("https://parent.fixture.test/not-modified", { cache: "no-store" });
    results.checks.fetch304Status = r304.status;
  } catch (err) {
    results.checks.fetch304Status = "ERR: " + err.message;
  }

  // 11. Page with malformed <base href> does not crash HTML rewriter
  try {
    const badBase = document.createElement("iframe");
    badBase.src = "https://parent.fixture.test/bad-base";
    await new Promise((resolve, reject) => {
      const timer = setTimeout(() => reject(new Error("bad-base timeout")), 4000);
      badBase.onload = () => { clearTimeout(timer); resolve(); };
      document.body.appendChild(badBase);
    });
    results.checks.badBaseTitle = badBase.contentDocument?.title || "";
  } catch (err) {
    results.checks.badBaseTitle = "ERR: " + err.message;
  }

  // Wait > 1.2s so URL.revokeObjectURL delayed setTimeout(1000) fires and surfaces any async Invalid URL errors!
  await new Promise((r) => setTimeout(r, 1300));

  document.getElementById("status").textContent = "done";
  window.__fixtureDone = true;
})();
</script>
</body>
</html>
"""

WIDGET_HTML = r"""<!doctype html>
<html>
<head><meta charset="utf-8"><title>Widget Fixture</title></head>
<body>
<p>Widget</p>
<script>
let mismatchCount = 0;
window.addEventListener("message", (e) => {
  if (e.data === "ping-string") {
    const info = "pong:origin=" + e.origin + ":sourceIsParent=" + (e.source === window.parent);
    e.source.postMessage(info, e.origin);
    return;
  }
  if (!e.data || typeof e.data !== "object") return;
  if (e.data.cmd === "echo-obj-listener") {
    window.parent.postMessage({ kind: "obj-listener-reply", source: "widget-channel" }, "*");
  } else if (e.data.cmd === "send-null") {
    window.parent.postMessage(null, "*");
  } else if (e.data.cmd === "echo-null-proto") {
    const reply = Object.create(null);
    reply.kind = "null-proto-reply";
    reply.echoed = e.data.source;
    window.parent.postMessage(reply, "*");
  } else if (e.data.cmd === "use-port") {
    const port = e.ports && e.ports[0];
    if (port) {
      port.postMessage("mc-str");
      port.postMessage({ source: "mc-obj" });
      port.postMessage(null);
    }
  } else if (e.data.cmd === "should-not-deliver") {
    mismatchCount++;
  } else if (e.data.cmd === "send-wrong-to-parent") {
    window.parent.postMessage("bad-child-to-parent", "https://wrong.fixture.test");
    window.parent.postMessage("bad-child-to-parent");
  } else if (e.data.cmd === "get-mismatch-count") {
    window.parent.postMessage({ kind: "mismatch-count", count: mismatchCount }, "*");
  }
});
window.parent.postMessage("widget-ready", "*");
</script>
</body>
</html>
"""

BAD_BASE_HTML = r"""<!doctype html>
<html>
<head><meta charset="utf-8"><base href="http://[::1"><title>Bad Base OK</title></head>
<body><p>Bad base survived</p></body>
</html>
"""


def free_port():
    with socket.socket() as s:
        s.bind(("127.0.0.1", 0))
        return s.getsockname()[1]


async def main():
    port = free_port()

    async def handle_sw(req):
        resp = web.FileResponse(WEB_DIR / "sw.js")
        resp.headers["Service-Worker-Allowed"] = "/"
        resp.headers["Cache-Control"] = "no-store"
        return resp

    async def handle_harness(req):
        return web.Response(text=HARNESS_HTML, content_type="text/html")

    async def handle_loopback_fetch(req):
        target = req.query.get("target", "")
        if target.startswith("https://parent.fixture.test/not-modified"):
            return web.Response(
                status=200,
                body=b"",
                headers={
                    "x-fixture-status": "304",
                    "x-fixture-statustext": "Not Modified",
                    "Cache-Control": "no-store",
                },
            )
        if target.startswith("https://parent.fixture.test/bad-base"):
            return web.Response(
                text=BAD_BASE_HTML,
                content_type="text/html",
                headers={"Cache-Control": "no-store"},
            )
        if target.startswith("https://parent.fixture.test/"):
            return web.Response(
                text=PARENT_HTML,
                content_type="text/html",
                headers={"Cache-Control": "no-store"},
            )
        if target.startswith("https://widget.fixture.test/"):
            return web.Response(
                text=WIDGET_HTML,
                content_type="text/html",
                headers={"Cache-Control": "no-store"},
            )
        return web.Response(status=404, text="Not found: " + target)

    app = web.Application()
    app.router.add_get("/favicon.ico", lambda req: web.Response(status=204))
    app.router.add_get("/sw.js", handle_sw)
    app.router.add_get("/harness.html", handle_harness)
    app.router.add_route("*", "/__loopback_fetch", handle_loopback_fetch)
    app.router.add_static("/assets/", WEB_DIR / "assets")

    runner = web.AppRunner(app)
    await runner.setup()
    site = web.TCPSite(runner, "127.0.0.1", port)
    await site.start()

    base = f"http://127.0.0.1:{port}"
    page_errors = []
    console_errors = []

    try:
        async with async_playwright() as pw:
            chrome_candidates = [
                os.environ.get("CHROME_PATH", ""),
                r"C:\Program Files\Google\Chrome\Application\chrome.exe",
                r"C:\Program Files (x86)\Microsoft\Edge\Application\msedge.exe",
                "/usr/bin/chromium",
                "/usr/bin/google-chrome",
            ]
            exe = next((p for p in chrome_candidates if p and Path(p).exists()), None)
            browser = await pw.chromium.launch(
                executable_path=exe,
                headless=True,
                args=["--no-sandbox", "--disable-gpu"],
            )
            ctx = await browser.new_context()
            page = await ctx.new_page()
            page.on("pageerror", lambda err: page_errors.append(str(err)))
            page.on(
                "console",
                lambda msg: console_errors.append(msg.text)
                if msg.type == "error"
                else None,
            )
            page.on(
                "response",
                lambda r: console_errors.append(f"HTTP {r.status} {r.url}")
                if r.status >= 400
                else None,
            )

            await page.goto(f"{base}/harness.html", wait_until="domcontentloaded")
            await page.evaluate("url => window.__startFixture(url)", "https://parent.fixture.test/")

            # Wait for the proxied parent frame to finish all checks
            await page.wait_for_function(
                """() => {
                  const f = document.querySelector('#mount iframe');
                  return f && f.contentWindow && f.contentWindow.__fixtureDone === true;
                }""",
                timeout=20000,
            )

            shell_errors = await page.evaluate("window.__harnessErrors")
            fixture_results = await page.evaluate(
                "document.querySelector('#mount iframe').contentWindow.__fixtureResults"
            )

            # Reload the proxied frame and verify all checks still pass identically after reload
            await page.evaluate("""() => {
              const f = document.querySelector('#mount iframe');
              f.contentWindow.__fixtureDone = false;
              window.__frame.reload();
            }""")
            await page.wait_for_function(
                """() => {
                  const f = document.querySelector('#mount iframe');
                  return f && f.contentWindow && f.contentWindow.__fixtureDone === true;
                }""",
                timeout=20000,
            )
            reload_results = await page.evaluate(
                "document.querySelector('#mount iframe').contentWindow.__fixtureResults"
            )
            shell_errors = await page.evaluate("window.__harnessErrors")
            await browser.close()
    finally:
        await runner.cleanup()

    report = {
        "checks": fixture_results["checks"],
        "reload_checks": reload_results["checks"],
        "iframe_errors": fixture_results["errors"] + reload_results["errors"],
        "shell_errors": shell_errors,
        "page_errors": page_errors,
        "console_errors": console_errors,
    }
    print(json.dumps(report, indent=2, ensure_ascii=False))

    # Strict assertions (for both initial load and post-reload)
    failures = []
    def check_suite(prefix, c):
        def expect(name, cond, detail=None):
            if not cond:
                failures.append(f"{prefix}{name}: {detail if detail is not None else c.get(name)}")

        expect("widgetReadyOrigin", c.get("widgetReadyOrigin") == "https://widget.fixture.test")
        expect("widgetReadySourceMatches", c.get("widgetReadySourceMatches") is True)
        expect("protoSourceGetterOk", c.get("protoSourceGetterOk") is True)
        expect(
            "stringHandshakeData",
            c.get("stringHandshakeData") == "pong:origin=https://parent.fixture.test:sourceIsParent=true",
        )
        expect("stringHandshakeOrigin", c.get("stringHandshakeOrigin") == "https://widget.fixture.test")
        expect("stringHandshakeSourceMatches", c.get("stringHandshakeSourceMatches") is True)
        expect(
            "objectListener",
            isinstance(c.get("objectListener"), dict)
            and c["objectListener"].get("origin") == "https://widget.fixture.test"
            and c["objectListener"].get("sourceMatches") is True
            and c["objectListener"].get("sourceField") == "widget-channel",
        )
        expect("nullDataOrigin", c.get("nullDataOrigin") == "https://widget.fixture.test")
        expect("nullDataValue", c.get("nullDataValue") is None and "nullData" not in c)
        expect("nullDataSourceMatches", c.get("nullDataSourceMatches") is True)
        expect(
            "nullProtoReply",
            isinstance(c.get("nullProtoReply"), dict)
            and c["nullProtoReply"].get("origin") == "https://widget.fixture.test"
            and c["nullProtoReply"].get("echoed") == "parent-null-proto",
        )
        expect("messageChannel", c.get("messageChannel") == ["mc-str", {"source": "mc-obj"}, None])
        expect("emptyTargetOriginThrowsSyntaxError", c.get("emptyTargetOriginThrowsSyntaxError") is True)
        expect("sameOriginDefaultTargetOk", c.get("sameOriginDefaultTargetOk") is True)
        expect("targetOriginMismatchBlocked", c.get("targetOriginMismatchBlocked") is True)
        expect("childToParentMismatchBlocked", c.get("childToParentMismatchBlocked") is True)
        expect(
            "aboutBlankPostMessage",
            isinstance(c.get("aboutBlankPostMessage"), dict)
            and c["aboutBlankPostMessage"].get("origin") == "https://parent.fixture.test"
            and c["aboutBlankPostMessage"].get("sourceMatches") is True,
        )
        expect("aboutBlankWorkerReply", c.get("aboutBlankWorkerReply") == "worker:hello")
        expect("aboutBlankSrcdocOk", c.get("aboutBlankSrcdocOk") is True)
        expect(
            "aboutBlankBaseUri",
            isinstance(c.get("aboutBlankBaseUri"), str) and "/sub/" in c["aboutBlankBaseUri"],
        )
        expect(
            "anchorPropsOk",
            isinstance(c.get("anchorPropsOk"), dict)
            and c["anchorPropsOk"].get("a2Hash") == "#section"
            and c["anchorPropsOk"].get("a3Proto") == "javascript:",
        )
        expect("fetch304Status", c.get("fetch304Status") == 304)
        expect("badBaseTitle", c.get("badBaseTitle") == "Bad Base OK")

    check_suite("initial.", report["checks"])
    check_suite("reload.", report["reload_checks"])
    if len(report["iframe_errors"]) != 0:
        failures.append(f"zero_iframe_errors: {report['iframe_errors']}")
    if len(report["shell_errors"]) != 0:
        failures.append(f"zero_shell_errors: {report['shell_errors']}")
    if len(report["page_errors"]) != 0:
        failures.append(f"zero_page_errors: {report['page_errors']}")
    if len(report["console_errors"]) != 0:
        failures.append(f"zero_console_errors: {report['console_errors']}")

    if failures:
        print("\nFAILED CHECKS:", file=sys.stderr)
        for f in failures:
            print("  -", f, file=sys.stderr)
        sys.exit(1)
    print("\nALL SCRAMJET RUNTIME CHECKS PASSED!")


if __name__ == "__main__":
    asyncio.run(main())
