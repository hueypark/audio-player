# audio-player — 설계 (Design)

모바일에서 팟캐스트를 재생하는 **서버리스 웹 앱**.

## 결정 사항 (2026-06-14)

| 항목 | 결정 | 비고 |
|------|------|------|
| 배포 | GitHub Pages (정적) | 백엔드 서버 없음 |
| 언어/프레임워크 | Rust → WASM, **Leptos (CSR)** | Bevy 미사용 (아래 사유) |
| 빌드 | Trunk | `trunk serve` / `trunk build --release` |
| 오디오 | HTML5 `<audio>` + (예정) Media Session API | 백그라운드/잠금화면 재생 |
| 저장 | `subscriptions.json`(구독 source) → `feeds.json`(자동 생성) + 브라우저 `localStorage` | git이 진실의 원천, feeds.json은 캐시 |
| v1 범위 | 구독 표시 + 스트리밍 재생 + 재생위치 저장 | |
| v2 범위 | RSS 자동 갱신 (GitHub Actions cron) — **구현됨** | 아래 |
| v3 범위 | 오프라인 & PWA (설치형 · 앱 셸 오프라인 · 에피소드 다운로드 · 잠금화면 컨트롤) — **구현됨** | 아래 |

## 왜 Bevy를 안 쓰는가

팟캐스트의 핵심은 **화면을 끈 채 백그라운드 재생**이다.
- Bevy 오디오(rodio → cpal → Web Audio `AudioContext`)는 탭이 백그라운드로 가거나
  화면이 잠기면 정지되고, 잠금화면 컨트롤도 없다.
- HTML5 `<audio>` 엘리먼트 + Media Session API는 백그라운드 재생, 잠금화면 재생/일시정지,
  스트리밍, 구간 요청을 네이티브로 지원한다. (모든 웹 팟캐스트 앱이 이 방식)
- 또한 목록/텍스트/스크롤 중심 UI는 캔버스(Bevy)보다 DOM이 접근성·번들 크기 면에서 유리하다.

→ UI/로직은 Rust(Leptos), 오디오는 브라우저 네이티브에 위임.

## 아키텍처 (v1)

```
GitHub repo ── feeds.json (구독 + 에피소드 = 수동 관리, git이 진실의 원천)
   └─ GitHub Pages: Rust/WASM 정적 앱 서빙
                       │
        Leptos/WASM ──┼─ feeds.json fetch → 목록 렌더
                       ├─ HTML5 <audio> 로 원본 CDN URL 스트리밍 (재생은 CORS 무관)
                       └─ localStorage: 에피소드별 재생 위치 저장 / 이어듣기
                            ├─ pos:{audio_url}      = 에피소드별 재생 위치(초)
                            └─ last:url|title|artist = 마지막 재생 에피소드
```

v1은 **순수 정적 · 서버 0대 · CORS 0** 으로 동작한다.

### 새로고침 후 마지막 재생 에피소드 복원
- `play()` 가 재생을 시작할 때 `last:url/title/artist` 를 저장하고(`url` 을 **마지막에** 써서
  커밋 마커로 삼아 중간 실패 시 빈 제목 복원을 막는다), 시작 시 `restore_last_played` 가 이를
  다시 불러와 플레이어를 채운다. 소스 결정은 `play()` 와 동일 — 다운로드된 에피소드는
  IndexedDB blob(오프라인 가능), 아니면 `audio_url` 스트리밍 — 이후 `loadedmetadata` 핸들러가
  `pos:{audio_url}` 위치로 시킹한다. **자동재생은 하지 않는다**(브라우저가 제스처 없는 재생을
  막고, 새로고침이 소리를 터뜨리면 안 됨) — 일시정지 상태로 복원하고 사용자가 재생을 누른다.
- 복원은 비동기(blob 해소)라 두 가드를 둔다: 복원 도중 사용자가 다른 에피소드를 누르면
  그 선택을 덮어쓰지 않고(`current` 가 비어있을 때만 복원), **재생 불가**(blob 없음 + 오프라인)
  이면 로드 불가한 src 로 잠금화면 카드를 만들지 않도록 통째로 건너뛴다.
- **주의(이어듣기 클로버 버그):** `load()` 는 엘리먼트를 리셋하며 `loadedmetadata` **이전에**
  `currentTime=0` 으로 `timeupdate` 를 한 번 쏜다. 이때 위치를 저장하면 시킹 전에 저장된 위치가
  0 으로 덮어써진다. 그래서 `timeupdate` 저장은 `readyState >= HAVE_CURRENT_DATA` 일 때만 한다
  (이 가드가 없으면 이어듣기 자체가 깨진다 — play() 경로도 동일).

## 아키텍처 (v2 — RSS 자동 갱신)

구독 목록과 에피소드를 분리한다. 사람은 피드 URL만 적고, 에피소드는 CI가 채운다.

