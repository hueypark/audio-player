use std::collections::{HashMap, HashSet};

use leptos::prelude::*;
use serde::Deserialize;
use wasm_bindgen::prelude::*;
use wasm_bindgen_futures::{spawn_local, JsFuture};
use web_sys::{
    HtmlAudioElement, MediaImage, MediaMetadata, MediaMetadataInit, MediaPositionState, MediaSession,
    MediaSessionAction, MediaSessionActionDetails, MediaSessionPlaybackState,
};

// Offline storage bridge — IndexedDB download/playback lives in storage.js (see
// that file for why blob: object URLs instead of the Cache API). wasm-bindgen
// bundles this module as a local JS snippet; async fns surface as Promises.
#[wasm_bindgen(module = "/storage.js")]
extern "C" {
    #[wasm_bindgen(js_name = downloadEpisode)]
    fn js_download_episode(key: &str, url: &str, on_progress: &js_sys::Function) -> js_sys::Promise;
    #[wasm_bindgen(js_name = getObjectUrl)]
    fn js_get_object_url(key: &str) -> js_sys::Promise;
    #[wasm_bindgen(js_name = revokeObjectUrl)]
    fn js_revoke_object_url(url: &str);
    #[wasm_bindgen(js_name = deleteEpisode)]
    fn js_delete_episode(key: &str) -> js_sys::Promise;
    #[wasm_bindgen(js_name = listDownloaded)]
    fn js_list_downloaded() -> js_sys::Promise;
    #[wasm_bindgen(js_name = estimateStorage)]
    fn js_estimate_storage() -> js_sys::Promise;
}

#[derive(Clone, Debug, Deserialize)]
struct Episode {
    title: String,
    audio_url: String,
    /// A CORS-fetchable direct URL for offline download, resolved server-side by
    /// feedsync (some hosts 302-redirect to a CDN and drop CORS on the redirect,
    /// which blocks an in-browser download of `audio_url`). Absent → fall back to
    /// `audio_url` for the download attempt. `audio_url` stays the streaming src
    /// and the stable identity/key everywhere.
    #[serde(default)]
    download_url: Option<String>,
}

impl Episode {
    /// The URL to `fetch()` for offline download. `audio_url` is the stable
    /// identity used everywhere else (resume-position key, IndexedDB key, `<For>`
    /// key); only the download fetch prefers the resolved `download_url`.
    fn fetch_url(&self) -> &str {
        self.download_url.as_deref().unwrap_or(&self.audio_url)
    }
}

#[derive(Clone, Debug, Deserialize)]
struct Podcast {
    title: String,
    #[serde(default)]
    episodes: Vec<Episode>,
}

#[derive(Clone, Debug, Deserialize)]
struct Feeds {
    podcasts: Vec<Podcast>,
}

/// The single persistent `<audio>` element used for all playback.
fn audio_el() -> Option<HtmlAudioElement> {
    web_sys::window()?
        .document()?
        .get_element_by_id("player")?
        .dyn_into::<HtmlAudioElement>()
        .ok()
}

fn storage() -> Option<web_sys::Storage> {
    web_sys::window()?.local_storage().ok().flatten()
}

fn save_pos(url: &str, secs: f64) {
    if let Some(s) = storage() {
        let _ = s.set_item(&format!("pos:{url}"), &secs.to_string());
    }
}

fn load_pos(url: &str) -> f64 {
    storage()
        .and_then(|s| s.get_item(&format!("pos:{url}")).ok().flatten())
        .and_then(|v| v.parse::<f64>().ok())
        .unwrap_or(0.0)
}

// Episode duration (seconds), persisted so a list progress bar can be drawn for
// episodes that aren't currently loaded in the <audio> element. `pos:{url}` alone
// can't yield a fraction without it, and feeds.json carries no duration. Learned
// from playback (loadedmetadata/durationchange) and keyed by the same audio_url.

fn save_dur(url: &str, secs: f64) {
    if let Some(s) = storage() {
        let _ = s.set_item(&format!("dur:{url}"), &secs.to_string());
    }
}

fn load_dur(url: &str) -> f64 {
    storage()
        .and_then(|s| s.get_item(&format!("dur:{url}")).ok().flatten())
        .and_then(|v| v.parse::<f64>().ok())
        .unwrap_or(0.0)
}

// When an episode was last listened to (epoch-ms). `pos:`/`dur:` say *how far*
// you got, never *when*; the 이어듣기 (Continue Listening) section needs recency
// to order the episodes you're juggling, most-recent first. Stamped at episode
// switch (in `play`), keyed by the same audio_url. Stored as a plain f64 string,
// mirroring pos:/dur:.
//
// Like pos:/dur:, seen: is intentionally never garbage-collected: feeds.json is a
// rolling window, so an episode absent from it today may reappear, and pruning
// keys for "missing" episodes would erase real listening progress. Growth is
// bounded — three small keys per episode ever played — and far from the storage
// quota for a curated feed.

fn save_seen(url: &str, ts: f64) {
    if let Some(s) = storage() {
        let _ = s.set_item(&format!("seen:{url}"), &ts.to_string());
    }
}

fn load_seen(url: &str) -> f64 {
    storage()
        .and_then(|s| s.get_item(&format!("seen:{url}")).ok().flatten())
        .and_then(|v| v.parse::<f64>().ok())
        // Reject NaN/inf (only reachable via manual storage tampering) so the
        // backfill heals it and the recency sort keeps a total order. Mirrors
        // detail_f64's finiteness guard.
        .filter(|v| v.is_finite())
        .unwrap_or(0.0)
}

