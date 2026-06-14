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
```

v1은 **순수 정적 · 서버 0대 · CORS 0** 으로 동작한다.

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

### v3 — 오프라인 & PWA
- Service Worker로 앱 셸 캐시 → 설치형 PWA, 홈화면 추가.
- 에피소드 오디오를 IndexedDB/Cache API에 저장 → 오프라인 재생.
  (단, 오프라인용 다운로드 fetch는 호스트의 CORS 허용 필요.)
- Media Session API로 잠금화면 메타데이터/컨트롤.

### v4 — 기기간 동기화 (선택)
- 사용자 GitHub 토큰(PAT)으로 구독/재생위치를 repo에 write-back → 기기간 동기화.
- 토큰 보관/보안 트레이드오프 검토 필요.

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
        { "title": "에피소드 제목", "audio_url": "https://.../episode.mp3" }
      ]
    }
  ]
}
```
