# Sigwin

Signal 프로토콜 호환 **경량 데스크톱 클라이언트**(비공식)입니다. Rust + [presage](https://github.com/whisperfish/presage) + [Slint](https://slint.dev)(소프트웨어 렌더러)로 만들었습니다.

> Signal과 무관한 비공식 클라이언트입니다. "Signal"은 Signal Technology Foundation의 상표입니다.

## 목표
- 메모리 **100MB 이하** (실계정 기준 working set 약 40MB, private 약 15MB)
- 단일 exe, GPU/WebView 불필요

## 기능
- QR 코드로 휴대폰에 **연결된 기기**로 등록
- 1:1·그룹 대화, 메시지 송수신·답장·수정·삭제·반응, 파일·이미지 첨부
- 스티커, @멘션, 글자 서식, 메시지 고정, 읽음 확인
- Windows 알림과 작업 표시줄 안 읽은 수 배지
- 로컬 DB와 첨부파일은 암호화하며, 키는 Windows DPAPI로 보호합니다.

### 미지원
음성·영상 통화, 스토리, 스티커 팩 삭제, 결제, 한 번 보기 미디어, 사라지는 메시지 타이머 변경, 기존 대화 기록 이전, 대화 음소거·알림 프로필

## 설치·제거
- `sigwin.exe`를 실행하면 `%LOCALAPPDATA%\Sigwin`에 설치되고 시작 메뉴에 등록됩니다. 관리자 권한은 필요 없습니다.
- 제거는 Windows 설정 > 앱 > 설치된 앱에서 합니다. 대화 기록은 원할 때만 함께 삭제됩니다.

## 사용법
- `Enter` 전송 / `Shift+Enter` 줄바꿈 (설정에서 변경 가능)
- 말풍선 우클릭: 반응·답장·고정·수정·삭제
- 그룹에서 `@`: 멘션할 멤버 고르기
- `☺` 스티커, `📎` 첨부, `⚙` 설정(연결 해제 포함)

## 메모리 절약 설계
- Slint **소프트웨어 렌더러** 사용 (femtovg 212MB, Skia 388MB 대비 private 8.4MB)
- 메시지는 60행씩 조회하고, 대화창에는 최대 600행만 유지
- 이미지는 썸네일만 메모리에 올리고 나머지는 디스크 캐시
- 움직이는 스티커 프레임은 화면에 나온 것만, 합계 24MB까지 (넘으면 오래 안 본 스티커부터 정지 화면으로)
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
AGPL-3.0-only (presage, libsignal과 동일). 전문은 [`LICENSE`](LICENSE)에 있습니다.