// Listened fraction (0..1) at/above which an episode is auto-classified as
// *played* (finished). 0.95 mirrors the ~95%-of-duration heuristic Pocket Casts
// and Overcast use: a listener usually stops a little before the true end
// (outro/credits), so requiring a full 100% would leave finished episodes stuck
// reading "in progress". This is the only state line in a three-state model —
// below it (and above 0) is "in progress", exactly 0 is "unplayed". Detection is
// automatic; there is no manual mark-as-played override.
const PLAYED_FRAC: f64 = 0.95;

// Sentinel "podcast title" for the 이어듣기 (Continue Listening) section, so it
// can reuse the per-podcast collapse machinery (collapsed:{title}, the <For>
// key, toggle_collapsed) without colliding with any real Podcast.title — a feed
// title can never contain a NUL byte.
const CONTINUE_KEY: &str = "\u{0}continue";

// ---- Collapsed podcasts (which episode lists are folded; survives a refresh) --
//
// Mirrors the per-key localStorage pattern (`pos:`/`dur:`): one boolean key per
// podcast, keyed by title (the same identity used as the <For> key). An absent
// key == expanded (the default), so a fresh visitor sees every list open.

fn save_collapsed(title: &str, collapsed: bool) {
    if let Some(s) = storage() {
        let key = format!("collapsed:{title}");
        if collapsed {
            let _ = s.set_item(&key, "1");
        } else {
            let _ = s.remove_item(&key);
        }
    }
}

fn load_collapsed(title: &str) -> bool {
    storage()
        .and_then(|s| s.get_item(&format!("collapsed:{title}")).ok().flatten())
        .is_some()
}

/// Fold/unfold a podcast's episode list and persist the choice. Shared by the
/// header's click and keyboard (Enter/Space) handlers.
fn toggle_collapsed(collapsed: RwSignal<HashSet<String>>, title: &str) {
    collapsed.update(|s| {
        if s.remove(title) {
            save_collapsed(title, false);
        } else {
            s.insert(title.to_string());
            save_collapsed(title, true);
        }
    });
}

// ---- Last-played episode (survives a page refresh) ---------------------------
//
// `pos:{url}` already remembers *where* you were in each episode; these remember
// *which* episode so a refresh reloads it instead of starting empty. Stored as
// three plain keys (no escaping, no serde_json) — written together in `play`.
// `audio_url` stays the identity; title/artist are kept verbatim so the footer
// and Media Session restore without re-reading feeds.json (works offline too).

fn save_last_played(url: &str, title: &str, artist: &str) {
    if let Some(s) = storage() {
        // Write `last:url` LAST as the commit marker: load_last_played keys off
        // it, so a mid-write quota failure (url never reached) reads as "nothing
        // saved" rather than a url paired with a blank title/artist.
        let _ = s.set_item("last:title", title);
        let _ = s.set_item("last:artist", artist);
        let _ = s.set_item("last:url", url);
    }
}

fn load_last_played() -> Option<(String, String, String)> {
    let s = storage()?;
    let url = s
        .get_item("last:url")
        .ok()
        .flatten()
        .filter(|v| !v.is_empty())?;
    let title = s.get_item("last:title").ok().flatten().unwrap_or_default();
    let artist = s.get_item("last:artist").ok().flatten().unwrap_or_default();
    Some((url, title, artist))
}

// ---- Media Session (lock-screen metadata + transport controls) --------------

fn media_session() -> Option<MediaSession> {
    Some(web_sys::window()?.navigator().media_session())
}

/// Set the lock-screen "now playing" card. `artwork_src` is resolved by the
/// browser against the document base, so a relative `icons/...` works under the
/// `/audio-player/` subpath and in dev.
fn set_now_playing(title: &str, artist: &str, artwork_src: &str) {
    let Some(session) = media_session() else {
        return;
    };
    let art = MediaImage::new(artwork_src);
    art.set_sizes("512x512");
    art.set_type("image/png");

    let init = MediaMetadataInit::new();
    init.set_title(title);
    init.set_artist(artist);
    init.set_album(artist);
    init.set_artwork(&[art]); // sequence<MediaImage> → &[MediaImage]

    if let Ok(meta) = MediaMetadata::new_with_init(&init) {
        session.set_metadata(Some(&meta));
    }
}

fn set_playback(playing: bool) {
    if let Some(session) = media_session() {
        session.set_playback_state(if playing {
            MediaSessionPlaybackState::Playing
        } else {
            MediaSessionPlaybackState::Paused
        });
    }
}

/// Keep the lock-screen scrubber accurate. The spec rejects a non-finite or
/// non-positive duration (common before metadata loads / for live streams), so
/// guard before calling — an unguarded call throws and panics the wasm.
fn update_position_state() {
    let (Some(session), Some(a)) = (media_session(), audio_el()) else {
        return;
    };
    let dur = a.duration();
    if !dur.is_finite() || dur <= 0.0 {
        return;
    }
    let pos = a.current_time().clamp(0.0, dur);
    let rate = {
        let r = a.playback_rate();
        if r == 0.0 {
            1.0
        } else {
            r
        }
    };
    let state = MediaPositionState::new();
    state.set_duration(dur);
    state.set_position(pos);
    state.set_playback_rate(rate);
    session.set_position_state_with_state(&state);
}

