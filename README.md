# lite-signal

Signal 프로토콜 호환 **경량 데스크톱 클라이언트**(비공식)입니다. Rust + [presage](https://github.com/whisperfish/presage) + [Slint](https://slint.dev)(소프트웨어 렌더러)로 만들었습니다.

> Signal Messenger LLC와 무관한 비공식 클라이언트입니다. "Signal"은 Signal Messenger LLC의 상표입니다.

## 목표
- 메모리 **100MB 이하** (Electron 기반 공식 클라이언트는 300~600MB)
- 단일 exe, GPU/WebView 불필요

## 기능
### 대화
- QR 코드로 휴대폰에 **연결된 기기**로 등록
- 1:1 및 그룹 대화 목록 (최근순, 안 읽은 메시지 수, 검색, 연락처로 새 대화 시작)
- 메시지 송수신, 답장(말풍선 더블클릭), 파일·사진 첨부
- 이미지 썸네일(디스크 캐시), 첨부파일 열기(기본 앱 실행)
- 반응·수정·삭제 표시

### 수신 확인
- 내가 보낸 메시지에 상태 표시: `✓` 전송 / `✓✓` 전달 / `읽음` (그룹은 `읽음 N`)
- 상태나 시간을 클릭하면 **메시지 정보** 창이 열립니다: 읽은 사람, 전달된 사람, 아직 전달 안 된 사람을 이름으로 표시
- 전달 확인(delivery receipt)은 자동으로 보냅니다. 읽음 확인은 보내지 않습니다.
- 다른 기기에서 읽으면 안 읽음 배지를 해제합니다.

### 스티커
- 받은 스티커를 말풍선 없이 이미지로 표시
- `☺` 버튼으로 스티커 창 열기 → 팩 탭 선택 → 클릭하면 전송
- 팩 추가 방법
  - Signal 기본 추천 팩 8종 (스티커 창의 "＋ 팩 추가" 탭)
  - `https://signal.art/addstickers/#pack_id=…&pack_key=…` 링크
  - 받은 스티커 클릭: 이미 있는 팩이면 그 팩이 열리고, 없으면 추가할지 물어봅니다
- 팩을 추가하면 공식 앱처럼 휴대폰에도 설치됩니다.

### 기타
- Windows 알림, 창 제목에 안 읽은 메시지 수 표시
- 로컬 DB는 **SQLCipher로 암호화**하며, 키는 Windows **DPAPI**로 보호합니다(현재 Windows 사용자만 복호화 가능)

### 미지원
음성·영상 통화, 스토리, 움직이는 스티커 애니메이션(첫 프레임만 표시), 스티커 팩 삭제, 결제, 한 번 보기 미디어, 컬러 이모지(단색으로 표시), 기존 대화 기록 이전(연결한 이후의 메시지부터 표시)

## 메모리 절약 설계
- Slint **소프트웨어 렌더러**: GPU 드라이버가 상주하며 쓰는 메모리가 없습니다.
- 메시지는 **페이지 단위**(60행)로 조회하고, 대화창에는 최대 600행만 유지합니다.
- 이미지는 260px 썸네일만 메모리에 올리고, 대화방을 바꾸면 해제합니다. 원본과 썸네일은 디스크에 캐시합니다.
- 스티커 창의 미리보기는 창을 닫으면 해제합니다.
- 파일 선택 대화상자는 **자식 프로세스**(`lite-signal.exe --pick-files`)에서 실행합니다. 셸 확장 DLL이 메인 프로세스에 남지 않습니다(측정 결과: 열려 있는 동안 +28MB, 닫은 후에도 +10MB 남던 것을 0으로).
- **Segment Heap** 매니페스트로 대량 동기화 후 해제된 메모리를 OS에 돌려줍니다.
- 10초마다 `mem: rust heap …, private …`을 로그로 남깁니다(Rust 힙과 네이티브 메모리 구분).

### 측정치 (Windows 11)
| 상태 | Working Set | Private |
|---|---|---|
| QR 연결 화면 | 31MB | 9.6MB |
| 실계정 연결 후 대화 목록 | 40~44MB | 14.5~16MB |
| 데모(대화 40개 / 메시지 300개) | 29MB | 8.5MB |

## 빌드 (Windows)
필요한 도구: Rust(stable), protoc, LLVM(clang), NASM, CMake

```powershell
winget install Google.Protobuf LLVM.LLVM NASM.NASM Kitware.CMake
cargo build --profile fast   # 개발용: 최적화 O, LTO X, 증분 빌드 (재빌드 수 초~40초)
cargo build --release        # 배포용: thin LTO (재빌드 약 2.5분)
cargo test
```
- 빌드 결과는 `target\fast\` 또는 `target\release\`에 생깁니다.
- `.cargo/config.toml`에 `PROTOC`·`LIBCLANG_PATH` 경로가 고정되어 있습니다. 경로가 다르면 수정하세요.
- SQLCipher는 순수 Rust 암호화 provider(`bundled-sqlcipher-custom-crypto`)를 쓰므로 OpenSSL이 필요 없습니다. Windows 빌드 스크립트가 경로 검사만 하기 때문에 `build-support/no-openssl`의 빈 폴더를 지정해 두었습니다.
- 앱이 실행 중이면 exe가 잠겨서 빌드가 실패합니다. 먼저 앱을 종료하세요.

## 실행
```powershell
target\fast\lite-signal.exe      # 개발용 빌드
target\release\lite-signal.exe   # 배포용 빌드
```
| 환경 변수 | 용도 |
|---|---|
| `LITE_SIGNAL_DATA` | 데이터 폴더 (기본값 `%LOCALAPPDATA%\lite-signal\lite-signal\data`) |
| `RUST_LOG` | 로그 레벨 (예: `info,lite_signal=debug`). 콘솔에서 실행하면 stderr로 출력 |
| `LITE_SIGNAL_DEMO=1` | 계정 없이 가짜 데이터로 UI 확인 |
| `LITE_SIGNAL_DEMO_STICKERS=1` / `=add` | 데모에서 스티커 창 열기 / 팩 추가 화면 열기 |
| `LITE_SIGNAL_DEMO_INFO=1` | 데모에서 메시지 정보 창과 답장 바 띄우기 |
| `LITE_SIGNAL_DEMO_TICK=1` | 데모에서 0.3초마다 UI 갱신 (최소화 중 갱신 재현용) |
| `LITE_SIGNAL_SELFTEST=1` | (데모와 함께) 파일 대화상자와 알림의 메모리 비용 측정 |

데이터 폴더 구성: `signal.db3`(암호화 DB), `db.key`(DPAPI로 보호한 키), `state.json`(안 읽은 수, 미리보기 캐시), `attachments/`, `thumbs/`

## 사용법
- `Enter` 전송 / `Shift+Enter` 줄바꿈
- 말풍선 더블클릭: 답장
- 내 메시지의 상태나 시간 클릭: 메시지 정보
- `☺`: 스티커, `📎`: 파일 첨부, `⏏`: 연결 해제(로컬 데이터 삭제)

## 라이선스
AGPL-3.0-only (presage, libsignal과 동일)
