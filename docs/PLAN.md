# HENOS plan: open-source cross-device ecosystem (Windows + macOS + Android)

Name: **HENOS**, from the Greek *henosis* (unity). Tagline: One Digital World.

## Context

You work across a Windows PC, a Mac and an Android phone and want what Apple's
ecosystem gives (shared clipboard, files, using another device's camera), but
open source, end-to-end encrypted, and working across all three. It must also be
something other people can install and use with almost no setup: no account, no
server, no network configuration.

Decisions already made:

- Build our own single app, on proven open-source parts (no home-made crypto).
- First version (MVP): **secure pairing (QR code or 6-digit code) + shared clipboard**.
- Stack: **Rust core + Tauri desktop app + native Kotlin Android app**.
- **Same WiFi/hotspot first**; secure link across networks in a later phase.

## Architecture

One Rust library holds all networking, crypto and sync logic. Each platform is a
thin shell around it, so every feature is written once.

```
henos/
  crates/
    core/      identity, discovery, transport, pairing, protocol, clipboard sync logic
    ffi/       UniFFI bindings exposing core to Kotlin
    cli/       headless dev/test tool (pair, list, send/watch clipboard)
  apps/
    desktop/   Tauri v2 (Rust backend + small TypeScript UI), tray app for Win + Mac
    android/   Kotlin + Jetpack Compose, foreground service, loads core via UniFFI
  .github/workflows/   CI: test + build installers for all three platforms
```

### Proven parts we build on

| Need | Choice | Why |
|---|---|---|
| Encrypted transport | **iroh** (QUIC, TLS 1.3) | Connections are dialed by a device's public key, so every link is encrypted and authenticated by design. It already includes LAN discovery, and NAT hole-punching + relays that we simply switch on in the remote phase. |
| Device identity | Ed25519 keypair per device | Private key stored in the OS keychain (`keyring` crate on desktop, Android Keystore on the phone). |
| Pairing by short code | **SPAKE2** (`spake2` crate) | Same technique as Magic Wormhole: a 6-digit code is safe even if someone is listening on the WiFi. |
| LAN discovery | mDNS (iroh local discovery on desktop, Android `NsdManager` on the phone) | Zero configuration. |
| Desktop clipboard | `clipboard-rs` (read, write, change watcher) | Covers Windows and macOS, text and images. |
| Rust to Kotlin | UniFFI + `cargo-ndk` | Standard, generated bindings. |
| Wire format | `serde` + `postcard`, versioned, with a capability handshake | Lets files and camera be added later without breaking old versions. |

Exact crate versions and API names get checked against current docs at
implementation time (iroh's API in particular has changed between releases).

### Pairing (the security foundation)

- **QR path** (phone to computer): the computer shows a QR containing its public
  key and a one-time random secret. The phone scans, connects to that exact key,
  and presents the secret. Both sides save each other as trusted.
- **Code path** (computer to computer): device A shows a 6-digit code. Device B
  lists nearby devices in pairing mode, the user picks A and types the code. The
  two run SPAKE2 with the code and confirm each other's public keys.
- Codes are single-attempt and expire after 2 minutes.
- After pairing, a device only ever accepts connections from keys in its trusted
  list. Devices can be unpaired from any side.

### Clipboard sync

- Desktop: watch for clipboard changes, send to all connected paired devices,
  apply incoming items. Text first, then images (size capped at about 10 MB).
- Loop prevention: every item carries a content hash and origin device, so an
  item just applied is never re-sent.
- Items marked concealed by password managers are skipped, never synced.
- **Android limitation (set by Android, same as KDE Connect):** apps cannot read
  the clipboard in the background since Android 10. So computer to phone is
  automatic, but phone to computer takes one tap: a notification action, a Quick
  Settings tile, or "Share to HENOS".

### "Install and it just works" for other users

- No account and no server in v1. Devices find each other on the local network.
- Desktop: tray app, starts on login (`tauri-plugin-autostart`), self-updates
  from GitHub Releases (`tauri-plugin-updater`).
- Installers built by CI: Windows `.msi`/`.exe`, macOS `.dmg`, Android `.apk`.
- First-run permission prompts handled in the UI: macOS Local Network access,
  Windows Firewall, Android notifications and nearby-devices/local-network.
- Costs to know about: a smooth macOS install needs an Apple Developer ID
  (99 USD/year) for signing and notarization. Without it the app still works but
  users must right-click to open it the first time. Windows shows a SmartScreen
  warning until the installer is code-signed.
- License: Apache-2.0 (can be changed before first release).

## MVP milestones

0. **Setup.** Choose the project folder, `git init`. Install the missing
   toolchain on this Mac: Rust (rustup), Android Studio (SDK + NDK), full Xcode,
   `cargo-ndk`, `tauri-cli`. Currently present: Node 26, Java 21, git. Missing:
   Rust, Android SDK, Xcode build tools.
1. **Core transport.** Device identity, iroh endpoint with LAN discovery only
   (relays off), protocol handshake. Proven with the CLI: two nodes find each
   other and exchange a message.
2. **Pairing.** QR and 6-digit code flows, trusted-device store, unpair.
3. **Desktop app.** Tauri tray app: pairing screens, device list, clipboard sync
   between two computers.
4. **Android app.** Compose UI, QR scanner, foreground service, receive
   clipboard automatically, send by tap/share.
5. **Packaging.** GitHub Actions builds and tests on all platforms, installers,
   autostart, updater.

## Later phases (designed for, not built in the MVP)

- **Files:** send files and browse a paired device's shared folders, resumable
  transfers over the same connection.
- **Remote link:** switch on iroh hole-punching and relays so devices connect
  from anywhere. The relay only ever sees encrypted traffic and can be
  self-hosted.
- **Camera:** capture on Android (CameraX) or Mac (AVFoundation), hardware H.264,
  low-latency stream to the other machine, shown as a real webcam there. This is
  the hardest part: Windows 11 virtual camera (Media Foundation) and a macOS
  Camera Extension (requires the Apple Developer ID).
- **Extras:** notification mirroring, phone as microphone, shared keyboard/mouse.

## Verification

- `cargo test` in `crates/core`: pairing succeeds with the right code and fails
  with a wrong or expired one; untrusted keys are rejected; protocol round-trips;
  clipboard loop prevention.
- Integration test: two in-process nodes pair and sync a clipboard item over
  loopback.
- Manual on this Mac: two CLI instances with separate data folders and a fake
  clipboard, then the real desktop app against the CLI.
- Real devices on one WiFi: Mac and Android phone paired by QR, copy on Mac and
  paste on phone, share from phone and paste on Mac.
- **Windows cannot be tested from this Mac.** CI builds the Windows installer;
  you install it on your PC and we run the same pair/copy/paste checks there.
- The Android emulator cannot do LAN discovery, so phone testing needs your real
  device with USB debugging.
