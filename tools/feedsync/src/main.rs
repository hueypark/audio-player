//! feedsync — regenerate `feeds.json` from the subscribed RSS feeds.
//!
//! Reads `subscriptions.json` (the hand-maintained source of truth: just a
//! display title + feed URL per podcast), fetches and parses each RSS feed,
//! and writes `feeds.json` in exactly the shape the Leptos/WASM app consumes
//! (`{ podcasts: [{ title, episodes: [{ title, audio_url }] }] }`).
//!
//! This runs server-side (GitHub Actions), so it sidesteps the browser CORS
//! limit on fetching arbitrary RSS feeds — git becomes the cache DB. See
//! DESIGN.md ("v2 — RSS 자동 갱신").
//!
//! Output is deterministic (feed order preserved, no timestamps) so that an
//! unchanged feed yields a byte-identical `feeds.json`, and the CI job only
//! commits — and redeploys — when something actually changed.

use std::collections::{HashMap, HashSet};
use std::error::Error;
use std::fs;
use std::net::IpAddr;
use std::time::Duration;

use serde::{Deserialize, Serialize};

/// Identifies us politely to feed hosts; some CDNs reject empty User-Agents.
const USER_AGENT: &str = concat!(
    "audio-player-feedsync/",
    env!("CARGO_PKG_VERSION"),
    " (+https://github.com/hueypark/audio-player)"
);

/// How many recent episodes to keep per podcast when the subscription doesn't
/// say otherwise. Feeds like MBC's expose their entire archive (thousands of
/// entries); the app renders every episode in one list, so we keep only the
/// latest N to bound feeds.json size and DOM cost.
const DEFAULT_LIMIT: usize = 50;

/// Whole-request timeout. ureq 3 sets NO timeouts by default, so without this a
/// host that accepts the connection but never sends a body would hang the run
/// forever — and since fetching is all-or-nothing, that wedges every feed.
const FETCH_TIMEOUT: Duration = Duration::from_secs(30);
/// Tighter bound on the connect phase (host down / unroutable).
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
/// Whole-request bound on a single download_url HEAD probe. Resolution is
/// best-effort (a timeout just leaves the field unresolved), so keep it tight to
/// never let a slow CDN drag out the hourly job.
const RESOLVE_TIMEOUT: Duration = Duration::from_secs(15);
/// Cap on bytes read from a single feed. `as_reader()` is unbounded by default,
/// so a broken/hostile host could stream forever; podcast feeds are well under
/// this.
const MAX_FEED_BYTES: u64 = 20 * 1024 * 1024;

/// `subscriptions.json` — the human-edited source of truth.
#[derive(Deserialize)]
struct Subscriptions {
    subscriptions: Vec<Subscription>,
}

#[derive(Deserialize)]
struct Subscription {
    /// Optional display-title override. When absent the feed's own channel
    /// title is used.
    #[serde(default)]
    title: Option<String>,
    feed_url: String,
    /// Optional cap on episodes kept (newest first). Defaults to
    /// [`DEFAULT_LIMIT`].
    #[serde(default)]
    limit: Option<usize>,
}

/// `feeds.json` — generated; mirrors the structs in `src/main.rs` (the app).
/// Also `Deserialize` so we can read the *previous* feeds.json and carry forward
/// already-resolved `download_url`s (see the resolution pass in `main`).
#[derive(Serialize, Deserialize)]
struct Feeds {
    podcasts: Vec<Podcast>,
}

#[derive(Serialize, Deserialize)]
struct Podcast {
    title: String,
    episodes: Vec<Episode>,
}

#[derive(Serialize, Deserialize)]
struct Episode {
    title: String,
    audio_url: String,
    /// A CORS-fetchable direct URL for offline download, when `audio_url`
    /// redirects to a host that drops CORS on the redirect (so an in-browser
    /// download of `audio_url` itself would be blocked). Resolved best-effort
    /// below; omitted when `audio_url` is already directly fetchable.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    download_url: Option<String>,
}

