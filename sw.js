// Service worker for the Podcasts PWA — app-shell caching for offline launch.
//
// Trunk content-hashes the WASM/JS/CSS filenames every build, so we CANNOT ship
// a static precache list of them. Instead this worker caches the app shell at
// RUNTIME (cache-on-fetch): whatever hashed asset the page actually requests is
// cached on first load, and a CACHE bump (below) drops the previous build's
// orphaned assets. This file is copied verbatim into dist by Trunk
// (rel="copy-file"), so it keeps a STABLE url (/audio-player/sw.js → scope
// /audio-player/) across deploys — which is what lets the browser detect updates.
//
// Offline *audio* is NOT handled here: episodes are downloaded by the app into
// IndexedDB and played back via blob: object URLs (the Cache API can't serve the
// Range requests an <audio> element makes). The cross-origin mp3 streams and the
// download fetches are therefore deliberately left untouched by this worker.
//
// Bump CACHE on every release: it both changes this file's bytes (forcing the
// browser to install the new worker) and namespaces a fresh cache so activate()
// can evict the old build's assets.
const CACHE = "audio-player-shell-v3";

// Only STABLE (non-hashed) URLs belong here; hashed assets are runtime-cached.
// './' and './index.html' are the offline navigation fallback target; feeds.json
// is precached so the episode list is available offline from the very first
// visit (network-first below still refreshes it when online). The hashed
// wasm/js/css are populated at runtime (their names change per build).
const SHELL = ["./", "./index.html", "./feeds.json", "./manifest.webmanifest"];

self.addEventListener("install", (event) => {
  event.waitUntil(
    (async () => {
      const cache = await caches.open(CACHE);
      // Per-URL (not addAll) so one missing/redirected/stale file — e.g. a
      // feeds.json a failed feedsync left behind — can't fail the whole install
      // and leave the app with no offline shell at all. {cache:'reload'}
      // bypasses the HTTP cache so we precache a fresh shell.
      await Promise.all(
        SHELL.map((u) =>
          cache.add(new Request(u, { cache: "reload" })).catch(() => {}),
        ),
      );
      await self.skipWaiting();
    })(),
  );
});

// First-visit warm-up: assets fetched before this worker took control aren't
// intercepted, so they never hit the runtime cache and the first offline reload
// would fail. The page posts the URLs it actually loaded (from Performance
// entries) right after we claim it; we fetch+cache any we don't already have.
self.addEventListener("message", (event) => {
  const data = event.data;
  if (!data || data.type !== "warm" || !Array.isArray(data.urls)) return;
  event.waitUntil(
    (async () => {
      const cache = await caches.open(CACHE);
      await Promise.all(
        data.urls.map(async (u) => {
          try {
            // Defense-in-depth: the page only posts same-origin URLs, but don't
            // trust that — an XSS'd page could post cross-origin URLs to poison
            // the shell cache. Re-validate origin here (the fetch handler does
            // the same for intercepted requests).
            if (new URL(u, self.location.href).origin !== self.location.origin) return;
            if (await cache.match(u)) return;
            const res = await fetch(u, { cache: "no-cache" });
            if (res.ok) await cache.put(u, res);
          } catch {
            /* best-effort */
          }
        }),
      );
    })(),
  );
});

self.addEventListener("activate", (event) => {
  event.waitUntil(
    (async () => {
      const keys = await caches.keys();
      await Promise.all(keys.filter((k) => k !== CACHE).map((k) => caches.delete(k)));
      await self.clients.claim();
    })(),
  );
});

self.addEventListener("fetch", (event) => {
  const req = event.request;
  if (req.method !== "GET") return;

  const url = new URL(req.url);

  // Never intercept cross-origin requests: mp3 audio streams (CORS-free <audio>
  // playback) and the IndexedDB download fetches must pass straight through.
  if (url.origin !== self.location.origin) return;

  // SPA navigations: network-first so an online user always gets the freshest
  // index.html (which references the current build's hashed bundle); fall back
  // to the cached shell when offline.
  if (req.mode === "navigate") {
    event.respondWith(
      (async () => {
        const cache = await caches.open(CACHE);
        try {
          const fresh = await fetch(req);
          cache.put("./index.html", fresh.clone());
          return fresh;
        } catch {
          return (
            (await cache.match(req)) ||
            (await cache.match("./index.html")) ||
            (await cache.match("./"))
          );
        }
      })(),
    );
    return;
  }

  // feeds.json is generated data with a STABLE name, so it can't be cache-busted
  // by a hash — go network-first to keep the episode list fresh online, falling
  // back to the last cached copy offline.
  if (url.pathname.endsWith("/feeds.json") || url.pathname.endsWith("feeds.json")) {
    event.respondWith(
      (async () => {
        const cache = await caches.open(CACHE);
        try {
          const fresh = await fetch(req);
          if (fresh.ok) cache.put(req, fresh.clone());
          return fresh;
        } catch {
          return (await cache.match(req)) || Response.error();
        }
      })(),
    );
    return;
  }

  // Everything else same-origin (hashed wasm/js/css, icons, manifest):
  // stale-while-revalidate. A cache hit returns instantly; we refresh in the
  // background. Hashed names make a hit always the correct bytes; new builds use
  // new URLs (cached on first fetch) and old ones are purged on the next bump.
  event.respondWith(
    (async () => {
      const cache = await caches.open(CACHE);
      const cached = await cache.match(req);
      const network = fetch(req)
        .then((res) => {
          if (res.ok) cache.put(req, res.clone());
          return res;
        })
        .catch(() => cached);
      return cached || network;
    })(),
  );
});
