# Sigwin

### 초경량 Signal 데스크톱 클라이언트 (비공식)

**Rust** • [**presage**](https://github.com/whisperfish/presage) • [**Slint**](https://slint.dev)(소프트웨어 렌더러)

> Signal과 무관한 비공식 클라이언트입니다. "Signal"은 Signal Technology Foundation의 상표입니다.

> **초기 버전(0.x)입니다.** 버그가 있을 수 있습니다. 대화는 휴대폰의 공식 앱에도 그대로 남아 있습니다.

<h3>
    <a href="https://github.com/Othereum/sigwin/releases/latest">📥 최신 버전 다운로드</a>
    <span> • </span>
    <a href="https://github.com/Othereum/sigwin/issues">🐛 버그 제보</a>
</h3>

---

## 목표
- 평상시(idle) private 메모리 **30MB 이하**
- 단일 exe, GPU/WebView 불필요

## 기능
- QR 코드로 휴대폰에 **연결된 기기**로 등록
- 1:1·그룹 대화: 답장, 수정, 삭제, 반응, @멘션, 글자 서식, 메시지 고정, 읽음 확인
- 사진·동영상·파일 첨부, 스티커
- 링크 미리보기. 보낼 때 그 사이트에 접속하므로 사이트에 IP 주소가 보이며, 설정에서 끌 수 있습니다.
- Windows 알림, 안 읽은 수 배지, 대화별 알림 설정
- 창을 닫아도 트레이에서 계속 실행, 로그인 시 자동 실행
- 자동 업데이트
- 로컬 데이터 암호화. 키는 Windows DPAPI로 보호합니다.

### 미지원
새 대화 시작, 음성·영상 통화, 스토리, 스티커 팩 삭제, 결제, 한 번 보기 미디어, 사라지는 메시지 타이머 변경, 기존 대화 기록 이전, 알림 프로필

## 메모리 절약 설계
- Slint **소프트웨어 렌더러** 사용 (femtovg 212MB, Skia 388MB 대비 private 8.4MB)
- 메시지는 60행씩 조회하고, 대화창에는 최대 600행만 유지
- 이미지는 썸네일만 메모리에 올리고 나머지는 디스크 캐시
- 움직이는 스티커 프레임은 화면에 나온 것만, 합계 24MB까지 (LRU)
- 파일 선택 대화상자는 자식 프로세스에서 실행
- 자세한 측정과 결정 배경은 [`docs/decisions.md`](docs/decisions.md)

## 빌드 (Windows)
필요한 도구: Rust(stable), protoc, LLVM(clang), NASM, CMake

```powershell
winget install Google.Protobuf LLVM.LLVM NASM.NASM Kitware.CMake
cargo build --profile fast   # 개발용
cargo build --release        # 배포용 (thin LTO)
cargo test
```
- 결과물은 `target\fast\` 또는 `target\release\`에 생깁니다.
- LLVM이 기본 경로(`C:\Program Files\LLVM\bin`)에 없으면 `LIBCLANG_PATH`로, protoc는 `PATH` 또는 `PROTOC`로 지정하세요.
- 앱이 실행 중이면 exe가 잠겨 빌드가 실패하니 먼저 종료하세요.
- `target` 아래의 exe는 설치 단계를 건너뛰고 그 자리에서 실행됩니다.

## 실행
```powershell
target\fast\sigwin.exe
```
| 환경 변수 | 용도 |
|---|---|
| `SIGWIN_DATA` | 데이터 폴더 (기본값 `%LOCALAPPDATA%\Sigwin\data`) |
| `RUST_LOG` | 로그 레벨 (예: `info,sigwin=debug`) |
| `SIGWIN_DEMO=1` | 계정 없이 가짜 데이터로 UI 확인 |

그 밖의 데모·헤드리스 옵션은 [`docs/headless.md`](docs/headless.md)를 보세요.

## 라이선스
AGPL-3.0-only (presage, libsignal과 동일). 전문은 [`LICENSE`](LICENSE)에 있습니다. 포함된 라이브러리의 고지는 앱의 `⚙` → 정보 → 오픈소스 라이선스, 또는 릴리스의 `licenses.html`에 있습니다.
