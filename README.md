# Sigwin

**English** • [한국어](README.ko.md)

### An ultra-light Signal desktop client (unofficial)

**Rust** • [**presage**](https://github.com/whisperfish/presage) • [**Slint**](https://slint.dev) (software renderer)

> An unofficial client, not affiliated with Signal. "Signal" is a trademark of the Signal Technology Foundation.

> **Early version (0.x).** Expect bugs. Your chats also stay in the official app on your phone.

<h3>
    <a href="https://github.com/Othereum/sigwin/releases/latest">📥 Download the latest version</a>
    <span> • </span>
    <a href="https://github.com/Othereum/sigwin/issues">🐛 Report a bug</a>
</h3>

---

## Why Sigwin
- **Light**: uses very little memory, so it can stay open all day.
- **IME-friendly**: `@mentions` work even while you're still composing a character (Korean, Japanese, Chinese input).
- **See who read it**: the profile photos of those who read your message show under it.
- **No waiting for link previews**: paste a link and press Enter before the preview appears, and it is still sent with the preview.
- **Per-chat notification settings**: set notification content, sound and badge for each chat, and override the settings synced from your phone on this PC only.
- **Image viewer**: images open in their own window at a fitting size, even when the chat window is small. Zoom fast with the wheel; click the image to close it.

## Features
- **Chats**: 1:1 and groups, starting a chat, message requests, replies, edits, deletes, reactions, @mentions, text formatting, pinned messages, read receipts
- **Attachments**: photos, videos and files, stickers, animated GIF, WebP and APNG
- **Notifications**: Windows notifications, unread badge
- **Always on**: keeps running in the tray when the window is closed, opens at login, updates itself
- **Getting started**: links to your phone as a **linked device** with a QR code, brings over your recent chat history from the phone
- **Security**: local data is encrypted, with the key protected by Windows DPAPI
- **Languages**: English, Korean

### Not supported
Voice and video calls, stories, removing sticker packs, payments, view-once media, changing the disappearing message timer, notification profiles

## Designed for low memory
- Slint's **software renderer** (8.4MB private, against 212MB for femtovg and 388MB for Skia)
- Messages are read 60 rows at a time, and a chat holds at most 600 rows
- Only thumbnails of images are kept in memory; the rest is cached on disk
- Animated sticker frames only for what's on screen, 24MB in all (LRU)
- File dialogs run in a child process
- Measurements and the reasons behind these choices: [`docs/decisions.md`](docs/decisions.md)

## Building (Windows)
You need Rust (stable), protoc, LLVM (clang), NASM and CMake.

```powershell
winget install Google.Protobuf LLVM.LLVM NASM.NASM Kitware.CMake
cargo build --profile fast   # day to day
cargo build --release        # for distribution (thin LTO)
cargo test
```
- The build goes to `target\fast\` or `target\release\`.
- If LLVM isn't in its default place (`C:\Program Files\LLVM\bin`), set `LIBCLANG_PATH`; set protoc with `PATH` or `PROTOC`.
- A running app locks its exe and the build fails, so quit it first.
- An exe under `target` skips installing and runs where it is.

## Running
```powershell
target\fast\sigwin.exe
```
| Environment variable | Use |
|---|---|
| `SIGWIN_DATA` | data folder (default `%LOCALAPPDATA%\Sigwin\data`) |
| `RUST_LOG` | log level (e.g. `info,sigwin=debug`) |
| `SIGWIN_DEMO=1` | try the UI with fake data, no account |
| `SIGWIN_LANG` | UI language for this run (`en`, `ko`), over the setting |

More demo and headless options: [`docs/headless.md`](docs/headless.md).

## License
AGPL-3.0-only (the same as presage and libsignal). The full text is in [`LICENSE`](LICENSE). Notices for the included libraries are in the app under `⚙` → About → Open source licenses, or in a release's `licenses.html`.
