# audio-player

모바일에서 팟캐스트를 재생하는 **서버리스 웹 앱**.
GitHub Pages에 정적 배포되며 Rust(Leptos) → WASM 으로 동작한다.

설계 상세는 [DESIGN.md](./DESIGN.md) 참고.

## 스택

- **Rust + Leptos (CSR)** → WASM, 빌드는 **Trunk**
- 오디오: 브라우저 네이티브 **HTML5 `<audio>`** (백그라운드/잠금화면 재생)
- 데이터: 구독은 `subscriptions.json` (git = 진실의 원천), 에피소드는 GitHub Actions가 생성하는 `feeds.json` 캐시 (직접 편집 안 함)
- 상태: `localStorage` 에 에피소드별 재생 위치 저장 (이어듣기)
- **PWA / 오프라인 (v3):** Service Worker(`sw.js`)로 앱 셸 캐시 → 설치형 · 홈화면 추가 · 오프라인 실행.
  에피소드별 **다운로드(⬇)** 는 IndexedDB(`storage.js`)에 저장해 오프라인 재생. **Media Session API** 로 잠금화면 메타데이터/컨트롤.

## 로컬 실행

```bash
# 1) 최초 1회: 빌드 도구 설치
rustup target add wasm32-unknown-unknown
cargo install trunk --locked

# 2) 개발 서버 (http://127.0.0.1:8080)
trunk serve

# 3) 정적 빌드 (dist/ 생성)
trunk build --release
```

## 배포 (GitHub Pages)

`main` 에 push 하면 `.github/workflows/deploy.yml` 가 자동 빌드/배포한다.
저장소 Settings → Pages → Source 를 **GitHub Actions** 로 설정해 두면 된다.
공개 경로는 `/audio-player/` 기준(`--public-url`).

## 설치 & 오프라인 (PWA)

- **설치:** 브라우저 메뉴 → "홈 화면에 추가" / "앱 설치". 독립 창으로 실행되고 홈화면 아이콘이 생긴다.
- **앱 셸 오프라인:** 한 번 방문하면 Service Worker 가 앱(코드/스타일/에피소드 목록)을 캐시해
  오프라인에서도 켜진다. 온라인일 땐 항상 최신 목록을 받는다.
- **에피소드 다운로드:** 에피소드 우측 **⬇** 버튼 → 진행률 표시 후 **✓**. 다시 누르면 삭제.
  다운로드분은 IndexedDB 에 저장돼 비행기 모드에서도 재생된다. 재생 위치(이어듣기)는 온라인/오프라인 공통.
  - 일부 호스트는 오프라인 저장용 `fetch` 의 **CORS** 를 막을 수 있다. 그 경우 다운로드는 실패하고
    **온라인 스트리밍으로 폴백**한다(목록 표시·스트리밍 재생엔 영향 없음). feedsync 가 가능한 한
    CORS 가능한 직접 URL(`download_url`)을 미리 찾아 둔다 — [DESIGN.md](./DESIGN.md) 참고.
- **아이콘 재생성:** `python tools/icons/generate.py` (Pillow). 결과는 `icons/` 에 저장되고 Trunk 가 복사한다.

## 구독 추가 (RSS 자동 갱신)

`subscriptions.json` 에 팟캐스트의 **RSS 피드 URL만** 추가하고 커밋한다.
에피소드 목록은 GitHub Actions 가 받아서 채운다 — `feeds.json` 은 **생성물이므로 직접 편집하지 않는다.**

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

- `feed_url` — (필수) 팟캐스트 RSS 피드 주소.
- `title` — (선택) 표시 이름. 생략하면 피드의 채널 제목을 쓴다.
- `limit` — (선택) 최신 몇 개까지 보관할지. 생략하면 50.

`subscriptions.json` 을 push 하면 `.github/workflows/feedsync.yml` 가 각 피드를 받아
파싱하여 `feeds.json`(앱이 읽는 파일)을 재생성·커밋하고, 변경이 있을 때만 Pages 를 다시 배포한다.
매시간 cron 으로도 자동 갱신되므로 새 에피소드는 가만히 둬도 채워진다.
CORS 우회 원리는 [DESIGN.md](./DESIGN.md) 참고.

로컬에서 직접 갱신해 보려면:

```bash
cargo run -p feedsync   # subscriptions.json → feeds.json 재생성
```