/// Mirror the `<audio>` element's position/duration into reactive signals that
/// feed the currently-playing episode's in-list progress bar. Mirrors
/// `update_position_state`'s guard: a non-finite or non-positive duration
/// (pre-metadata, or a live stream) collapses to `0.0` == "unknown", which the
/// view renders as no bar. Duration is persisted (once, when it first becomes
/// known) so other episodes can draw their bar after a reload.
fn update_play_state(
    current: RwSignal<String>,
    play_pos: RwSignal<f64>,
    play_dur: RwSignal<f64>,
) {
    let Some(a) = audio_el() else {
        return;
    };
    let dur = a.duration();
    let dur = if dur.is_finite() && dur > 0.0 { dur } else { 0.0 };
    // Only touch play_dur (and persist) when it actually changes — not on every
    // ~4Hz timeupdate — to avoid a redundant localStorage write/notify per tick.
    if play_dur.get_untracked() != dur {
        play_dur.set(dur);
        if dur > 0.0 {
            let url = current.get_untracked();
            if !url.is_empty() {
                save_dur(&url, dur);
            }
        }
    }
    play_pos.set(a.current_time().max(0.0));
}

/// Read an optional f64 field (seekTime / seekOffset) off the action details.
/// web-sys 0.3.102 models `MediaSessionActionDetails` as a write-only dict with
/// no getters, so reach in via Reflect.
fn detail_f64(d: &MediaSessionActionDetails, key: &str) -> Option<f64> {
    js_sys::Reflect::get(d.as_ref(), &JsValue::from_str(key))
        .ok()
        .and_then(|v| v.as_f64())
        // Reject NaN/Infinity: set_current_time on a non-finite value throws.
        .filter(|v| v.is_finite())
}

/// Wire the OS transport buttons to the single `<audio>` element. Each closure
/// is leaked (`forget`) to live for the page; handlers re-fetch `audio_el()` at
/// invocation so they need no captured state. Programmatic play/pause fire the
/// element's play/pause events, which update playbackState via the view.
fn register_media_handlers() {
    let Some(session) = media_session() else {
        return;
    };

    fn install(
        session: &MediaSession,
        action: MediaSessionAction,
        cb: Box<dyn FnMut(MediaSessionActionDetails)>,
    ) {
        let cl = Closure::wrap(cb);
        session.set_action_handler(action, Some(cl.as_ref().unchecked_ref()));
        cl.forget();
    }

    install(
        &session,
        MediaSessionAction::Play,
        Box::new(|_d| {
            if let Some(a) = audio_el() {
                let _ = a.play();
            }
        }),
    );
    install(
        &session,
        MediaSessionAction::Pause,
        Box::new(|_d| {
            if let Some(a) = audio_el() {
                let _ = a.pause();
            }
        }),
    );
    install(
        &session,
        MediaSessionAction::Seekbackward,
        Box::new(|d| {
            if let Some(a) = audio_el() {
                let off = detail_f64(&d, "seekOffset").unwrap_or(10.0);
                a.set_current_time((a.current_time() - off).max(0.0));
            }
        }),
    );
    install(
        &session,
        MediaSessionAction::Seekforward,
        Box::new(|d| {
            if let Some(a) = audio_el() {
                let off = detail_f64(&d, "seekOffset").unwrap_or(10.0);
                let dur = a.duration();
                let mut t = a.current_time() + off;
                if dur.is_finite() {
                    t = t.min(dur);
                }
                a.set_current_time(t);
            }
        }),
    );
    install(
        &session,
        MediaSessionAction::Seekto,
        Box::new(|d| {
            if let (Some(a), Some(t)) = (audio_el(), detail_f64(&d, "seekTime")) {
                a.set_current_time(t);
            }
        }),
    );
}

// ---- Offline download state helpers -----------------------------------------

/// Refresh the downloaded-key set and the storage-used figure from IndexedDB.
fn refresh_downloads(downloaded: RwSignal<HashSet<String>>, storage_used: RwSignal<f64>) {
    spawn_local(async move {
        if let Ok(v) = JsFuture::from(js_list_downloaded()).await {
            let mut set = HashSet::new();
            for item in js_sys::Array::from(&v).iter() {
                if let Some(s) = item.as_string() {
                    set.insert(s);
                }
            }
            downloaded.set(set);
        }
        if let Ok(v) = JsFuture::from(js_estimate_storage()).await {
            if let Some(bytes) = v.as_f64() {
                storage_used.set(bytes);
            }
        }
    });
}

/// On startup, reload the episode that was last played so a refresh keeps the
/// player populated. Mirrors `play`'s source resolution — a downloaded episode
/// plays from its IndexedDB blob (works offline), otherwise it streams the
/// `audio_url` — then lets the `loadedmetadata` handler seek to the saved resume
/// position. Left PAUSED on purpose: browsers block autoplay without a user
/// gesture and a refresh shouldn't blast audio; the user taps play to resume.
///
/// Two guards keep the async restore from doing harm: it bails if the user has
/// already started playback while it was resolving the blob (don't stomp their
/// choice), and it skips entirely when nothing is actually playable (no local
/// blob and offline) rather than show a card backed by a src that can't load.
fn restore_last_played(
    current: RwSignal<String>,
    now_title: RwSignal<String>,
    obj_url: RwSignal<Option<String>>,
    play_pos: RwSignal<f64>,
    play_dur: RwSignal<f64>,
) {
    let Some((key, title, artist)) = load_last_played() else {
        return;
    };
    spawn_local(async move {
        // Authoritative downloaded check straight from IndexedDB: the
        // `downloaded` signal may not be populated yet this early, and offline
        // playback must use the stored blob, never the unreachable network src.
        let is_downloaded = match JsFuture::from(js_list_downloaded()).await {
            Ok(v) => js_sys::Array::from(&v)
                .iter()
                .any(|i| i.as_string().as_deref() == Some(key.as_str())),
            Err(_) => false,
        };
        // A local blob plays offline; without one (not downloaded, or the blob
        // is gone/corrupt) we can only stream.
        let object_url = if is_downloaded {
            JsFuture::from(js_get_object_url(&key))
                .await
                .ok()
                .and_then(|v| v.as_string())
        } else {
            None
        };

        // The user tapped an episode while we resolved the blob (`current` is
        // empty until the first play/restore): don't stomp their choice, and
        // drop the object URL we minted but won't use.
        if !current.get_untracked().is_empty() {
            if let Some(u) = object_url {
                js_revoke_object_url(&u);
            }
            return;
        }
        // Nothing playable: no local blob and no network. Skip rather than show
        // a lock-screen card backed by a src that can't load — a doomed offline
        // fetch (a deleted download, or a stream with the network gone).
        let offline = web_sys::window()
            .map(|w| !w.navigator().on_line())
            .unwrap_or(false);
        if object_url.is_none() && offline {
            return;
        }

        let src = match object_url {
            Some(u) => {
                obj_url.set(Some(u.clone()));
                u
            }
            None => key.clone(),
        };
        if let Some(audio) = audio_el() {
            audio.set_src(&src);
            current.set(key.clone());
            // The seeded bar shows the saved point immediately; loadedmetadata
            // then drives it live. Reset so a stale duration can't skew it first.
            play_pos.set(0.0);
            play_dur.set(0.0);
            now_title.set(title.clone());
            audio.load(); // fires loadedmetadata → seeks to the saved position
        }
        set_now_playing(&title, &artist, "icons/icon-512.png");
        set_playback(false);
    });
}

