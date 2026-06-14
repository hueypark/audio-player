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

use std::collections::HashSet;
use std::error::Error;
use std::fs;
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
#[derive(Serialize)]
struct Feeds {
    podcasts: Vec<Podcast>,
}

#[derive(Serialize)]
struct Podcast {
    title: String,
    episodes: Vec<Episode>,
}

#[derive(Serialize)]
struct Episode {
    title: String,
    audio_url: String,
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

    let feeds = Feeds { podcasts };
    let mut json = serde_json::to_string_pretty(&feeds)?;
    json.push('\n'); // trailing newline, matches editor/git convention
    fs::write(&feeds_path, json).map_err(|e| format!("failed to write {feeds_path}: {e}"))?;

    eprintln!("wrote {} ({} podcast(s))", feeds_path, feeds.podcasts.len());
    Ok(())
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
