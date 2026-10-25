# Talkee

Use your phone as a microphone for your computer.

Your phone already has a better microphone than your laptop, and it can be
somewhere more useful. Talkee streams that microphone to your desktop and feeds it
into a virtual audio device, so anything that can read a microphone — Discord,
Zoom, Teams, OBS, Audacity — reads your phone instead.

## Install (beta)

Get both apps from **https://foundationstudioza.github.io/talkee/** or the
[Releases](https://github.com/foundationstudioza/talkee/releases/latest) page.

* **Windows 10/11:** run `Talkee-Setup.exe`. It installs per user, with no administrator prompt. These
  are beta builds and are not code-signed, so Windows asks you to confirm.
* **Android 8+:** install `Talkee.apk` (allow installs from your browser when asked).

**First run on Windows:** Windows Firewall asks whether to let Talkee use the network. Click
**Allow access** (private networks is enough). If you dismiss it, Windows quietly blocks Talkee and the
phone will not connect; remove the block with
`netsh advfirewall firewall delete rule name="Talkee.exe"` in an Administrator PowerShell, then start
Talkee again to get the prompt back.

Both apps look for updates on their own. Windows downloads the new version quietly and offers
*Restart to update* in the tray menu and in the window. Android downloads it and shows an
*Install* line at the top of the screen; Android always asks you to confirm the install itself.

The desktop app runs from the tray with no console window; its log is
`%APPDATA%\Talkee	alkee.log`. Start it with `--console` to watch the log live, `--no-update-check`
to skip update checks, and `--help` for the rest.

## Releasing

```bash
git tag v0.2.0 && git push origin v0.2.0
```

The `release` workflow tests and builds the Windows installer and the signed Android APK, then
publishes them with `update.json` (what installed copies check). The Android signing key is in the
repository secrets `ANDROID_KEYSTORE_B64` and `ANDROID_KEYSTORE_PASSWORD`; **it must never change**, or
installed phones can no longer update. The landing page in `site/` is deployed by the `pages` workflow.

## Status: M0, the spike

M0 is deliberately unfinished and deliberately ugly. It exists to answer one
question honestly before the rest of the architecture is committed to:

> **How long does audio actually take to get from a phone to a virtual
> microphone?**

There is no codec, no encryption, no discovery and no UI yet. What there *is* is
a working audio path and an instrument panel that measures it.

### What is verified working

Measured on a Windows 11 machine with VB-CABLE, using `talkee send` as a
stand-in for the phone:

* Endpoint detection correctly identifies both installed cables, including
  distinguishing the free `VB-CABLE` from `VB-CABLE A+B (16 channel)`.
* Audio flows UDP → parse → decode → resample → queue → `CABLE Input`, and
  appears as `CABLE Output`. **800/800 datagrams, 0% loss, 0 late, 0 dropped.**
* 257 unit tests pass; zero compiler warnings.

### What M0 already taught us

Three findings that changed the design, all recorded in `docs/LATENCY.md`:

1. **A queue without prefill is worse than no queue.** At 0 prefill, 65% of audio
   callbacks padded with silence — an audible mess that still reported "0% loss".
   Buffering 60 ms before playback starts took underruns to **0**.
2. **A drift figure needs a long window to mean anything.** An 8-second window
   reported +17 800 ppm, about 90× any plausible crystal difference, because it
   was measuring process startup rather than clocks.
3. **Raw PCM is ~1.6 Mbps.** That single number is the entire argument for Opus
   in M1.

## Quick start

Three pieces: a phone app, a desktop app, and the virtual cable they meet in.

**On the PC** — build once, then run the app:

```powershell
cargo build --release
.\target\release\talkee-app
```

**On the phone** — build and install the Flutter app:

```powershell
cd app
flutter build apk --debug
adb install -r build\app\outputs\flutter-apk\app-debug.apk
```

Open it once to grant the microphone permission. After that it streams by itself
whenever it is running, restarts itself if the PC goes away, and keeps working with
the screen off.

The desktop app shows one of four states and a level meter: *no virtual microphone*,
*looking for your phone*, *live*, or **connected but silent**. The last one is the
state worth having — it is the only place in the product that tells you your
microphone has stopped carrying your voice, and for a microphone, silence with no
indication is the worst failure there is.

### Without a phone

`talkee-cli` synthesises the phone, so the pipeline can be exercised and measured
with nothing but a PC:

```powershell
cargo run -p talkee-cli -- probe                          # what endpoints exist
cargo run -p talkee-cli --release -- host                 # the receiver, headless
cargo run -p talkee-cli --release -- send --seconds 30    # stand in for the phone
```

Then record from the endpoint `probe` names — usually `CABLE Output (VB-Audio
Virtual Cable)` — in Audacity or OBS. See `docs/LATENCY.md` for how to read the
health line, and for what its numbers mean.

`probe` output on a machine with VB-CABLE installed looks like:

```
  render CABLE Input (VB-Audio Virtual Cable)    VIRTUAL, = VB-CABLE
  render CABLE In 16ch (VB-Audio Virtual Cable)  VIRTUAL, = VB-CABLE A+B (16 channel)
  capture CABLE Output (VB-Audio Virtual Cable)   VIRTUAL
```

## Repository layout

```
Talkee/
├─ rust/
│  ├─ talkee-proto/   # wire format. No platform code, no DSP
│  ├─ talkee-core/    # jitter buffer, drift control, UDP. No platform code, no UI
│  ├─ talkee-vmic/    # the platform boundary: opens a virtual mic endpoint
│  └─ talkee-cli/     # M0 spike: probe, host, send
├─ docs/
│  ├─ LATENCY.md      # how to measure, and how to read the numbers
│  └─ PROTOCOL.md     # the contract, written once
└─ Cargo.toml         # workspace
```

The rule that keeps the pipeline testable: **`talkee-core` has zero platform
code.** All device I/O goes through two boundaries — `MicSource` on the phone and
`VirtualMicWriter` on the desktop. That is what lets the whole pipeline run in CI
on a Linux runner against a PulseAudio null sink, with no virtual cable.

## Why a virtual cable is necessary

This is the hard part of the project, and it is worth stating plainly because it
shapes the roadmap.

**On Windows there is no user-mode way to create a microphone.** Microsoft's
[ACX documentation](https://learn.microsoft.com/en-us/windows-hardware/drivers/audio/acx-audio-class-extensions-overview)
is explicit that ACX is *"built using the Kernel Mode Driver Framework (KMDF) and
not the User Mode Driver Framework (UMDF)"*. A device appears in Discord only if a
**signed kernel driver** put it there.

So until we ship our own signed driver (M6), Talkee plays into an existing virtual
cable. That is what `talkee probe` is for, and it is why the UI cannot simply say
"pick a microphone".

### Licensing — these are not interchangeable

| Cable | Platform | Can we ship it? |
|---|---|---|
| **VB-CABLE** | Windows | **Yes.** VB-Audio permits redistributing and silently installing it with another app, provided the user can see it is VB-Audio and can donate |
| VB-CABLE A+B / C+D | Windows | **No.** VB-Audio states these "cannot be distributed or bundled with other product" |
| Voicemeeter | Windows | Free to distribute as-is; Potato cannot be bundled |
| Virtual Audio Cable | Windows | No. Paid, per-machine |
| BlackHole | macOS | **No**, unless the whole app is GPL-3.0 |
| PipeWire / PulseAudio | Linux | Not needed — a virtual source is a shell command away |

`talkee-vmic::cable` encodes this as data, and a test asserts only VB-CABLE is
ever offered for auto-install.

## Roadmap

| # | Milestone | Goal |
|---|---|---|
| **M0** | **Spike — you are here** | Measure honest latency through both audio paths |
| **M1** | **Audio engine — in progress** | Jitter buffer and drift correction are **done** (`DepthController`; measurements in `docs/LATENCY.md`). Opus + FEC is next |
| M2 | Security | QR pairing, mDNS discovery, AES-256-GCM |
| M3 | GUI | Flutter desktop + mobile, one-click cable setup |
| M4 | Robustness | Reconnect, roaming, USB transport, stats panel |
| M5 | Platforms | macOS (BlackHole), Linux (PipeWire), iOS |
| M6 | Own drivers | ACX KMDF on Windows, AudioServerPlugIn on macOS |

The M0 exit criteria are in `docs/LATENCY.md`. The short version: if the clap test
lands under ~150 ms, the architecture is sound and M1 begins.

## Development

```powershell
cargo test --workspace     # 68 tests
cargo build --release      # ~40 s from cold with thin LTO
```

Type and lint discipline, since the audio thread has hard deadlines:

* `#![forbid(unsafe_code)]` in every crate.
* The audio callback never allocates, never blocks, never resamples. There is a
  comment on `talkee_vmic::build_typed` spelling this out because it is the one
  place where a well-meaning refactor will cause audible clicks.
* Wrapping sequence arithmetic is tested explicitly, including the `u32::MAX` →
  `0` wrap.

Every non-obvious decision in the code is commented with *why*, not *what*. The
comments that matter most record things that were measured — the 65% underrun
figure, the +17 800 ppm artefact — because those are the ones that stop someone
"simplifying" the fix back out.

## Notes for whoever picks this up next

Things that are known, deliberately unfinished, and not bugs:

* `talkee host` has no signal handler. Ctrl+C terminates the process.
* `SO_RCVBUF` is not enlarged (`std` cannot set socket options). The Windows
  default is worth ~330 ms at raw-PCM rates, which exceeds the default buffer
  depth, so it is not yet a problem. M1 should use `socket2`.
* `SharedQueue::push` still discards the *oldest* samples when it overflows, but it is
  no longer the drift strategy. `DepthController` holds the depth by trimming the
  resampler's ratio, and `slip` is now a failure signal rather than a routine one. What
  is left of the old behaviour is the capacity guard, which stops a burst from growing
  latency without bound, and `trim`, which is the loop choosing to shed.
* `magnum-opus` **0.3.2** is a minimal binding: it has `set_bitrate`,
  `set_inband_fec` and `set_packet_loss_perc`, but **no `set_complexity` and no
  `set_dtx`**, and `Application` is construction-only. M1 should confirm whether
  `Decoder` supports decoding an empty slice for packet-loss concealment before
  committing to it.