fn main() -> Result<(), Box<dyn Error>> {
    let mut args = std::env::args().skip(1);
    let subs_path = args.next().unwrap_or_else(|| "subscriptions.json".to_string());
    let feeds_path = args.next().unwrap_or_else(|| "feeds.json".to_string());

    let raw = fs::read_to_string(&subs_path)
        .map_err(|e| format!("failed to read {subs_path}: {e}"))?;
    let subs: Subscriptions = serde_json::from_str(&raw)
        .map_err(|e| format!("failed to parse {subs_path}: {e}"))?;

    let agent: ureq::Agent = ureq::Agent::config_builder()
        .timeout_global(Some(FETCH_TIMEOUT))
        .timeout_connect(Some(CONNECT_TIMEOUT))
        .user_agent(USER_AGENT)
        .build()
        .into();

    // All-or-nothing: if any feed fails we bail without writing, so a transient
    // network blip never overwrites a good feeds.json with a partial one. The
    // last-good file stays in place and the next run retries.
    let mut podcasts = Vec::with_capacity(subs.subscriptions.len());
    let mut titles = HashSet::new();
    for sub in &subs.subscriptions {
        let podcast = fetch_podcast(&agent, sub).map_err(|e| {
            format!(
                "feed '{}' ({}): {e}",
                sub.title.as_deref().unwrap_or("?"),
                sub.feed_url
            )
        })?;
        // The app keys its outer <For> by podcast title, so titles must be
        // unique — otherwise feeds.json would break the app's keyed rendering.
        if !titles.insert(podcast.title.clone()) {
            return Err(format!(
                "duplicate podcast title {:?}; titles must be unique because the app keys podcasts by title",
                podcast.title
            )
            .into());
        }
        eprintln!("ok: {} ({} episodes)", podcast.title, podcast.episodes.len());
        podcasts.push(podcast);
    }

    resolve_download_urls(&mut podcasts, &feeds_path);

    let feeds = Feeds { podcasts };
    // Determinism (see module docs): serde_json emits fields in struct-declaration
    // order, so DON'T reorder the Episode/Podcast fields — byte-identical output on
    // unchanged input is what keeps the hourly job from making spurious commits.
    let mut json = serde_json::to_string_pretty(&feeds)?;
    json.push('\n'); // trailing newline, matches editor/git convention
    fs::write(&feeds_path, json).map_err(|e| format!("failed to write {feeds_path}: {e}"))?;

    eprintln!("wrote {} ({} podcast(s))", feeds_path, feeds.podcasts.len());
    Ok(())
}

/// v3 offline: give every episode a CORS-fetchable `download_url`.
///
/// Browsers stream any cross-origin `<audio src>` without CORS, but `fetch()`-ing
/// an mp3 to *store it offline* DOES require the host's CORS headers — and many
/// podcast CDNs 302-redirect to a file host while dropping CORS on the redirect,
/// which blocks an in-browser download of the feed's `audio_url`. We resolve that
/// redirect chain here (server-side = no CORS limit) to the final direct URL.
///
/// Best-effort and deterministic: an episode is probed only when no resolved
/// `download_url` is already carried forward from the previous feeds.json. Once a
/// redirecting host resolves, the value sticks and that episode is never probed
/// again, so steady-state runs make no extra HTTP requests and emit byte-
/// identical output. (A host that does NOT redirect resolves to nothing and so is
/// re-probed each run — fine for the current single redirecting feed; revisit if
/// a high-volume non-redirecting feed is ever added.)
fn resolve_download_urls(podcasts: &mut [Podcast], feeds_path: &str) {
    let prev_dl = load_previous(feeds_path);

    let resolver: ureq::Agent = ureq::Agent::config_builder()
        .timeout_global(Some(RESOLVE_TIMEOUT))
        .timeout_connect(Some(CONNECT_TIMEOUT))
        // Don't auto-follow: we read each Location ourselves to find the final
        // direct URL. `*_will_error(false)` makes a 3xx come back as a response
        // instead of an error.
        .max_redirects(0)
        .max_redirects_will_error(false)
        .user_agent(USER_AGENT)
        .build()
        .into();

    let mut probed = 0usize;
    for podcast in podcasts.iter_mut() {
        for ep in podcast.episodes.iter_mut() {
            ep.download_url = match prev_dl.get(&ep.audio_url) {
                Some(d) => Some(d.clone()), // carry forward, no re-probe
                None => {
                    probed += 1;
                    resolve_one(&resolver, &ep.audio_url)
                }
            };
        }
    }
    if probed > 0 {
        eprintln!("download_url: probed {probed} unresolved episode(s)");
    }
}

