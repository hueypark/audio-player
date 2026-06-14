# audio-player — 설계 (Design)

모바일에서 팟캐스트를 재생하는 **서버리스 웹 앱**.

## 결정 사항 (2026-06-14)

| 항목 | 결정 | 비고 |
|------|------|------|
| 배포 | GitHub Pages (정적) | 백엔드 서버 없음 |
| 언어/프레임워크 | Rust → WASM, **Leptos (CSR)** | Bevy 미사용 (아래 사유) |
| 빌드 | Trunk | `trunk serve` / `trunk build --release` |
| 오디오 | HTML5 `<audio>` + (예정) Media Session API | 백그라운드/잠금화면 재생 |
| 저장 | 수동 `feeds.json` + 브라우저 `localStorage` | git이 구독 목록의 source of truth |
| v1 범위 | 구독 표시 + 스트리밍 재생 + 재생위치 저장 | |

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

## CORS 노트 (중요)

- `<audio src="...">` 재생은 cross-origin이어도 **CORS 제약 없음** → 어떤 호스트의 mp3든 스트리밍 가능.
- 반면 브라우저에서 임의의 **RSS 피드를 `fetch`** 하면 대부분 CORS로 막힌다.
- 그래서 v1은 RSS를 자동 파싱하지 않고 `feeds.json`에 에피소드를 **직접 기입**한다.
- RSS 자동 갱신은 v2에서 (아래).

## 로드맵

### v2 — RSS 자동 갱신
피드 URL만 적으면 에피소드 목록이 자동으로 채워지게. CORS 우회가 필요하므로 둘 중 하나:
1. **GitHub Actions (권장)** — cron으로 RSS를 받아 파싱하여 `episodes/*.json`을 repo에 커밋.
   서버리스 백엔드 + git이 캐시 DB. CORS 완전 회피.
2. **CORS 프록시** — 클라이언트가 프록시 경유로 RSS fetch. 서드파티 의존/신뢰성 이슈.

### v3 — 오프라인 & PWA
- Service Worker로 앱 셸 캐시 → 설치형 PWA, 홈화면 추가.
- 에피소드 오디오를 IndexedDB/Cache API에 저장 → 오프라인 재생.
  (단, 오프라인용 다운로드 fetch는 호스트의 CORS 허용 필요.)
- Media Session API로 잠금화면 메타데이터/컨트롤.

### v4 — 기기간 동기화 (선택)
- 사용자 GitHub 토큰(PAT)으로 구독/재생위치를 repo에 write-back → 기기간 동기화.
- 토큰 보관/보안 트레이드오프 검토 필요.

## feeds.json 스키마

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