```
subscriptions.json (사람이 편집: title? + feed_url + limit? = 진실의 원천)
   │
   │  .github/workflows/feedsync.yml
   │    ├─ cron 매시간 / subscriptions.json push / 수동 dispatch
   │    ├─ tools/feedsync (Rust, feed-rs+ureq): 각 RSS fetch·파싱 (서버사이드 = CORS 무관)
   │    ├─ feeds.json 재생성 (결정론적: 변경 있을 때만 커밋)
   │    └─ 변경 시 deploy.yml 을 dispatch → Pages 재배포
   ▼
feeds.json (생성물 = 캐시 DB) ── 앱이 fetch (v1 경로 그대로)
```

- 빌드/실행 주체가 **GitHub Actions(서버)** 라서 브라우저 CORS 제약을 받지 않는다.
- `feeds.json` 의 스키마·소비 경로는 v1과 동일 → 프런트엔드 변경 0.
- `feedsync` 출력은 결정론적(피드 순서 보존·타임스탬프 없음)이라, 피드에 새 에피소드가
  없으면 바이트 동일 → 불필요한 커밋/재배포가 발생하지 않는다.
- 봇이 기본 `GITHUB_TOKEN` 으로 push 한 커밋은 다른 push 워크플로를 트리거하지 않으므로
  (재귀 가드), deploy 는 `workflow_dispatch` 로 명시 호출한다. 덕분에 무한 루프도 없다.

## 아키텍처 (v3 — 오프라인 & PWA)

설치형 PWA + 오프라인. 순수 정적 호스팅을 유지한 채 브라우저 기능만으로 구현한다.

```
index.html ── <link rel=manifest> + iOS meta + 인라인 SW 등록(./sw.js)
   │
   ├─ sw.js  (Trunk copy-file → /audio-player/sw.js, scope=/audio-player/)
   │    런타임 캐시(cache-on-fetch): 내비게이션·feeds.json 은 network-first,
   │    해시된 wasm/js/css·아이콘은 stale-while-revalidate. 크로스오리진(오디오)
   │    요청은 가로채지 않음. CACHE 버전 bump 시 옛 캐시 evict.
   │
   ├─ manifest.webmanifest + icons/  (상대경로 start_url/scope → / 와 /audio-player/ 양쪽 동작)
   │
   ├─ src/main.rs ── Media Session API (web-sys, --cfg=web_sys_unstable_apis)
   │    잠금화면 메타데이터/아트워크 + play/pause/seek 핸들러 + positionState
   │
   └─ 오프라인 다운로드
        storage.js (wasm-bindgen 모듈 스니펫) ── IndexedDB 에 에피소드 Blob 저장,
          진행률 스트리밍 다운로드, 재생은 URL.createObjectURL(blob)
        (Cache API 대신 IndexedDB+objectURL: <audio> 의 Range 요청을 브라우저가
         네이티브로 처리 → SW 의 206 합성 불필요, 완전 오프라인)
```

### 왜 이런 선택인가
- **런타임 캐시(정적 precache 리스트 X):** Trunk 가 wasm/js/css 파일명을 매 빌드 해시하므로
  고정 precache 리스트는 즉시 낡는다. 요청 URL 기준 런타임 캐시는 해시가 바뀌어도
  자동으로 새 번들을 캐시하고, 옛 번들은 CACHE 버전 bump 때 정리된다.
- **SW 위치/스코프:** GitHub Pages 는 `Service-Worker-Allowed` 헤더를 못 줘서 스코프를
  넓힐 수 없다. `sw.js` 를 앱 루트(`/audio-player/sw.js`)에 두면 자연히 `/audio-player/`
  스코프가 된다. 등록은 **상대경로 `./sw.js`** → dev(`/`)·prod(`/audio-player/`) 양쪽 정상.
- **오프라인 오디오 = IndexedDB + objectURL:** Cache API 는 Range 요청을 무시(전체 200 반환)해
  `<audio>` 시킹이 깨진다. IndexedDB Blob 의 `blob:` URL 은 브라우저가 Range 를 직접
  처리하므로 SW 없이 완전 오프라인 재생이 된다.
- **Media Session 은 Rust:** 오디오 엘리먼트와 Leptos 시그널을 이미 Rust 가 소유하므로
  같은 곳에 둔다. 단 web-sys 의 Media Session 바인딩은 아직 unstable → wasm 타깃에만
  `--cfg=web_sys_unstable_apis`(`.cargo/config.toml`)를 주어 native `feedsync` 빌드는 안 건드린다.

### 다운로드 CORS 우회 (feedsync 가 download_url 해소)
- `<audio src>` 스트리밍은 CORS 무관하지만, 오프라인 저장을 위해 mp3 를 `fetch()` 하려면
  **호스트의 CORS 허용이 필요**하다. 그런데 많은 팟캐스트 CDN 은 파일 호스트로 **302 리다이렉트**
  하면서 리다이렉트 응답에 CORS 헤더를 주지 않아, 브라우저의 CORS `fetch` 가 리다이렉트 단계에서 막힌다.