fn fmt_bytes(b: f64) -> String {
    if b >= 1_073_741_824.0 {
        format!("{:.1} GB", b / 1_073_741_824.0)
    } else if b >= 1_048_576.0 {
        format!("{:.0} MB", b / 1_048_576.0)
    } else if b >= 1024.0 {
        format!("{:.0} KB", b / 1024.0)
    } else {
        format!("{b:.0} B")
    }
}

/// Human "time left" for a 이어듣기 row, ceiled to whole minutes so a nearly
/// finished (but still in-progress) episode never reads "0분 남음". Under a
/// minute collapses to a single label.
fn fmt_left(remaining_secs: f64) -> String {
    if remaining_secs < 60.0 {
        "1분 미만 남음".to_string()
    } else {
        let mins = (remaining_secs / 60.0).ceil() as i64;
        format!("{mins}분 남음")
    }
}

fn download_error_msg(reason: &str) -> String {
    match reason {
        "cors" => "다운로드 실패: 이 호스트가 오프라인 저장(CORS)을 허용하지 않습니다. 온라인 재생만 가능합니다.".into(),
        "quota" => "다운로드 실패: 저장 공간이 부족합니다.".into(),
        _ => format!("다운로드 실패: {reason}"),
    }
}

/// One episode row, shared by the per-podcast lists and the 이어듣기 (Continue
/// Listening) section. With `show_subline=false` it is byte-identical to the
/// original inline row, so the main list is unchanged; `show_subline=true` adds
/// the 이어듣기 second line (podcast name + "N분 남음"). `cached_dur` is the
/// persisted duration used to compute remaining time for a *non-playing* row
/// (the playing row uses the live `play_dur`), so non-playing rows touch neither
/// the `<audio>` element nor localStorage.
///
/// The `frac`/`played` Memos preserve the original subscription discipline: a
/// non-playing row reads only `current`/`saved_frac` and stays inert during the
/// ~4Hz timeupdate stream — only the playing row re-runs.
#[allow(clippy::too_many_arguments)]
fn episode_row(
    e: Episode,
    artist: String,
    show_subline: bool,
    cached_dur: f64,
    current: RwSignal<String>,
    online: RwSignal<bool>,
    downloaded: RwSignal<HashSet<String>>,
    progress: RwSignal<HashMap<String, i32>>,
    saved_frac: RwSignal<HashMap<String, f64>>,
    play_pos: RwSignal<f64>,
    play_dur: RwSignal<f64>,
    play: impl Fn(Episode, String) + Copy + Send + Sync + 'static,
    download: impl Fn(Episode) + Copy + Send + Sync + 'static,
    delete_dl: impl Fn(Episode) + Copy + Send + Sync + 'static,
) -> AnyView {
    let key = e.audio_url.clone();
    let title_text = e.title.clone();
    let artist_play = artist.clone();
    let ep_play = e.clone();
    let ep_dl = e.clone();
    let ep_del = e.clone();
    let k_state = key.clone();
    let k_cls = key.clone();
    let k_click = key.clone();
    let k_prog = key.clone();
    let k_left = key.clone();

    let frac = Memo::new(move |_| {
        if current.get() == k_prog {
            let dur = play_dur.get();
            if dur > 0.0 {
                (play_pos.get() / dur).clamp(0.0, 1.0)
            } else {
                // Pre-metadata (e.g. just after a refresh): fall back to the
                // saved fraction.
                saved_frac.with(|m| m.get(&k_prog).copied()).unwrap_or(0.0)
            }
        } else {
            saved_frac.with(|m| m.get(&k_prog).copied()).unwrap_or(0.0)
        }
    });
    let played = Memo::new(move |_| frac.get() >= PLAYED_FRAC);

    // Episodes you can't act on offline: not downloaded and no network. A
    // finished episode also gets a `played` class so its title recedes (dimmed).
    let title_cls = {
        let k = k_cls.clone();
        move || {
            let playable = online.get() || downloaded.with(|s| s.contains(&k));
            let mut cls = String::from("ep-title");
            if !playable {
                cls.push_str(" unplayable");
            }
            if played.get() {
                cls.push_str(" played");
            }
            cls
        }
    };

    // Saved episodes show a non-interactive "✓ 저장됨" status badge paired with a
    // *separate* destructive ✕ remove button. Pre-download: a single ⬇ button;
    // mid-download: a live percent readout.
    let dl_controls = move || {
        if let Some(p) = progress.with(|m| m.get(&k_state).copied()) {
            let label = if p < 0 { "…".to_string() } else { format!("{p}%") };
            view! { <span class="dl-progress">{label}</span> }.into_any()
        } else if downloaded.with(|s| s.contains(&k_state)) {
            let ep = ep_del.clone();
            view! {
                <span class="dl-saved">"✓ 저장됨"</span>
                <button
                    class="dl-btn dl-del"
                    aria-label="오프라인 저장 삭제"
                    on:click=move |_| delete_dl(ep.clone())
                >
                    "✕"
                </button>
            }
                .into_any()
        } else {
            let ep = ep_dl.clone();
            view! {
                <button
                    class="dl-btn"
                    aria-label="오프라인 저장 다운로드"
                    on:click=move |_| download(ep.clone())
                >
                    "⬇"
                </button>
            }
                .into_any()
        }
    };

    // In-progress strip on the row's bottom edge, shown only while
    // 0 < frac < PLAYED_FRAC. A finished (played) row drops the strip and shows
    // the "들음" badge instead. The width still tracks live for the playing row.
    let progress_bar = move || {
        let f = frac.get();
        if f <= 0.0 || played.get() {
            ().into_any() // unplayed or finished → no strip
        } else {
            let pct = f * 100.0;
            view! { <div class="ep-bar" style=format!("width:{pct:.2}%")></div> }.into_any()
        }
    };

    // Auto-marked "played" badge — a passive status, visually distinct from the
    // green "✓ 저장됨" download badge. (Never shown in 이어듣기: played items are
    // filtered out of that section.)
    let played_badge = move || {
        if played.get() {
            view! { <span class="ep-played">"✓ 들음"</span> }.into_any()
        } else {
            ().into_any()
        }
    };

    // 이어듣기 second line: podcast name + remaining time. Remaining is live for
    // the playing row (frac drives it) and uses the cached duration for others,
    // so non-playing rows touch neither the <audio> element nor storage.
    let subline = if show_subline {
        let artist_sub = artist.clone();
        let left = Memo::new(move |_| {
            let f = frac.get();
            let dur = if current.get() == k_left {
                let d = play_dur.get();
                if d > 0.0 { d } else { cached_dur }
            } else {
                cached_dur
            };
            fmt_left((dur * (1.0 - f)).max(0.0))
        });
        Some(view! {
            <span class="ep-sub">
                <span class="pod">{artist_sub}</span>
                <span class="left">{move || left.get()}</span>
            </span>
        })
    } else {
        None
    };

    view! {
        <li>
            <span
                class=title_cls
                on:click=move |_| {
                    let playable = online.get_untracked()
                        || downloaded.with_untracked(|s| s.contains(&k_click));
                    if playable {
                        play(ep_play.clone(), artist_play.clone());
                    }
                }
            >
                {title_text}
                {subline}
            </span>
            {played_badge}
            {dl_controls}
            {progress_bar}
        </li>
    }
    .into_any()
}

