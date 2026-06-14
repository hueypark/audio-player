use leptos::prelude::*;
use serde::Deserialize;
use wasm_bindgen::JsCast;
use wasm_bindgen_futures::spawn_local;
use web_sys::HtmlAudioElement;

#[derive(Clone, Debug, Deserialize)]
struct Episode {
    title: String,
    audio_url: String,
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

#[component]
fn App() -> impl IntoView {
    let podcasts = RwSignal::new(Vec::<Podcast>::new());
    let current = RwSignal::new(String::new());
    let now_title = RwSignal::new(String::new());

    // Load the generated feeds cache (episodes) once at startup.
    // (subscriptions.json is the human-edited source; feeds.json is built from it
    // by tools/feedsync — see DESIGN.md.)
    spawn_local(async move {
        if let Ok(resp) = gloo_net::http::Request::get("feeds.json").send().await {
            if let Ok(feeds) = resp.json::<Feeds>().await {
                podcasts.set(feeds.podcasts);
            }
        }
    });

    let play = move |ep: Episode| {
        if let Some(audio) = audio_el() {
            audio.set_src(&ep.audio_url);
            current.set(ep.audio_url.clone());
            now_title.set(ep.title.clone());
            audio.load();
            let _ = audio.play();
        }
    };

    view! {
        <header>
            <h1>"🎧 Podcasts"</h1>
        </header>
        <main>
            <For
                each=move || podcasts.get()
                key=|p| p.title.clone()
                children=move |p: Podcast| {
                    let eps = p.episodes.clone();
                    view! {
                        <section class="podcast">
                            <h2>{p.title}</h2>
                            <ul>
                                <For
                                    each=move || eps.clone()
                                    key=|e| e.audio_url.clone()
                                    children=move |e: Episode| {
                                        let target = e.clone();
                                        view! {
                                            <li on:click=move |_| play(target.clone())>
                                                {e.title}
                                            </li>
                                        }
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
            <audio
                id="player"
                controls=true
                on:loadedmetadata=move |_| {
                    let url = current.get_untracked();
                    let pos = load_pos(&url);
                    if pos > 0.0 {
                        if let Some(a) = audio_el() {
                            a.set_current_time(pos);
                        }
                    }
                }
                on:timeupdate=move |_| {
                    let url = current.get_untracked();
                    if !url.is_empty() {
                        if let Some(a) = audio_el() {
                            save_pos(&url, a.current_time());
                        }
                    }
                }
            ></audio>
        </footer>
    }
}

fn main() {
    console_error_panic_hook::set_once();
    leptos::mount::mount_to_body(App);
}
