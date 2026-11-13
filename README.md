# HENOS

**One Digital World.**

HENOS brings your devices together into one seamless, unified ecosystem. Mac,
Windows PC, Android or iOS: the boundaries between them should not be your
problem. Copy on one device and paste on another today; files, folders and
more to come. It is open source and end-to-end encrypted, with no account, no
server and no network setup.

## The name

HENOS comes from the Greek philosophical idea of *henosis* (ἕνωσις): unity,
union, the state of becoming one. That is the whole vision. HENOS is not just a
clipboard or file-sharing utility; it is one connected digital world in which
all your devices work together as parts of a single environment. Different
devices, one experience, without boundaries.

**Status:** on a local network, any number of your devices find each other,
pair with a 6-digit code or a QR code, and keep an encrypted connection to the
devices they are paired with. Over it they share what you copy (text
everywhere, images between computers) and send each other files, which the
receiving person accepts or declines. There are apps for macOS, Windows and
Android, and a developer tool. The full design and roadmap are in
[docs/PLAN.md](docs/PLAN.md).

## Layout

```
crates/core               shared Rust library: identity, discovery, transport, pairing, sync
crates/app                what every app shares above the core: state, pairing, activity
crates/clipboard-desktop  system clipboard access for Windows, macOS and Linux
crates/cli                headless dev/test tool (binary name: henos)
apps/desktop              Tauri app for Windows and macOS (window, tray, clipboard)
crates/ffi                bindings that let the Android app use the Rust code
apps/android              Android app (Kotlin, Jetpack Compose)
```

## Build