#[component]
fn App() -> impl IntoView {
    let podcasts = RwSignal::new(Vec::<Podcast>::new());
    // Podcast titles whose episode lists are folded. Seeded from localStorage
    // when feeds load; the header toggles membership. Absent == expanded.
    let collapsed = RwSignal::new(HashSet::<String>::new());
    let current = RwSignal::new(String::new());
    let now_title = RwSignal::new(String::new());
    // Live position/duration (seconds) of the *current* episode, feeding its list
    // progress bar; reset on every episode switch so a new row never inherits the
    // old bar. 0.0 duration == unknown (pre-metadata / live stream) → no bar.
    let play_pos = RwSignal::new(0.0_f64);
    let play_dur = RwSignal::new(0.0_f64);
    // audio_url → last-known listened fraction (0..1) for *every* episode's bar.
    // Seeded from localStorage at startup; refreshed on switch (never per tick).
    let saved_frac = RwSignal::new(HashMap::<String, f64>::new());
    // audio_url → when last listened (epoch-ms), for ordering the 이어듣기 list
    // most-recent-first. Seeded at feeds-load (with a first-session backfill);
    // refreshed on switch in `play` so within-session reorder is reactive.
    let seen_map = RwSignal::new(HashMap::<String, f64>::new());
    // audio_url → (podcast title, Episode, duration secs). Built once at
    // feeds-load so the 이어듣기 section can reverse-lookup an in-progress
    // audio_url to its Episode (resume via `play`), its podcast name, and a
    // cached duration for "N분 남음" — all without scanning podcasts or hitting
    // localStorage per render. Stores the title (not the whole Podcast) so it
    // never clones a podcast's full episode Vec per key.
    let episode_map = RwSignal::new(HashMap::<String, (String, Episode, f64)>::new());
    // audio_url keys of episodes saved for offline playback.
    let downloaded = RwSignal::new(HashSet::<String>::new());
    // audio_url key → download percent (-1 = indeterminate) while downloading.
    let progress = RwSignal::new(HashMap::<String, i32>::new());
    let storage_used = RwSignal::new(0.0_f64);
    let online = RwSignal::new(true);
    // Active blob: object URL backing downloaded playback; revoked on switch.
    let obj_url = RwSignal::new(Option::<String>::None);
    // Transient status/error banner (tap to dismiss).
    let status = RwSignal::new(String::new());

    // Lock-screen controls can be wired immediately (no DOM dependency).
    register_media_handlers();

    // Online/offline awareness for the offline-only-playable hinting.
    if let Some(win) = web_sys::window() {
        online.set(win.navigator().on_line());
        let on = Closure::<dyn FnMut()>::new(move || online.set(true));
        let off = Closure::<dyn FnMut()>::new(move || online.set(false));
        let _ = win.add_event_listener_with_callback("online", on.as_ref().unchecked_ref());
        let _ = win.add_event_listener_with_callback("offline", off.as_ref().unchecked_ref());
        on.forget();
        off.forget();
    }

    // Load the generated feeds cache (episodes), then the offline-download state.
    spawn_local(async move {
        if let Ok(resp) = gloo_net::http::Request::get("feeds.json").send().await {
            if let Ok(feeds) = resp.json::<Feeds>().await {
                // One pass over every episode: seed each row's resume fraction
                // (saved_frac), build the audio_url → (podcast, episode, dur) map
                // the 이어듣기 section resolves against, and seed the recency map
                // used to order it. First-session backfill: an in-progress episode
                // with no `seen:` stamp yet gets one now, stepped down by feed
                // order, so the very first render is deterministically recency-
                // sorted (newest feed items first) rather than arbitrary. It
                // self-heals to a real listen time on the next `play`.
                let mut fracs = HashMap::new();
                let mut emap = HashMap::new();
                let mut smap = HashMap::new();
                let now = js_sys::Date::now();
                let mut idx = 0.0_f64;
                for p in &feeds.podcasts {
                    for e in &p.episodes {
                        let pos = load_pos(&e.audio_url);
                        let dur = load_dur(&e.audio_url);
                        emap.insert(e.audio_url.clone(), (p.title.clone(), e.clone(), dur));
                        if dur > 0.0 && pos > 0.0 {
                            let frac = (pos / dur).clamp(0.0, 1.0);
                            fracs.insert(e.audio_url.clone(), frac);
                            if frac > 0.0 && frac < PLAYED_FRAC {
                                let mut seen = load_seen(&e.audio_url);
                                if seen <= 0.0 {
                                    seen = now - idx;
                                    save_seen(&e.audio_url, seen);
                                }
                                smap.insert(e.audio_url.clone(), seen);
                            }
                        }
                        idx += 1.0;
                    }
                }
                saved_frac.set(fracs);
                seen_map.set(smap);
                episode_map.set(emap);
                // Restore which podcast lists were folded last time — including the
                // 이어듣기 section (keyed by CONTINUE_KEY).
                let mut folded = HashSet::new();
                for p in &feeds.podcasts {
                    if load_collapsed(&p.title) {
                        folded.insert(p.title.clone());
                    }
                }
                if load_collapsed(CONTINUE_KEY) {
                    folded.insert(CONTINUE_KEY.to_string());
                }
                collapsed.set(folded);
                podcasts.set(feeds.podcasts);
            }
        }
    });
    refresh_downloads(downloaded, storage_used);

    // Reload the last-played episode (loaded + paused) so a refresh resumes it.
    restore_last_played(current, now_title, obj_url, play_pos, play_dur);

    // Play an episode: from IndexedDB (object URL, fully offline) if downloaded,
    // otherwise stream `audio_url` online. `current` stays the audio_url so the
    // resume-position keying is identical on both paths.
    let play = move |ep: Episode, artist: String| {
        spawn_local(async move {
            if let Some(prev) = obj_url.get_untracked() {
                js_revoke_object_url(&prev);
                obj_url.set(None);
            }
            let ts = js_sys::Date::now();
            // Freeze the outgoing episode's list bar at where we left it — the live
            // signals are about to be reset for the new episode, so capture its
            // fraction into saved_frac now (untracked: this is event handling) — and
            // stamp its recency so 이어듣기 can order it by "last listened".
            let prev = current.get_untracked();
            let prev_dur = play_dur.get_untracked();
            if !prev.is_empty() {
                if prev_dur > 0.0 {
                    let frac = (play_pos.get_untracked() / prev_dur).clamp(0.0, 1.0);
                    saved_frac.update(|m| {
                        m.insert(prev.clone(), frac);
                    });
                }
                save_seen(&prev, ts);
                seen_map.update(|m| {
                    m.insert(prev.clone(), ts);
                });
            }
            let key = ep.audio_url.clone();
            let src = if downloaded.with_untracked(|s| s.contains(&key)) {
                match JsFuture::from(js_get_object_url(&key)).await {
                    Ok(v) => v.as_string(),
                    Err(_) => None,
                }
            } else {
                None
            };
            let src = match src {
                Some(u) => {
                    obj_url.set(Some(u.clone()));
                    u
                }
                None => ep.audio_url.clone(),
            };
            if let Some(audio) = audio_el() {
                audio.set_src(&src);
                current.set(ep.audio_url.clone());
                // Reset live progress so the new row starts clean; loadedmetadata
                // repopulates it for the now-current episode.
                play_pos.set(0.0);
                play_dur.set(0.0);
                now_title.set(ep.title.clone());
                audio.load();
                let _ = audio.play();
            }
            // Remember this as the last-played so a refresh reloads it.
            save_last_played(&ep.audio_url, &ep.title, &artist);
            // Stamp the now-current episode's recency too, so a fresh start sorts
            // to the top of 이어듣기 and a reload (which restores it) keeps it there.
            // One millisecond newer than the outgoing stamp so the two never tie
            // (a tie would order by non-deterministic HashMap iteration).
            let in_ts = ts + 1.0;
            save_seen(&ep.audio_url, in_ts);
            seen_map.update(|m| {
                m.insert(ep.audio_url.clone(), in_ts);
            });
            set_now_playing(&ep.title, &artist, "icons/icon-512.png");
            set_playback(true);
        });
    };

    let download = move |ep: Episode| {
        let key = ep.audio_url.clone();
        let url = ep.fetch_url().to_string();
        // Short title so a failure banner names which episode (downloads of
        // different episodes can run concurrently).
        let title: String = ep.title.chars().take(36).collect();
        if progress.with_untracked(|m| m.contains_key(&key)) {
            return; // already downloading
        }
        status.set(String::new());
        progress.update(|m| {
            m.insert(key.clone(), 0);
        });
        spawn_local(async move {
            let k = key.clone();
            let cb = Closure::<dyn FnMut(f64, f64)>::new(move |received: f64, total: f64| {
                let pct = if total > 0.0 {
                    ((received / total) * 100.0).floor() as i32
                } else {
                    -1
                };
                progress.update(|m| {
                    m.insert(k.clone(), pct);
                });
            });
            let res = JsFuture::from(js_download_episode(
                &key,
                &url,
                cb.as_ref().unchecked_ref(),
            ))
            .await;
            drop(cb);
            progress.update(|m| {
                m.remove(&key);
            });
            match res.ok().and_then(|v| v.as_string()).as_deref() {
                Some("ok") => {
                    downloaded.update(|s| {
                        s.insert(key.clone());
                    });
                    refresh_downloads(downloaded, storage_used);
                }
                Some(reason) => status.set(format!("‘{title}’ — {}", download_error_msg(reason))),
                None => status.set(format!("‘{title}’ — {}", download_error_msg("unknown"))),
            }
        });
    };

    let delete_dl = move |ep: Episode| {
        let key = ep.audio_url.clone();
        spawn_local(async move {
            let _ = JsFuture::from(js_delete_episode(&key)).await;
            // If we just deleted the episode currently playing from a blob: URL,
            // revoke it so the freed Blob isn't pinned in memory (the audio
            // element has already loaded it, so playback continues).
            if current.get_untracked() == key {
                if let Some(u) = obj_url.get_untracked() {
                    js_revoke_object_url(&u);
                    obj_url.set(None);
                }
            }
            downloaded.update(|s| {
                s.remove(&key);
            });
            refresh_downloads(downloaded, storage_used);
        });
    };

    // The single live-gated read for the 이어듣기 membership: Some(url) iff the
    // currently-playing episode is itself in-progress (0 < frac < PLAYED_FRAC).
    // Without it, membership would only refresh at the next episode switch
    // (saved_frac is frozen mid-listen), so a freshly-started episode would be
    // invisible during its first play and a finished one would linger. This Memo
    // recomputes on every ~4Hz timeupdate but only NOTIFIES when the Option flips
    // (a threshold crossing or a switch), so it is the ONLY thing subscribing to
    // play_pos/play_dur — the list Memo below stays inert per tick.
    let current_in_progress = Memo::new(move |_| {
        let url = current.get();
        if url.is_empty() {
            return None;
        }
        // Prefer the LIVE fraction once it's meaningful (so a finished episode
        // drops the instant it crosses 95%, and re-seeking backwards re-adds it),
        // but fall back to the SAVED fraction while the live position is still 0 —
        // i.e. pre-metadata, or the brief load/seek window when `play` has reset
        // play_pos to 0 before the resume-seek lands. Without the fallback a
        // just-tapped resume item would flicker out of the list and back during
        // load.
        let dur = play_dur.get();
        let live_f = if dur > 0.0 { play_pos.get() / dur } else { 0.0 };
        let f = if live_f > 0.0 {
            live_f
        } else {
            saved_frac.with(|m| m.get(&url).copied()).unwrap_or(0.0)
        };
        if f > 0.0 && f < PLAYED_FRAC {
            Some(url)
        } else {
            None
        }
    });

    // The ordered audio_urls feeding the 이어듣기 section. Membership = every
    // episode known started-but-unfinished from saved_frac, EXCEPT the currently-
    // playing one — whose membership is decided live by current_in_progress, so
    // it enters on first play and leaves the instant it crosses 95% rather than
    // at the next switch. Ordered most-recently-listened first (seen_map), with
    // the playing episode forced to the top. Depends only on switch-cadence
    // signals plus the memoized current_in_progress flag — never play_pos/play_dur
    // directly — so the <For> never churns on the 4Hz stream.
    let in_progress = Memo::new(move |_| {
        let cur_url = current.get();
        let cip = current_in_progress.get();
        let mut urls: Vec<String> = saved_frac.with(|fracs| {
            episode_map.with(|emap| {
                fracs
                    .iter()
                    .filter(|(u, &f)| f > 0.0 && f < PLAYED_FRAC && emap.contains_key(*u))
                    .map(|(u, _)| u.clone())
                    .filter(|u| *u != cur_url)
                    .collect()
            })
        });
        if let Some(c) = cip {
            if episode_map.with(|m| m.contains_key(&c)) {
                urls.push(c);
            }
        }
        // Borrow seen_map in place (don't clone the whole map per switch) while
        // still registering the reactive dependency on it.
        seen_map.with(|seen| {
            urls.sort_by(|a, b| {
                let ac = *a == cur_url;
                let bc = *b == cur_url;
                if ac != bc {
                    return bc.cmp(&ac); // currently-playing first
                }
                let sa = seen.get(a).copied().unwrap_or(0.0);
                let sb = seen.get(b).copied().unwrap_or(0.0);
                sb.partial_cmp(&sa).unwrap_or(std::cmp::Ordering::Equal)
            })
        });
        urls
    });

    view! {
        <header>
            <h1>"🎧 Podcasts"</h1>
            <Show when=move || !online.get()>
                <span class="offline-badge">"오프라인"</span>
            </Show>
        </header>
        <Show when=move || !status.get().is_empty()>
            <div class="banner" on:click=move |_| status.set(String::new())>
                {move || status.get()}
            </div>
        </Show>
        <main>
            // 이어듣기 (Continue Listening): a cross-podcast collection of every
            // started-but-unfinished episode, pinned above the feed and resumable
            // in one tap. Reuses the .podcast section + episode_row verbatim; the
            // whole section unmounts when nothing is resumable.
            <Show when=move || in_progress.with(|v| !v.is_empty())>
                <section
                    class="podcast continue"
                    class:collapsed=move || collapsed.with(|s| s.contains(CONTINUE_KEY))
                >
                    <h2
                        class="podcast-head"
                        role="button"
                        tabindex="0"
                        aria-expanded=move || {
                            (!collapsed.with(|s| s.contains(CONTINUE_KEY))).to_string()
                        }
                        on:click=move |_| toggle_collapsed(collapsed, CONTINUE_KEY)
                        on:keydown=move |ev| {
                            let k = ev.key();
                            if k == "Enter" || k == " " {
                                ev.prevent_default();
                                toggle_collapsed(collapsed, CONTINUE_KEY);
                            }
                        }
                    >
                        <span class="podcast-title">"이어듣기"</span>
                        <span class="ep-count">{move || in_progress.with(|v| v.len())}</span>
                    </h2>
                    <ul>
                        <For
                            each=move || in_progress.get()
                            key=|u| u.clone()
                            children=move |u: String| {
                                match episode_map.with(|m| m.get(&u).cloned()) {
                                    Some((artist, ep, dur)) => {
                                        episode_row(
                                            ep,
                                            artist,
                                            true,
                                            dur,
                                            current,
                                            online,
                                            downloaded,
                                            progress,
                                            saved_frac,
                                            play_pos,
                                            play_dur,
                                            play,
                                            download,
                                            delete_dl,
                                        )
                                    }
                                    None => ().into_any(),
                                }
                            }
                        />
                    </ul>
                </section>
            </Show>
            <For
                each=move || podcasts.get()
                key=|p| p.title.clone()
                children=move |p: Podcast| {
                    let eps = p.episodes.clone();
                    let ep_count = eps.len();
                    let artist = p.title.clone();
                    let title = p.title.clone();
                    let t_class = title.clone();
                    let t_aria = title.clone();
                    let t_click = title.clone();
                    let t_key = title.clone();
                    view! {
                        <section
                            class="podcast"
                            class:collapsed=move || collapsed.with(|s| s.contains(&t_class))
                        >
                            <h2
                                class="podcast-head"
                                role="button"
                                tabindex="0"
                                aria-expanded=move || {
                                    (!collapsed.with(|s| s.contains(&t_aria))).to_string()
                                }
                                on:click=move |_| toggle_collapsed(collapsed, &t_click)
                                on:keydown=move |ev| {
                                    let k = ev.key();
                                    if k == "Enter" || k == " " {
                                        ev.prevent_default();
                                        toggle_collapsed(collapsed, &t_key);
                                    }
                                }
                            >
                                <span class="podcast-title">{title}</span>
                                <span class="ep-count">{ep_count}</span>
                            </h2>
                            <ul>
                                <For
                                    each=move || eps.clone()
                                    key=|e| e.audio_url.clone()
                                    children=move |e: Episode| {
                                        episode_row(
                                            e,
                                            artist.clone(),
                                            false,
                                            0.0,
                                            current,
                                            online,
                                            downloaded,
                                            progress,
                                            saved_frac,
                                            play_pos,
                                            play_dur,
                                            play,
                                            download,
                                            delete_dl,
                                        )
                                    }
                                />
                            </ul>
                        </section>
                    }
                }
            />
        </main>
        <footer class="player">
            <div class="now">{move || now_title.get()}</div>
            <Show when=move || !downloaded.get().is_empty()>
                <div class="storage">
                    {move || {
                        format!(
                            "오프라인 {}개 · {}",
                            downloaded.with(|s| s.len()),
                            fmt_bytes(storage_used.get()),
                        )
                    }}
                </div>
            </Show>
            <audio
                id="player"
                controls=true
                on:play=move |_| set_playback(true)
                on:pause=move |_| set_playback(false)
                on:loadedmetadata=move |_| {
                    let url = current.get_untracked();
                    let pos = load_pos(&url);
                    if pos > 0.0 {
                        if let Some(a) = audio_el() {
                            a.set_current_time(pos);
                        }
                    }
                    update_position_state();
                    // Fires after the resume-seek, so the list bar reflects the
                    // restored position even while paused (post-refresh restore).
                    update_play_state(current, play_pos, play_dur);
                }
                on:durationchange=move |_| update_play_state(current, play_pos, play_dur)
                on:timeupdate=move |_| {
                    let url = current.get_untracked();
                    if !url.is_empty() {
                        if let Some(a) = audio_el() {
                            // load() (on play/restore) resets the element and fires a
                            // timeupdate at currentTime=0 *before* loadedmetadata. Saving
                            // then would clobber the stored resume position before the
                            // loadedmetadata seek can read it. Only persist once the media
                            // actually has a playback position (HAVE_CURRENT_DATA+).
                            if a.ready_state() >= web_sys::HtmlMediaElement::HAVE_CURRENT_DATA {
                                save_pos(&url, a.current_time());
                            }
                        }
                    }
                    update_position_state();
                    update_play_state(current, play_pos, play_dur);
                }
            ></audio>
        </footer>
    }
}

fn main() {
    console_error_panic_hook::set_once();
    leptos::mount::mount_to_body(App);
}
