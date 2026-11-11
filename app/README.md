# Talkee for Android

The phone half of Talkee: it captures the microphone and streams it, encrypted, to the PC.

## Layout

* `lib/main.dart` contains the two screens. The scanner is shown until the phone is paired, and the
  microphone screen after that (status, level, mute and the shared switches, and an update line).
* `android/app/src/main/kotlin/net/talkee/talkee_app/` holds everything else:

| File | Job |
|---|---|
| `TalkeeEngine.kt` | The pipeline: capture, encode, seal, send; reconnect and retry |
| `CaptureService.kt` | Microphone foreground service, so capture survives the screen turning off |
| `OpusEncoder.kt` | Opus through Android's own codec (Android 10+); raw PCM otherwise |
| `Control.kt` | The pairing handshake and control connection (`HELLO` / `CHALLENGE` / `PAIR`) |
| `Crypto.kt` | HMAC proofs, session keys, AES-GCM, byte for byte with `rust/talkee-crypto` |
| `PairLink.kt` | Parses the `talkee://pair?...` link in the QR code |
| `PairingStore.kt` | Keeps the link, encrypted with an Android Keystore key |
| `HostFinder.kt` | Finds the paired PC by its fingerprint over mDNS when its address changes |
| `Updater.kt` | Checks GitHub releases, downloads and verifies the APK, hands it to the installer |
| `TalkeePlugin.kt` | The bridge to Dart: start, stop, status, pair |

The wire format is specified in [`../docs/PROTOCOL.md`](../docs/PROTOCOL.md).

## Build and test

```powershell
flutter test
flutter build apk --debug
adb install -r build\app\outputs\flutter-apk\app-debug.apk

# Kotlin unit tests (crypto vectors shared with Rust, Opus packets, pairing)
flutter build apk --config-only
cd android; ./gradlew :app:testDebugUnitTest
```

Release APKs are built and signed by `.github/workflows/release.yml`. The signing key must never
change, or installed phones can no longer update.