/// `audio_url -> download_url` for episodes that already had one resolved in the
/// previous feeds.json. Missing/corrupt file → empty (first run probes all).
fn load_previous(path: &str) -> HashMap<String, String> {
    let mut map = HashMap::new();
    if let Ok(raw) = fs::read_to_string(path) {
        if let Ok(feeds) = serde_json::from_str::<Feeds>(&raw) {
            for p in &feeds.podcasts {
                for e in &p.episodes {
                    if let Some(d) = &e.download_url {
                        map.insert(e.audio_url.clone(), d.clone());
                    }
                }
            }
        }
    }
    map
}

/// Walk the redirect chain (HEAD, no body) to the first non-redirect URL. Returns
/// it only if it differs from `audio_url` (i.e. a redirect actually happened);
/// `None` means "already direct, just fetch `audio_url`" or "couldn't resolve"
/// — both of which the app handles by falling back to `audio_url`.
fn resolve_one(agent: &ureq::Agent, audio_url: &str) -> Option<String> {
    let mut current = audio_url.to_string();
    for _ in 0..6 {
        // SSRF guard: never probe loopback/private/link-local/localhost targets.
        // A malicious feed could otherwise redirect us at e.g. cloud metadata
        // (169.254.169.254) from the CI runner. Checked every hop (incl. the
        // initial audio_url, which also comes from the feed).
        if !host_allowed(&current) {
            return None;
        }
        // GET, not HEAD: this host returns an empty Location to HEAD. With
        // redirects disabled the 3xx body is empty, and on the final 2xx we read
        // only status/headers and drop the response without consuming the body,
        // so no episode audio is actually downloaded.
        let resp = agent.get(&current).call().ok()?;
        let status = resp.status();
        if status.is_redirection() {
            let loc = resp
                .headers()
                .get(ureq::http::header::LOCATION)?
                .to_str()
                .ok()?;
            if loc.trim().is_empty() {
                return None; // non-compliant empty Location — don't loop on it
            }
            let next = join_url(&current, loc)?;
            if next == current {
                return None; // self-referential redirect
            }
            current = next;
            continue;
        }
        if status.is_success() {
            return (current != audio_url).then_some(current);
        }
        return None; // 4xx/5xx — leave unresolved
    }
    None // redirect loop / too many hops
}

/// Minimal URL joiner for a redirect `Location` (absolute, protocol-relative,
/// root-relative, or a relative path) without pulling in the `url` crate.
fn join_url(base: &str, loc: &str) -> Option<String> {
    if loc.starts_with("http://") || loc.starts_with("https://") {
        return Some(loc.to_string());
    }
    let scheme = base.split("://").next()?;
    if let Some(rest) = loc.strip_prefix("//") {
        return Some(format!("{scheme}://{rest}")); // protocol-relative
    }
    let authority = base.split("://").nth(1)?.split('/').next()?;
    if loc.starts_with('/') {
        return Some(format!("{scheme}://{authority}{loc}")); // root-relative
    }
    let dir = base.rsplit_once('/').map(|(d, _)| d).unwrap_or(base);
    Some(format!("{dir}/{loc}")) // relative path
}

