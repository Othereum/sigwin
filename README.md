# Sigwin

Signal 프로토콜 호환 **경량 데스크톱 클라이언트**(비공식)입니다. Rust + [presage](https://github.com/whisperfish/presage) + [Slint](https://slint.dev)(소프트웨어 렌더러)로 만들었습니다.

> Signal과 무관한 비공식 클라이언트입니다. "Signal"은 Signal Technology Foundation의 상표입니다.

## 목표
- 메모리 **100MB 이하** (실계정 기준 working set 약 40MB, private 약 15MB)
- 단일 exe, GPU/WebView 불필요

## 기능
- QR 코드로 휴대폰에 **연결된 기기**로 등록
- 1:1·그룹 대화, 대화 검색, 연락처로 새 대화 시작, 프로필 사진
- 메시지 송수신, 답장, 수정, 삭제, 반응, 파일·이미지 첨부 (`Ctrl+V` 붙여넣기 포함)
- 메시지 고정, @멘션, 글자 서식(굵게·기울임·취소선·고정폭·스포일러), 링크 열기
- 연속 메시지 묶음, 입력 중 표시, 메시지 초안, 사라지는 메시지, 컬러 이모지
- 전송·전달·읽음 상태와 읽음 확인, 메시지 정보 창
- 스티커 보내기·받기, 움직이는 스티커 재생, 팩 추가(기본 추천 팩, `signal.art` 링크)
- Windows 알림(클릭하면 그 대화 열기)과 안 읽은 수 배지
- 설정(⚙): 알림, 테마·크기, 전송 키, 계정, 저장 공간
- 로컬 DB는 SQLCipher로, 첨부파일과 캐시는 AES-256-GCM으로 암호화합니다. 키는 Windows DPAPI로 보호합니다.

### 미지원
음성·영상 통화, 스토리, 스티커 팩 삭제, 결제, 한 번 보기 미디어, 사라지는 메시지 타이머 변경, 국기 이모지, 기존 대화 기록 이전

## 사용법
- `Enter` 전송 / `Shift+Enter` 줄바꿈 (설정에서 반대로 변경 가능)
- 말풍선 더블클릭: 답장, 우클릭: 반응·답장·고정·수정·삭제
- 글자를 선택하고 `Ctrl+B` 굵게 / `Ctrl+I` 기울임 / `Ctrl+Shift+X` 취소선 / `Ctrl+E` 고정폭 / `Ctrl+Shift+B` 스포일러
- 그룹에서 `@`: 멘션할 멤버 고르기
- `☺` 스티커, `📎` 파일 첨부, `⚙` 설정(연결 해제 포함)

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
