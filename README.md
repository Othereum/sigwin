# lite-signal

Signal 프로토콜 호환 **경량 데스크톱 클라이언트** (비공식). Rust + [presage](https://github.com/whisperfish/presage) + [Slint](https://slint.dev)(소프트웨어 렌더러).

> Signal Messenger LLC와 무관한 비공식 클라이언트입니다. "Signal"은 Signal Messenger LLC의 상표입니다.

## 목표
- 메모리 **100MB 이하** (Electron 기반 공식 클라이언트는 300~600MB)
- 단일 exe, GPU/WebView 불필요

## 기능
- QR 코드로 휴대폰에 **연결된 기기**로 등록
- 1:1 및 그룹 대화 목록 (최근순, 안 읽은 메시지 수, 검색, 연락처로 새 대화 시작)
- 메시지 송수신, 답장(말풍선 더블클릭), 파일·사진 첨부
- 이미지 썸네일(디스크 캐시), 첨부파일 열기
- 반응·수정·삭제·수신 확인(✓ / ✓✓ / 읽음) 표시
- 전달 확인(delivery receipt) 자동 전송
- 다른 기기에서 읽으면 안 읽음 배지 해제
- Windows 알림, 창 제목에 안 읽은 메시지 수 표시
- 로컬 DB는 **SQLCipher로 암호화**하며, 키는 Windows **DPAPI**로 보호합니다(현재 Windows 사용자에게만 복호화 가능)

### 미지원
음성·영상 통화, 스토리, 스티커 이미지(이모지로 표시), 결제, 한 번 보기 미디어, 기존 대화 기록 이전(연결 이후 메시지부터 표시)

## 메모리 절약 설계
- Slint **소프트웨어 렌더러**: GPU 드라이버 상주 메모리 없음
- 메시지는 **페이지 단위**(60행)로 조회하고, 대화창에는 최대 600행만 유지
- 이미지는 260px 썸네일만 메모리에 올리며, 대화방을 바꾸면 해제
- 원본 첨부파일과 썸네일은 디스크 캐시 사용
- SQLite 페이지 캐시 축소
- 파일 선택 대화상자는 **자식 프로세스**(`lite-signal.exe --pick-files`)에서 실행합니다. 셸 확장 DLL이 메인 프로세스에 남지 않습니다(측정 결과 +28MB, 닫은 후에도 +10MB가 남던 것을 0으로 줄임).
- **Segment Heap** 매니페스트를 적용해 대량 동기화 후 해제된 메모리를 OS에 반환합니다.
- 10초마다 `mem: rust heap …, private …`을 로그로 남깁니다(Rust 힙과 네이티브 메모리 구분).

### 측정치 (릴리스, Windows 11)
| 상태 | Working Set | Private |
|---|---|---|
| QR 연결 화면 | 31MB | 9.6MB |
| 실계정 연결 후 대화 목록 | 41MB | 15MB |
| 데모(대화 40개 / 메시지 300개) | 29MB | 8.5MB |

`LITE_SIGNAL_DEMO=1 LITE_SIGNAL_SELFTEST=1`: 파일 대화상자와 알림의 메모리 비용을 자동으로 측정합니다.

## 빌드 (Windows)
필요한 도구: Rust(stable), protoc, LLVM(clang), NASM, CMake

```powershell
winget install Google.Protobuf LLVM.LLVM NASM.NASM Kitware.CMake
cargo build --profile fast   # 개발용: 최적화 O, LTO X, 증분 빌드. 재빌드 수 초~20초 -> targetast\
cargo build --release        # 배포용: thin LTO, 증분 재빌드 약 2.5분 -> targetelease\
```
`.cargo/config.toml`에 `PROTOC`/`LIBCLANG_PATH` 경로가 설정되어 있습니다. 경로가 다르면 수정하세요.
SQLCipher는 순수 Rust 암호화 provider(`bundled-sqlcipher-custom-crypto`)를 쓰므로 OpenSSL이 필요 없습니다.

## 실행
```powershell
target\release\lite-signal.exe
```
- 데이터 위치: `%LOCALAPPDATA%\lite-signal\lite-signal\data`(환경변수 `LITE_SIGNAL_DATA`로 변경 가능)
- `LITE_SIGNAL_DEMO=1`: 계정 없이 가짜 데이터로 UI 확인
- 로그: `RUST_LOG=debug` (콘솔에서 실행 시 stderr)

## 단축키
- `Enter` 전송 / `Shift+Enter` 줄바꿈
- 말풍선 더블클릭: 답장

## 라이선스
AGPL-3.0-only (presage, libsignal과 동일)
