# audio-player

모바일에서 팟캐스트를 재생하는 **서버리스 웹 앱**.
GitHub Pages에 정적 배포되며 Rust(Leptos) → WASM 으로 동작한다.

설계 상세는 [DESIGN.md](./DESIGN.md) 참고.

## 스택

- **Rust + Leptos (CSR)** → WASM, 빌드는 **Trunk**
- 오디오: 브라우저 네이티브 **HTML5 `<audio>`** (백그라운드/잠금화면 재생)
- 데이터: 수동 관리 `feeds.json` (구독 목록 = git이 진실의 원천)
- 상태: `localStorage` 에 에피소드별 재생 위치 저장 (이어듣기)

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

## 구독 추가

`feeds.json` 을 편집해 팟캐스트와 에피소드를 추가한 뒤 커밋한다.

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

> v2에서 RSS 피드 URL만으로 에피소드를 자동 채우는 기능을 추가할 예정
> (CORS 때문에 GitHub Actions 또는 프록시 필요 — DESIGN.md 참고).
