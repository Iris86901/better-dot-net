importScripts('/assets/rt/controller.sw.js?v=2');

const LEGACY_CACHE_PREFIXES = ['dot-proxy-static-'];

self.addEventListener('activate', (event) => {
  event.waitUntil((async () => {
    try {
      const keys = await caches.keys();
      await Promise.all(
        keys
          .filter((k) => LEGACY_CACHE_PREFIXES.some((p) => k.startsWith(p)))
          .map((k) => caches.delete(k))
      );
    } catch {}
  })());
});

const SHELL_EXACT_PATHS = new Set([
  '/',
  '/index.html',
  '/auth',
  '/logout',
  '/health',
  '/ws',
  '/ws/',
  '/sw.js',
  '/app.js',
  '/styles.css',
  '/favicon.svg',
  '/lifecycle.js',
  '/api/routing',
  '/api/routing/check',
  '/api/routing/subscription',
]);

function isShellAssetOrApi(pathname) {
  return (
    (pathname !== '/' && SHELL_EXACT_PATHS.has(pathname)) ||
    pathname.startsWith('/assets/rt/')
  );
}

// Parse a Scramjet v2 frame URL: /p/<controllerId>/<frameId>/<encodedUrl>
// Returns { framePrefix, targetUrl, targetOrigin } or null.
function decodeProxyContext(proxyUrl) {
  if (!proxyUrl) return null;
  try {
    const u = new URL(proxyUrl);
    if (u.origin !== self.location.origin) return null;
    const match = u.pathname.match(/^(\/p\/[^/]+\/[^/]+\/)(.+)$/);
    if (!match) return null;
    const framePrefix = match[1];
    const encoded = match[2];
    const raw = decodeURIComponent(encoded);
    const target = new URL(raw);
    if (!/^https?:$/.test(target.protocol)) return null;
    return { framePrefix, targetUrl: target.href, targetOrigin: target.origin };
  } catch {
    return null;
  }
}

function encodeProxyTarget(framePrefix, targetUrl) {
  const u = new URL(targetUrl);
  const hash = u.hash ? '#' + encodeURIComponent(u.hash.slice(1)) : '';
  u.hash = '';
  return self.location.origin + framePrefix + encodeURIComponent(u.href) + hash;
}

async function resolveEscapedTarget(event, url) {
  // Never hijack gateway shell assets/APIs; only recover requests originating from a /p/ frame
  // whose root-relative form/navigation/fetch escaped Scramjet's prefix (e.g. /search).
  if (isShellAssetOrApi(url.pathname)) return null;
  if (url.pathname === '/' && event.request.mode !== 'navigate') return null;

  let ctx = decodeProxyContext(event.request.referrer);
  if (!ctx && event.clientId) {
    try {
      const client = await self.clients.get(event.clientId);
      ctx = decodeProxyContext(client?.url);
    } catch {}
  }
  if (!ctx) return null;
  try {
    const resolved = new URL(url.pathname + url.search + url.hash, ctx.targetOrigin);
    if (!/^https?:$/.test(resolved.protocol)) return null;
    return encodeProxyTarget(ctx.framePrefix, resolved.href);
  } catch {
    return null;
  }
}

function resolveOrphanedProxyNavigation(url) {
  const ctx = decodeProxyContext(url.href);
  if (!ctx) return null;
  return self.location.origin + '/?goto=' + encodeURIComponent(ctx.targetUrl);
}

self.addEventListener('fetch', (event) => {
  const url = new URL(event.request.url);
  if (url.origin !== self.location.origin) return;

  if ($scramjetController.shouldRoute(event)) {
    event.respondWith($scramjetController.route(event));
    return;
  }

  // If a top-level navigation hits a stale /p/<cid>/<fid>/<url> (e.g. native "Open in new tab"),
  // hand it back to the proxy shell with ?goto= so the user stays inside the browser UI.
  if (event.request.mode === 'navigate' && url.pathname.startsWith('/p/')) {
    const shellUrl = resolveOrphanedProxyNavigation(url);
    if (shellUrl) {
      event.respondWith(Response.redirect(shellUrl, 302));
      return;
    }
  }

  if (isShellAssetOrApi(url.pathname)) return;

  event.respondWith((async () => {
    const recovered = await resolveEscapedTarget(event, url);
    if (recovered) {
      return Response.redirect(recovered, 307);
    }
    return fetch(event.request);
  })());
});