Requires Rust (stable), installed with [rustup](https://rustup.rs).

```bash
cargo test --workspace
```

## The desktop app

Needs Node.js as well as Rust. From `apps/desktop`:

```bash
npm install
```

```bash
npm run dev
```

`npm run build` makes the installer for the system you are on, in
`target/release/bundle`: a `.dmg` on macOS, a setup `.exe` on Windows.

Installers for macOS (Apple silicon), Windows and Android are also built on
GitHub: pushing a version tag
such as `v0.1.0` (or running the Release workflow by hand) produces a draft
release with them attached.

Good to know about the installers today:

- They are not signed with a paid developer certificate yet. macOS will say the
  app is from an unidentified developer (right-click the app and choose Open
  the first time), and Windows SmartScreen will warn before running the setup.
- The Windows setup asks for administrator rights once. It uses them to add a
  firewall rule that lets your other devices reach HENOS on private networks,
  and removes the rule again on uninstall.
- "Start HENOS at login" is a switch in the tray menu, off until you turn it
  on. Started that way, HENOS stays in the tray without opening its window.
- There are no automatic updates yet.
- Intel Macs are not built for.
- The Android app is installed from its `.apk` file (your phone will ask you
  to allow installing from the browser or file manager). Until the project's
  signing key is set up in the repository, each release is signed with a
  different throwaway key, so updating means uninstalling the old one first.

The window lists paired devices, shows a pairing code and QR, and lets you
enter another device's code. Closing the window keeps HENOS running in the
tray so the clipboard stays shared; quit from the tray menu, which also has a
switch to pause clipboard sharing.

By default the app keeps its key and paired-device list in the system's app
data folder. Set `HENOS_DATA_DIR` to use another folder, and
`HENOS_DEVICE_NAME` to choose the name other devices see.

To work on the interface without building the app, serve `apps/desktop` with
any static file server and open `dev/preview.html`, which runs the real
interface against fake data.

## The Android app

In `apps/android`. It pairs by scanning the QR code a computer shows, by typing
a 6-digit code, or by showing a code itself.

- Text copied on a paired device arrives on the phone's clipboard while the
  HENOS notification is showing.
- Android does not let apps read the clipboard in the background, so sending
  from the phone takes one tap: "Send my clipboard" on the HENOS notification,
  or Share then "Send to my devices" from any app.
- HENOS reconnects on its own after the phone restarts.

To build it you need Android Studio (SDK, NDK and command-line tools), Rust
with the Android targets, and `cargo-ndk`:

```bash
rustup target add aarch64-linux-android armv7-linux-androideabi x86_64-linux-android
```

```bash
cargo install cargo-ndk
```

Then, from `apps/android`, build the Rust core for phones and the app. On
macOS or Linux:

```bash
./build-rust.sh && ./gradlew assembleDebug
```

On Windows, `setup-gradle.cmd` (once), then `build-rust.cmd` and
`build-apk.cmd`. The app file ends up in `app/build/outputs/apk/debug`.

The Kotlin file under `app/src/main/java/uniffi` is generated from
`crates/ffi`. After changing that crate, regenerate it:

```bash
cargo build -p henos-ffi && cargo run -p henos-ffi --features cli --bin uniffi-bindgen -- generate --library target/debug/libhenos_ffi.dylib --language kotlin --no-format --out-dir apps/android/app/src/main/java
```

## Try it: two simulated devices on one computer

Each simulated device keeps its key and paired-device list in its own data
folder. Use two terminals.

Terminal 1 shows a pairing code and waits:

```bash
cargo run -q -p henos-cli -- --data-dir target/dev-nodes/a --name "Node A" pair show
```

Terminal 2 joins with that code:

```bash
cargo run -q -p henos-cli -- --data-dir target/dev-nodes/b --name "Node B" pair join --code <code>
```

Now run both. They find each other, connect and report a ping:

```bash
cargo run -q -p henos-cli -- --data-dir target/dev-nodes/a --name "Node A" run
```

```bash
cargo run -q -p henos-cli -- --data-dir target/dev-nodes/b --name "Node B" run
```

Other commands: `devices` lists paired devices, `unpair <name>` forgets one.
Add `--verbose` to any command for debug logs.

## Try it: shared clipboard

Two simulated devices on one computer would share the one real clipboard, so
for that case each uses a text file as its clipboard. Saving the file is a
"copy"; received text is written to it. Add this to each `run` command, with a
different file per device:

```bash
cargo run -q -p henos-cli -- --data-dir target/dev-nodes/a --name "Node A" run --clipboard-file target/dev-nodes/a.clip
```

Then write some text to one file and watch it appear in the other.

On two real computers, share the real clipboard instead:

```bash
cargo run -q -p henos-cli -- --data-dir target/dev-nodes/a run --clipboard
```

How the shared clipboard behaves:

- Text up to 1 MB. Copied images up to 10 MB, between computers; phones
  share images as files for now.
- A device shares with the devices it is paired with directly. Nothing is
  passed on to another device, so pair each device with every other device
  it should share with. There is no limit on how many devices you pair.
- Only new copies travel; connecting does not send what is already there.
- Content that a password manager marks as secret is never shared.
- The tool prints sizes and device names, never the copied text.

## Sending files

In the apps: the Send file button next to a connected device, dragging a file
onto a device in the desktop window, or Share then "Send to my devices" on
Android.

- The other person is asked first. Nothing is saved until they accept.
- Accepted files go to a HENOS folder inside Downloads. An existing file is
  never overwritten; the new one gets a number added.
- A device cannot make another save a file anywhere else: the name it sends
  is reduced to a plain file name.
- Files travel over the same encrypted connection as everything else.

With the developer tool, `run --send <file>` offers a file to each paired
device as it connects, and `run --receive-into <folder>` accepts everything.

## How devices find each other

Two ways at once, so that one failing does not matter:

- Standard network discovery (mDNS).
- HENOS's own beacons: a device announces itself to the local network on UDP
  port 47474 and every HENOS device that hears it answers directly. This
  still works when announcements only get through in one direction.

Beacons go out in a quick burst when HENOS starts or you open it, then one
every 20 seconds. Paired devices also remember each other's last address and
try it directly.

## How pairing stays safe

- The code is never sent over the network. Both devices prove they know it
  using SPAKE2, so someone listening learns nothing.
- A code is good for one pairing and one wrong guess, and expires after two
  minutes. A wrong guess discards it.
- Pairing is tied to both devices' keys, so a device relaying between the two
  cannot pair in their place.
- After pairing, a device only talks to keys on its paired list. Unpairing on
  one device makes the other forget it too.

## Open to everyone

HENOS is free and open source, and it is open to everyone: use it, read the
code, report problems, suggest ideas, or send changes. Issues and pull requests
are welcome.

## License

Apache-2.0. See [LICENSE](LICENSE).
