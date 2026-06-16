// Offline episode storage — imported by the Rust app as a wasm-bindgen module
// snippet (see `#[wasm_bindgen(module = "/storage.js")]` in src/main.rs).
//
// Why IndexedDB + blob: object URLs (and NOT the Cache API): an <audio> element
// issues HTTP Range requests when seeking, and the Cache API serves only the
// full 200 response — it ignores Range, which breaks scrubbing unless a service
// worker hand-synthesizes 206 responses. A blob: URL minted from an IndexedDB
// Blob is range-served natively by the browser, fully offline, with no SW. So
// downloaded audio lives here, entirely separate from the app-shell sw.js.
//
// CORS note: fetch()-ing the mp3 to store it DOES require the host to send
// Access-Control-Allow-Origin (unlike <audio src> streaming, which never does).
// When that fails we return a reason string and the app keeps streaming online.

const DB_NAME = "podcasts";
const STORE = "episodes";
const VERSION = 1;

function openDB() {
  return new Promise((resolve, reject) => {
    const req = indexedDB.open(DB_NAME, VERSION);
    req.onupgradeneeded = () => {
      const db = req.result;
      if (!db.objectStoreNames.contains(STORE)) {
        db.createObjectStore(STORE, { keyPath: "key" });
      }
    };
    req.onsuccess = () => resolve(req.result);
    req.onerror = () => reject(req.error);
  });
}

function idbGet(key) {
  return openDB().then(
    (db) =>
      new Promise((resolve, reject) => {
        const tx = db.transaction(STORE, "readonly");
        const r = tx.objectStore(STORE).get(key);
        r.onsuccess = () => resolve(r.result || null);
        r.onerror = () => reject(r.error);
        // A transaction abort (db close / version change) fires neither
        // onsuccess nor onerror — without this the Promise would hang.
        tx.onabort = () => reject(tx.error || new Error("aborted"));
      }),
  );
}

function idbPut(value) {
  return openDB().then(
    (db) =>
      new Promise((resolve, reject) => {
        const tx = db.transaction(STORE, "readwrite");
        tx.objectStore(STORE).put(value);
        tx.oncomplete = () => resolve();
        tx.onerror = () => reject(tx.error);
        tx.onabort = () => reject(tx.error || new Error("aborted"));
      }),
  );
}

function idbDelete(key) {
  return openDB().then(
    (db) =>
      new Promise((resolve, reject) => {
        const tx = db.transaction(STORE, "readwrite");
        tx.objectStore(STORE).delete(key);
        tx.oncomplete = () => resolve();
        tx.onerror = () => reject(tx.error);
        tx.onabort = () => reject(tx.error || new Error("aborted"));
      }),
  );
}

function idbKeys() {
  return openDB().then(
    (db) =>
      new Promise((resolve, reject) => {
        const tx = db.transaction(STORE, "readonly");
        const r = tx.objectStore(STORE).getAllKeys();
        r.onsuccess = () => resolve(r.result || []);
        r.onerror = () => reject(r.error);
        tx.onabort = () => reject(tx.error || new Error("aborted"));
      }),
  );
}

async function quotaShortfall(bytes) {
  // true if we can see a quota and `bytes` (plus headroom) won't fit.
  try {
    if (navigator.storage && navigator.storage.estimate) {
      const { usage = 0, quota = 0 } = await navigator.storage.estimate();
      return quota > 0 && quota - usage < bytes * 1.1;
    }
  } catch {
    /* ignore — let the write attempt surface QuotaExceededError */
  }
  return false;
}

async function persistBlob(key, blob, onProgress) {
  if (await quotaShortfall(blob.size)) return "quota";
  try {
    await idbPut({ key, blob, size: blob.size, mime: blob.type, savedAt: Date.now() });
  } catch (e) {
    return e && e.name === "QuotaExceededError" ? "quota" : "store";
  }
  if (onProgress) onProgress(blob.size, blob.size);
  return "ok";
}