- 그래서 **feedsync(서버사이드 = CORS 무관)** 가 각 에피소드의 리다이렉트 체인을 따라가
  최종 직접 URL 을 `download_url` 로 저장한다. 앱은 다운로드 시 `download_url`(없으면 `audio_url`)을
  fetch 하고, 그래도 CORS 가 막히면 **온라인 스트리밍으로 우아하게 폴백**한다.
- **결정론 유지:** `download_url` 해소는 best-effort 이고 **sticky** 하다 — 이미 해소된 값은
  다음 실행에서 그대로 carry-forward 하고 재탐색하지 않는다(리다이렉트하는 호스트 기준).
  덕분에 정상 상태에서 추가 HTTP 0회 · feeds.json 바이트 동일 → 불필요한 커밋/재배포 없음.
  (`audio_url` 의 의미·키는 그대로라 v1/v2 경로와 완전 하위호환.)

## CORS 노트 (중요)

- `<audio src="...">` 재생은 cross-origin이어도 **CORS 제약 없음** → 어떤 호스트의 mp3든 스트리밍 가능.
- 반면 브라우저에서 임의의 **RSS 피드를 `fetch`** 하면 대부분 CORS로 막힌다.
- 그래서 v1은 RSS를 자동 파싱하지 않고 `feeds.json`에 에피소드를 **직접 기입**했다.
- v2는 RSS fetch·파싱을 **GitHub Actions(서버사이드)** 로 옮겨 이 제약을 우회한다 (위 아키텍처).
  클라이언트는 여전히 동일 출처의 `feeds.json` 만 fetch 하므로 CORS 0 을 유지한다.

## 로드맵

### v2 — RSS 자동 갱신 ✅ 구현됨
피드 URL만 적으면 에피소드 목록이 자동으로 채워진다. **GitHub Actions** 방식을 택했다(권장안):
cron(매시간)으로 RSS를 받아 파싱하여 `feeds.json` 을 재생성·커밋한다. 서버리스 백엔드 +
git이 캐시 DB. CORS 완전 회피. (대안이던 **CORS 프록시** 는 서드파티 의존/신뢰성 때문에 미채택.)

- 사람 편집 source: `subscriptions.json`, 생성물: `feeds.json` (분리).
- 동기화 도구: `tools/feedsync` (Rust 워크스페이스 멤버, `feed-rs` + `ureq`).
- 워크플로: `.github/workflows/feedsync.yml` (cron / subscriptions push / 수동 dispatch).

### v3 — 오프라인 & PWA ✅ 구현됨
- **Service Worker(`sw.js`)** 로 앱 셸 런타임 캐시 → 설치형 PWA, 홈화면 추가, 앱 셸 오프라인.
- 에피소드 오디오를 **IndexedDB** 에 저장(`storage.js`) → `blob:` objectURL 로 **오프라인 재생**.
  오프라인 다운로드 fetch 는 호스트 CORS 가 필요 → **feedsync 가 리다이렉트를 해소해 `download_url`**
  을 채우고, 막히면 온라인 스트리밍으로 폴백.
- **Media Session API**(web-sys) 로 잠금화면 메타데이터/아트워크/재생·일시정지·시킹.
- 상세는 위 "아키텍처 (v3 — 오프라인 & PWA)" 참고. 아이콘은 `tools/icons/generate.py`(Pillow)로 생성.

## subscriptions.json 스키마 (사람이 편집하는 source)

```json
{
  "subscriptions": [
    {
      "title": "손에 잡히는 경제 (MBC)",
      "feed_url": "https://minicast.imbc.com/podcast/pod.aspx?code=1000671100000100000",
      "limit": 50
    }
  ]
}
```

- `feed_url` (필수) · `title`(선택, 생략 시 피드 채널 제목) · `limit`(선택, 기본 50).

## feeds.json 스키마 (feedsync가 생성, 앱이 소비)

```json
{
  "podcasts": [
    {
      "title": "팟캐스트 이름",
      "episodes": [
        {
          "title": "에피소드 제목",
          "audio_url": "https://.../episode.mp3",
          "download_url": "https://cdn.../episode.mp3"
        }
      ]
    }
  ]
}
```

- `audio_url` (필수) — 스트리밍 src 이자 **안정 식별자**(이어듣기 키 · IndexedDB 키 · `<For>` 키). 절대 안 바뀐다.
- `download_url` (선택, v3) — 오프라인 다운로드용 **CORS 가능한 직접 URL**. feedsync 가 `audio_url` 의
  리다이렉트를 서버사이드로 해소해 채운다. 리다이렉트가 없으면 생략(앱은 `audio_url` 로 폴백) →
  기존 feeds.json 과 바이트 하위호환.