/// SSRF allowlist for a probe target: only http(s), and reject hosts that are
/// `localhost` or a literal loopback/private/link-local/etc. IP. Domain names
/// are allowed without DNS resolution (cheap; real podcast CDNs are public
/// domains) — this blocks the obvious internal-IP redirect, not DNS rebinding.
fn host_allowed(url: &str) -> bool {
    let Some((scheme, rest)) = url.split_once("://") else {
        return false;
    };
    if !scheme.eq_ignore_ascii_case("http") && !scheme.eq_ignore_ascii_case("https") {
        return false;
    }
    // authority = up to the first '/', '?' or '#'; strip any userinfo@.
    let authority = rest.split(['/', '?', '#']).next().unwrap_or("");
    let host_port = authority.rsplit_once('@').map(|(_, h)| h).unwrap_or(authority);
    // host: handle [IPv6]:port and host:port.
    let host = if let Some(inner) = host_port.strip_prefix('[') {
        inner.split_once(']').map(|(h, _)| h).unwrap_or(inner)
    } else {
        host_port.rsplit_once(':').map(|(h, _)| h).unwrap_or(host_port)
    };
    if host.is_empty() {
        return false;
    }
    let lc = host.to_ascii_lowercase();
    if lc == "localhost" || lc.ends_with(".localhost") {
        return false;
    }
    match host.parse::<IpAddr>() {
        Ok(ip) => is_public_ip(&ip),
        Err(_) => true, // a domain name — accept (no DNS lookup here)
    }
}

fn is_public_ip(ip: &IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => !(v4.is_loopback()
            || v4.is_private()
            || v4.is_link_local()
            || v4.is_broadcast()
            || v4.is_documentation()
            || v4.is_unspecified()
            || v4.is_multicast()),
        // std lacks stable is_unique_local/is_unicast_link_local for v6; cover
        // the clear cases (CDNs are v4 in practice, incl. the 169.254 metadata IP).
        IpAddr::V6(v6) => !(v6.is_loopback() || v6.is_unspecified() || v6.is_multicast()),
    }
}

/// Fetch and parse one subscription into a `Podcast`.
fn fetch_podcast(agent: &ureq::Agent, sub: &Subscription) -> Result<Podcast, Box<dyn Error>> {
    let resp = agent.get(&sub.feed_url).call()?; // User-Agent is set on the agent
    // Bounded reader so a runaway host can't stream unbounded data into the parser.
    let reader = resp
        .into_body()
        .into_with_config()
        .limit(MAX_FEED_BYTES)
        .reader();
    let parsed = feed_rs::parser::parse(reader)?;

    let title = sub
        .title
        .as_ref()
        .map(|t| t.trim().to_string())
        .or_else(|| parsed.title.as_ref().map(|t| t.content.trim().to_string()))
        .filter(|t| !t.is_empty())
        .unwrap_or_else(|| sub.feed_url.clone());

    // The app keys episodes by `audio_url` in its <For>, so duplicate enclosure
    // URLs would collide — de-dup, keeping the first (newest) occurrence.
    let limit = sub.limit.unwrap_or(DEFAULT_LIMIT);
    let mut seen = HashSet::new();
    let mut episodes = Vec::new();
    for entry in &parsed.entries {
        if episodes.len() >= limit {
            break; // feed is newest-first; keep only the latest `limit`
        }
        let Some(audio_url) = enclosure_url(entry) else {
            continue; // no playable audio (e.g. a blog-style item) — skip
        };
        if !seen.insert(audio_url.clone()) {
            continue;
        }
        let ep_title = entry
            .title
            .as_ref()
            .map(|t| t.content.trim().to_string())
            .filter(|t| !t.is_empty())
            .unwrap_or_else(|| audio_url.clone());
        episodes.push(Episode {
            title: ep_title,
            audio_url,
            download_url: None, // filled by the resolution pass in `main`
        });
    }

    if episodes.is_empty() {
        return Err("no playable episodes (no <enclosure> audio urls found)".into());
    }

    Ok(Podcast { title, episodes })
}

/// Pull the audio enclosure URL out of an entry. feed-rs maps the RSS 2.0
/// `<enclosure>` into `entry.media[].content[].url`; the `rel="enclosure"`
/// link is a defensive fallback for nonstandard feeds.
fn enclosure_url(entry: &feed_rs::model::Entry) -> Option<String> {
    entry
        .media
        .iter()
        .flat_map(|m| &m.content)
        .find_map(|c| c.url.as_ref().map(|u| u.to_string()))
        .or_else(|| {
            entry
                .links
                .iter()
                .find(|l| l.rel.as_deref() == Some("enclosure"))
                .map(|l| l.href.clone())
        })
}
