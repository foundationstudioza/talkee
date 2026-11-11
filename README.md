# Talkee

Use your phone as a microphone for your computer.

Your phone already has a better microphone than your laptop, and it can be
somewhere more useful. Talkee streams that microphone to your PC and feeds it
into a virtual audio device, so anything that can read a microphone — Discord,
Zoom, Teams, OBS, Audacity — reads your phone instead.

**Status: beta.** Windows 10/11 and Android 8+. It works day to day over Wi-Fi, encrypted, with
echo cancellation and noise suppression. Builds are not code-signed yet.

## Install

Get both apps from **https://foundationstudioza.github.io/talkee/** or the
[Releases](https://github.com/foundationstudioza/talkee/releases/latest) page.

* **Windows 10/11:** run `Talkee-Setup-<version>.exe`. It installs per user, with no administrator
  prompt. The builds are not code-signed, so Windows asks you to confirm.
* **Android 8+:** install `Talkee.apk` (allow installs from your browser when asked).

**First run on Windows:** Windows Firewall asks whether to let Talkee use the network. Click
**Allow access** (private networks is enough). If the prompt is dismissed, Windows quietly blocks
Talkee; the setup guide in the Talkee window notices and offers a one-click fix (it asks Windows for
permission). By hand, in an Administrator PowerShell:
`netsh advfirewall firewall delete rule name="Talkee.exe"`, then start Talkee again.

## Using it

1. Start Talkee on the PC. It lives in the tray and opens its window on first run.
2. The setup guide checks for a virtual microphone (and installs VB-CABLE if there is none — one
   Windows permission prompt and a restart), checks the firewall, and shows a QR code.
3. Open Talkee on the phone and scan the code. You only do this once: both sides remember the pairing.
4. In Discord, Zoom or OBS, pick the microphone called **`CABLE Output`**.

After that the phone streams whenever the app is running, keeps going with the screen off, and finds
the PC again by itself if its address changes.

**On the PC:**

* **Tray icon:** a two-bar level meter (audio arriving, audio leaving) coloured by state. Its menu has
  the switches, *Reconnect phone*, *Install VB-CABLE* when needed, *Restart to update* when an update
  is ready, and *Open Talkee…*.
* **Window:** a native window (Rust, `egui`; no browser). It shows the state, live levels, the delay,
  the switches and the pairing code.
* **States:** *no virtual microphone*, *looking for your phone*, *live*, and **connected but silent**.
  The last is the one that matters most. Silence that nobody points out is the worst way a
  microphone can fail.
* **Switches:** microphone on/off, noise suppression, echo cancellation, auto level. The tray, the
  window and the phone share them, and a change in one shows in the others. They are remembered in
  `%APPDATA%\Talkee\settings.json`.
* **Log:** `%APPDATA%\Talkee\talkee.log` (the previous one is kept as `talkee.old.log`).
  Start with `--console` to watch it live, `--help` for every option.

**Updates:** both apps check GitHub releases on their own. Windows downloads the new installer, checks
its SHA-256 and offers *Restart to update*. Android downloads the APK and shows an *Install* line;
Android always asks you to confirm the install itself.

## How it works

```
phone mic ─► Opus (Android encoder) ─► AES-256-GCM ─► UDP 47801 ─┐
                                                                 ▼
      PC: check + open ─► Opus decode / conceal ─► echo cancel + noise suppress ─► auto level
          ─► resample (ratio trimmed by the drift controller) ─► queue ─► CABLE Input
                                                                            │
                     Discord / Zoom / OBS read  "CABLE Output"  ◄───────────┘
```

* **Pairing:** the PC makes a 32-byte key and shows it once as a QR code. The phone proves it holds
  the key over TCP 47800 (challenge/response); the key itself never crosses the network. Stored with
  DPAPI on Windows and the Android Keystore on the phone. The PC refuses unencrypted audio by default.
* **Discovery:** the PC advertises over mDNS, so a paired phone finds it after its address changes.
  The QR code also carries the address, for networks where mDNS is blocked.
* **Audio:** Opus at 48 kHz mono, 10 ms frames (raw PCM on phones older than Android 10). An adaptive
  jitter buffer sizes itself from how evenly packets arrive (50–200 ms). A drift controller keeps the
  queue at that depth by trimming the resampler's ratio. Echo cancellation is WebRTC AEC3 fed with
  what the PC is playing.

The wire format and the crypto are specified in [`docs/PROTOCOL.md`](docs/PROTOCOL.md). How to read
the receiver's health line and what was measured is in [`docs/LATENCY.md`](docs/LATENCY.md).

## Repository layout

```
Talkee/
├─ rust/                    # the PC side, one Cargo workspace
│  ├─ talkee-proto/         # wire format: 24-byte audio header, control messages. No platform code
│  ├─ talkee-crypto/        # pairing link, proof, session keys, AES-GCM sealing
│  ├─ talkee-core/          # the receive pipeline: decode, jitter, drift, echo, level, queue. No platform code
│  ├─ talkee-vmic/          # the platform boundary: plays into a virtual cable (cpal)
│  ├─ talkee-discovery/     # mDNS advertise and browse
│  ├─ talkee-store/         # the pairing key on disk, DPAPI-protected
│  ├─ talkee-desktop/       # the app users run: tray, window, setup, updater  → talkee-app.exe
│  └─ talkee-cli/           # developer tool: probe, cable, host, send, discover  → talkee.exe
├─ app/                     # the phone app: Flutter UI (lib/) over a Kotlin engine (android/)
├─ installer/talkee.iss     # Inno Setup script for Talkee-Setup.exe
├─ site/                    # the download page (GitHub Pages)
├─ docs/                    # PROTOCOL.md, LATENCY.md
└─ .github/workflows/       # ci, release, pages
```

The rule that keeps the pipeline testable: **`talkee-core` has no platform code.** Device output goes
through the `VirtualMicWriter` trait, implemented in `talkee-vmic`.

On the phone, the Flutter side is only the screens (scanner, microphone screen). Capture, Opus,
pairing, crypto, discovery and updates are Kotlin in `app/android/app/src/main/kotlin/`, behind a
plugin with four verbs: start, stop, status, pair.

## Building

PC (needs Rust 1.80+ and CMake, for the bundled libopus):

```powershell
cargo build --release              # target\release\talkee-app.exe is the app
cargo test --workspace             # ~350 tests
cargo clippy --workspace --all-targets
```

Phone (needs Flutter and the Android SDK):

```powershell
cd app
flutter test
flutter build apk --debug
adb install -r build\app\outputs\flutter-apk\app-debug.apk
```

The Kotlin unit tests (crypto vectors shared with Rust, Opus packets, pairing) run with
`./gradlew :app:testDebugUnitTest` from `app/android` after `flutter build apk --config-only`.

### Without a phone

`talkee-cli` stands in for the phone, so the PC side can be exercised on its own:

```powershell
cargo run -p talkee-cli -- probe                          # which audio endpoints exist
cargo run -p talkee-cli --release -- host                 # the receiver, headless
cargo run -p talkee-cli --release -- send --seconds 30    # synthesised audio, like a phone
```

Note that `talkee host` accepts plaintext by default (for measurement); pass `--secure` to make it
pair like the app does.

## Releasing

```bash
git tag v0.1.8 && git push origin v0.1.8
```

The `release` workflow tests, builds the Windows installer and the signed Android APK, and publishes
them with `update.json` (what installed copies check). The Android signing key is in the repository
secrets `ANDROID_KEYSTORE_B64` and `ANDROID_KEYSTORE_PASSWORD`; **it must never change**, or installed
phones can no longer update. `ci` runs on every push; `pages` deploys `site/`.

## Why a virtual cable

**On Windows there is no user-mode way to create a microphone.** Microsoft's
[ACX documentation](https://learn.microsoft.com/en-us/windows-hardware/drivers/audio/acx-audio-class-extensions-overview)
is explicit that ACX is built on the kernel-mode driver framework. A device appears in Discord only if
a signed kernel driver put it there, so Talkee plays into VB-Audio's VB-CABLE, which it may bundle.

| Cable | Platform | Can we ship it? |
|---|---|---|
| **VB-CABLE** | Windows | **Yes**, provided the user can see it is VB-Audio's and can donate |
| VB-CABLE A+B / C+D | Windows | **No**, VB-Audio forbids bundling them |
| Voicemeeter | Windows | Free to distribute as-is; Potato cannot be bundled |
| Virtual Audio Cable | Windows | No, paid per machine |
| BlackHole | macOS | **No**, unless the whole app is GPL-3.0 |
| PipeWire / PulseAudio | Linux | Not needed: a virtual source is a shell command away |

`talkee-vmic::cable` encodes this as data, and a test asserts only VB-CABLE is ever offered for
install. The licence terms and the package's hash and signature are in
[`rust/talkee-desktop/assets/README.md`](rust/talkee-desktop/assets/README.md).

## Code rules

* `#![forbid(unsafe_code)]` in `talkee-proto`, `talkee-crypto`, `talkee-core` and `talkee-vmic`.
  `talkee-desktop` and `talkee-store` deny it, with the Win32 calls isolated in a few named modules
  (`message_pump`, `shell`, `single`, `logging`, `dpapi`).
* The audio callback never allocates, never blocks and never resamples (see `talkee_vmic::build_typed`).
* Decisions about what the user is told are pure functions (`talkee-desktop/src/state.rs`), tested
  without a device.
* Comments say *why*, and the ones that matter most record what was measured, so nobody "simplifies"
  a fix back out.