/// Download `url` and store it under `key`. Reports (received,total) bytes via
/// onProgress (total=0 when the host omits Content-Length). Returns a status:
/// "ok" | "cors" | "quota" | "store" | "http_<code>". Idempotent.
export async function downloadEpisode(key, url, onProgress) {
  const existing = await idbGet(key).catch(() => null);
  if (existing && existing.blob) return "ok";

  // A download tap is a user gesture, the best moment to ask for durable storage
  // (best-effort storage is evicted under pressure / after 7 idle days on Safari).
  try {
    if (navigator.storage && navigator.storage.persist) await navigator.storage.persist();
  } catch {
    /* non-fatal */
  }

  let resp;
  try {
    // NEVER mode:'no-cors' — an opaque response is unreadable and unslicable and
    // pads quota by ~7MB/entry. A CORS/network failure means "stream online".
    resp = await fetch(url, { mode: "cors" });
  } catch {
    return "cors";
  }
  if (!resp.ok) return "http_" + resp.status;

  const mime = resp.headers.get("Content-Type") || "audio/mpeg";
  const total = Number(resp.headers.get("Content-Length")) || 0;
  if (total > 0 && (await quotaShortfall(total))) return "quota";

  if (!resp.body || !resp.body.getReader) {
    return await persistBlob(key, await resp.blob(), onProgress);
  }

  const reader = resp.body.getReader();
  const chunks = [];
  let received = 0;
  let lastPct = -1;
  for (;;) {
    const { done, value } = await reader.read();
    if (done) break;
    chunks.push(value);
    received += value.length;
    if (onProgress) {
      const pct = total > 0 ? Math.floor((received / total) * 100) : -1;
      if (pct !== lastPct) {
        lastPct = pct;
        onProgress(received, total);
      }
    }
  }
  return await persistBlob(key, new Blob(chunks, { type: mime }), onProgress);
}

/// Mint a blob: object URL for the stored episode, or null if not downloaded.
/// Caller must revokeObjectUrl() it when done (on episode switch / unmount).
export async function getObjectUrl(key) {
  const rec = await idbGet(key).catch(() => null);
  if (!rec || !rec.blob) return null;
  return URL.createObjectURL(rec.blob);
}

export function revokeObjectUrl(u) {
  if (u) {
    try {
      URL.revokeObjectURL(u);
    } catch {
      /* already revoked */
    }
  }
}

export async function deleteEpisode(key) {
  await idbDelete(key).catch(() => {});
}

/// Keys (= episode audio_url identities) of all downloaded episodes.
export async function listDownloaded() {
  return await idbKeys().catch(() => []);
}

/// Lightweight metadata for every downloaded episode: { key, savedAt } per
/// record, for the startup auto-remove sweep (which needs savedAt — the download
/// time — to decide staleness; listDownloaded returns keys only). Uses a cursor
/// and reads ONLY key + savedAt so the audio blobs are never materialized into
/// the JS heap (getAll() would deserialize every Blob just to read two fields).
/// A record missing savedAt (none should exist — it shipped with the field)
/// reports 0, which the Rust predicate treats as "unknown age → never sweep".
export async function listDownloadedMeta() {
  return openDB()
    .then(
      (db) =>
        new Promise((resolve, reject) => {
          const tx = db.transaction(STORE, "readonly");
          const out = [];
          const r = tx.objectStore(STORE).openCursor();
          r.onsuccess = () => {
            const cur = r.result;
            if (!cur) {
              resolve(out);
              return;
            }
            out.push({ key: cur.key, savedAt: (cur.value || {}).savedAt || 0 });
            cur.continue();
          };
          r.onerror = () => reject(r.error);
          tx.onabort = () => reject(tx.error || new Error("aborted"));
        }),
    )
    .catch(() => []);
}

/// Total bytes this origin is using (dominated by the audio blobs); for the UI's
/// storage line. Estimated/padded by the browser — display only.
export async function estimateStorage() {
  try {
    if (navigator.storage && navigator.storage.estimate) {
      const { usage = 0 } = await navigator.storage.estimate();
      return usage;
    }
  } catch {
    /* ignore */
  }
  return 0;
}
