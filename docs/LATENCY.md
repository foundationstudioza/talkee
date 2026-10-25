# Measuring Talkee's latency

M0 exists to answer one question honestly: **how long does audio take to get from
a phone's microphone to a virtual microphone on the desktop?** Everything else in
the roadmap depends on that number, and a GUI built on an unmeasured pipeline is
how projects like this fail.

## The budget

Every stage, with where the number comes from.

| Stage | Typical | Notes |
|---|---|---|
| Phone capture | 5–10 ms | Oboe at `LowLatency` + 2 bursts of 48 kHz |
| Encode | 1–2 ms | Opus, 10 ms frame. Zero in M0 (raw PCM) |
| Network | 2–15 ms | Wi-Fi LAN. USB/ADB is 1–2 ms |
| Jitter buffer | 20–40 ms | The dial you actually tune |
| Decode + PLC | ~1 ms | Zero in M0 |
| Drift resampler | ~2 ms | Zero in M0 when rates already match |
| Output endpoint | 10–20 ms | WASAPI shared mode, plus the cable's own buffer |
| **Total** | **~65 ms** | |

Two of those are not really under our control, so they are worth stating plainly:

* **The cable's internal buffer.** VB-CABLE adds latency of its own, tunable in
  its control panel. We cannot see it and cannot measure it from inside the
  stream; it shows up as a fixed offset.
* **WASAPI shared mode.** Microsoft's own documentation puts the audio engine at
  **1.3 ms** in Windows 10 and later, and the default buffer at **10 ms** unless
  the app opts into smaller buffers via `IAudioClient3`. M0 uses cpal, so it takes
  the default. M1 hand-writes the WASAPI path to claim the smaller buffer.

## Method 1 — true end-to-end (recommended)

This measures everything, including phone capture and the acoustic path, which is
the number a user actually experiences.

You need one app that can capture two devices at once. On Windows that is **OBS**
(Audacity can only record one device per instance, which makes it useless here).

1. Install and run VB-CABLE, then `talkee probe` to confirm Talkee can see it.
2. Run `talkee host`.
3. In OBS, add **two** sources:
   * Audio Input Capture → `CABLE Output (VB-Audio Virtual Cable)`
   * Audio Input Capture → your real microphone
4. In OBS → Settings → Output → Recording, enable **Audio Track 2** and
   **Audio Track 3**, and assign each source its own track in
   Advanced Audio Properties.
5. Start recording. **Clap once, sharply.** Stop recording.
6. Open the recording in Audacity (or any waveform editor), view the two tracks,
   and measure the distance between the two clap transients.

The offset is your end-to-end latency. Do it three times; the *variation* between
attempts tells you how much jitter the pipeline adds, which matters more than the
absolute figure for voice chat.

## Method 2 — pipeline stability without OBS

This cannot give you absolute latency, but it does expose drift, dropouts and
jitter in the part we control.

```powershell
# terminal 1
talkee host

# terminal 2
talkee send --click --click-interval-ms 2000 --seconds 300
```

Record `CABLE Output` in Audacity for the same 300 seconds. The clicks should be
**exactly** 2000 ms apart. What deviations mean:

| Symptom | Meaning |
|---|---|
| Clicks drift steadily apart | Clock drift the resampler is not correcting |
| Occasional doubled or missing click | Packet loss, or the jitter buffer starving |
| Clicks compressed/stretched then snapping back | The queue hit capacity and dropped oldest samples (`slip`) |
| Irregular spacing throughout | Scheduling jitter — check CPU load and power plan |

## Reading the health line

`talkee host` prints one line every few seconds:

```
recv       0 dgram |     -- dgram/s |      -- frames/s |   -- frames/dgram | loss   0.00% (0) | late     0 | slip      0 | trim      0 | under     0 | busy    0 | queue    0.0 ms | ctrl       -- ppm | drift       -- ppm
recv     202 dgram |  100.4 dgram/s |   48169 frames/s |  480 frames/dgram | loss   0.00% (0) | late     0 | slip      0 | trim      0 | under     0 | busy    0 | queue   67.9 ms | ctrl  +2449.6 ppm | drift  +3523.9 ppm
recv    1708 dgram |  100.3 dgram/s |   48145 frames/s |  480 frames/dgram | loss   0.00% (0) | late     0 | slip      0 | trim      0 | under     0 | busy    0 | queue   92.2 ms | ctrl  -3490.1 ppm | drift  +3029.0 ppm
```

| Field | What it is | Worry when |
|---|---|---|
| `dgram/s` | Datagrams that arrived, per second, over the last window | Below the sender's frame rate |
| `frames/s` | Mono frames that arrived, per second, over the last window | Below the rate the sender claims |
| `frames/dgram` | Mean frames per datagram | Below the sender's frame size |
| `loss` | Frames inferred missing from gaps in `seq` | Above ~0.5% |
| `late` | Duplicate or reordered datagrams, discarded | Rising alongside `loss` |
| `slip` | Samples thrown away because the queue reached its **capacity** | **Anything above 0** — this is the drift loop losing |
| `trim` | Samples the loop *chose* to shed, to bring a burst back to the target depth | Steady growth means the mismatch is past the ±1% the loop can absorb |
| `under` | Callbacks that ran short and padded with silence | Anything above a handful **while audio is flowing** |
| `busy` | Callbacks that could not take the queue lock and emitted silence | Should be 0; nonzero means lock contention on the audio thread |
| `queue` | Current output buffer depth | Should hold near `--buffer-ms`, not climb |
| `gain` | How hard the automatic level stage is working, as a multiplier | Pinned at 32× means the sender is very quiet; `--` means the stage is off |
| `ctrl` | What the loop is doing about `drift`: the ratio correction, in ppm | Pinned at ±10 000 ppm means it has run out of authority |
| `drift` | `frames/s` versus the rate the sender claims, in ppm | A sustained departure from zero while the three rate columns look healthy |

A rate column prints `--` when nothing arrived in the window. That is deliberate:
a confident `0 ppm` before the sender starts reads like a healthy stream.

### Read the three rate columns together

They separate failures that used to be indistinguishable, because all three lower
the same average:

| `dgram/s` | `frames/s` | `frames/dgram` | Reading | Fix |
|---|---|---|---|---|
| nominal | low | nominal | Frames are being lost, or the sender stalls between bursts | The sender's scheduling |
| low | low | nominal | The same failure seen from the datagram side | The sender's scheduling |
| nominal | low | low | The sender is emitting short frames | The capture format |
| nominal | nominal | nominal | Healthy *now* — `slip` and `under` totals are historical | none |

Measured on this machine, changing only the sender's frame shape moves exactly the
columns it should and leaves the clock column alone:

```
recv     411 dgram |  100.0 dgram/s |   48015 frames/s |  480 frames/dgram | ... | drift   +314.6 ppm   <- 10 ms frames
recv     605 dgram |  200.0 dgram/s |   48012 frames/s |  240 frames/dgram | ... | drift   +244.2 ppm   <- 5 ms frames
```

### About `drift` — the two numbers measure *different* mismatches

Two devices are never at exactly the same rate. The mismatch is a few hundred
parts per million, which sounds negligible until you do the arithmetic: 200 ppm
over one hour is **0.72 seconds** of accumulated audio. Uncorrected, that appears
as a periodic click followed by a growing delay into the hundreds of
milliseconds.

There are two ways to see a rate mismatch, and they are **not the same mismatch**:

| Number | Compares | Converges to |
|---|---|---|
| `drift` | Frames received per second vs **wall clock**, over the current window | sender's clock vs *this machine's* clock |
| `queue` trend | Frames received vs frames the **device consumed** | sender's clock vs the *output device's* clock |

This distinction is easy to miss and changes a design decision, so here is the
measurement that forced it. In a 95-second soak the sender and receiver shared a
clock — `talkee send` paces itself on `Instant`, so its rate is exactly nominal by
construction — and `drift` duly converged toward zero. **`queue` still climbed
from 60 ms to 92 ms**, which is ~344 ppm of rate error. If we had only read
`drift`, we would have concluded the clocks agreed. They did not: that 344 ppm is
**VB-CABLE's own output clock**, and it is invisible to a wall-clock measurement.

**Design consequence for M1:** the drift controller must target **queue depth**,
not the wall-clock rate. Queue depth is the only signal that sees the whole chain.

#### Why `drift` is a window, not a session

`drift` used to be frames received divided by wall-clock time **since the first
frame of the session**, and that made it unusable as a health signal. The
measurement that settled it: in the first real-phone run the last two and a half
minutes delivered a clean **100 dgram/s**, and `drift` still read **-137 748 ppm**.
An average over a session cannot forget the stalls at the start of it, so a run
that had recovered still looked broken — and because a slow clock, lost frames and
a stalling sender all lower the same number, the figure could not say which had
happened.

So `drift` now compares this window's `frames/s` against the rate the sender
claims. The window's clock starts at its **first datagram** and that datagram's
frames are excluded from the count, so the interval and the samples describe the
same span. What is left is LAN jitter, of the order of **±1 %**.

That is a deliberate trade. ±1 % catches a 12 % shortfall and is useless for a
200 ppm crystal difference; for small numbers read the `queue` trend over minutes,
which by construction sees the whole chain, including the cable's own clock.

The old cumulative figure also had a start-up artefact worth remembering, because
it is why an early M0 build reported **+17 800 ppm** on an 8-second window. The
uncertainty was the gap between when the first frame's *audio* was captured and
when the clock started — about one frame plus encode and network — measured in the
soak at **38 ms**, giving `ppm_measured ≈ 38 270 / t`: 2 902 ppm at 13 s, 556 ppm
at 68 s, 411 ppm at 93 s, with `ppm × t` constant at 38 270 ± 300 throughout.
Starting the window at the first datagram is what removes that term, rather than
hiding it behind a ten-second minimum.

#### Reading the queue trend

A single `queue` reading resolves to about one audio callback — measured here at
**10 ms**, which is Windows' default WASAPI shared-mode buffer and corroborates
the latency budget above. So a reading is only good to ±10 ms, and you need many
seconds of trend to separate a real 344 ppm from quantisation noise.

**What M1 does about drift:** closes the loop. `DepthController` reads the queue depth
every 200 ms and corrects the resampler's ratio by up to **±1 %**, so the output device
drains the queue at exactly the rate the sender fills it. A clock difference becomes a
fractional stretch of the audio — a few hundred ppm is about a hundredth of a semitone,
and only while the error is being drained. Bursts, which are *not* clock differences,
are shed whole frames at a time down to the target instead.

Measured on this machine against VB-CABLE, using `talkee send`, which can be paced off
nominal on purpose (`--pace-ppm`) precisely so this is testable:

| Sender pacing | Loop | `queue` | `slip` | `trim` | `ctrl` |
|---|---|---|---|---|---|
| +3 000 ppm | off | 60 → 110 ms, still climbing | 0 | 0 | `--` |
| +3 000 ppm | on | 68 → 92 ms, then draining back | **0** | 0 | **−3 490 ppm** |
| +20 000 ppm | off | 100 → **240 ms (capacity)**, pinned | **9 120 and climbing** | 0 | `--` |
| +20 000 ppm | on | 107 → 90 ms, bounded | **0** | 15 232 | −922 ppm |

The third row is the M0 pathology reproduced on demand: the queue pins at its capacity
and throws the oldest audio away for as long as the mismatch lasts. That is the shape of
the 37-minute phone run in `run.host.log` (`slip 968736`, `queue 230 ms`). The fourth row
is the same input with one flag changed: the loop cannot absorb 2 % because its authority
is 1 %, so it sheds deliberately instead — `trim` counts what it shed, and `slip` stays
at zero because the queue never reaches capacity.

Three limits, all measured rather than theoretical:

* **The loop is slow, on purpose: ~60 s to converge.** It has to be. The depth is only
  knowable to one frame — 10 ms — so a loop fast enough to settle in two seconds is a
  loop chasing its own quantisation. An earlier attempt at exactly that produced a limit
  cycle that never decayed, and the whole story is in `controller.rs`.
* **It overshoots by ~30 ms before settling.** That is what the 50 ms high-water margin
  is sized to tolerate without shedding, and it is why shedding must never be the
  primary mechanism.
* **`ctrl` has ±1 % of authority.** Past that the mismatch is not a clock difference, and
  shedding is the honest response — bounded, but audible.

### Two traps that cost us a round of debugging

Both of these produced alarming-looking numbers that were artefacts, and both are
worth knowing about before you panic at the health line:

* **`under` counts up after the sender stops.** With nothing arriving, every
  callback runs short — correctly. A run showing `under 209` was mostly the five
  seconds *after* `talkee send` exited. Judge `under` only while audio is flowing.
* **`drift` reads `--` or wildly when nothing is flowing.** A window in which the
  sender was silent, or in which it restarted, has no rate to report. Judge
  `drift` only over windows where `frames/s` is at the sender's claimed rate.

### Why `--buffer-ms` matters more than you would think

Without prefill, a producer and consumer running at the same average rate leave the
queue permanently near empty, so *any* scheduling delay starves the callback.
Measured here at 0 prefill: **65% of callbacks padded with silence** — an audible
mess that still reported `loss 0.00%`. Setting `--buffer-ms` (default 60) makes
audio wait until that much has accumulated, which took underruns to **0**. This is
the latency-versus-reliability dial, and there is no setting that gives you both.

## Measured results (M0, on this machine)

A 95-second soak, Windows 11, VB-CABLE, `talkee send` standing in for the phone,
release build, `--buffer-ms 60`:

| Metric | Result |
|---|---|
| Datagrams received | 9500 of 9500 |
| `loss` | **0.00%** |
| `late` | **0** |
| `slip` | **0** |
| `under` **while audio flowed** | **0** |
| `busy` | **0** |
| Sender pacing | 9500 frames in 95 s = **exactly 100.0 fps** |
| Wire bitrate | 1555 kbps raw PCM |
| Endpoint format negotiated | 48000 Hz, 2 ch, F32 |
| Callback period | ~10 ms (= Windows' default WASAPI shared-mode buffer) |
| Device rate error, from the queue trend | ~344 ppm |
| Start-of-stream offset, from `drift × t` | ~38 ms |

Not a single dropped frame, starvation event or lock contention in 95 seconds of
continuous streaming. **The architecture is sound**; what remained was Opus and real
phone capture, and both are measured below — along with the drift loop that M0 left as
a placeholder.

### Measured with the real phone (M0.3)

The same instrument, with the Android app streaming to `talkee host` over Wi-Fi
(release host, `--buffer-ms 60`):

| Metric | Result |
|---|---|
| Duration | 110 s continuous |
| Datagrams received | 10 102 |
| `dgram/s` | **99.7 - 100.0** |
| `frames/s` | **47 840 - 48 442**, every window within ±0.9% of nominal |
| `frames/dgram` | **exactly 480** in every window |
| `loss` | **0.03%** (3 frames) |
| `late` | **0** |
| `slip` | **0** |
| `under` | **11** in 110 s: 7 in the opening 20 s, then 4 in the last 10 s |
| `busy` | **0** |
| `queue` | Climbed to 110-140 ms within 30 s, then wandered between 20 and 150 ms |
| `drift` | Wandered ±9000 ppm window to window, settling to +17 ppm |

A real phone delivers full 10 ms frames at the nominal rate with a clock within a
fraction of a percent. The Android capture path is **not** the bottleneck it was
assumed to be.

Two honest qualifications:

* `queue` climbed to ~130 ms instead of holding the 60 ms target, then wandered
  between 20 ms and 150 ms for the rest of the run — including one dip to 20 ms that
  came within a callback of starving the audio thread. That is the M0 "drop the
  oldest" strategy doing its job badly: a burst of early frames raised the depth, and
  nothing ever brings it back down. It is ~70 ms of user-visible latency and a
  standing risk of dropouts, it is exactly what M1's jitter buffer is for, and
  **Opus does not fix it**.
* 110 seconds is not the ten-minute soak in the acceptance criteria below.

### A failure that did not reproduce, and why that matters

An earlier 37-minute run of the same app against a **debug** host printed
`slip 968736`, `under 31759`, `busy 105` and `drift -137 748 ppm`. That log is what
motivated the rate columns in the first place.

Repeating it — same APK, same phone, same network, and the same *old* debug host
binary — produced `slip 0`, `under 5`, `busy 7`. **The failure did not reproduce**,
so it was environmental rather than architectural; the suspects are how the app was
launched that time (under a Flutter debug harness) and the phone's power state, both
of which can starve the capture loop. Debug is not free, but it is not sufficient
either: `busy` ran 5-7 against release's 0, and the queue sat 30-50 ms deeper.

The lesson is not "it was fine all along". It is that the old health line could not
say *why* a run looked bad, so a real defect, a debug build and a throttled phone
were indistinguishable — and the first hypothesis it suggested (a 44.1 kHz capture
path) was wrong. `frames/dgram` is what rules that out.

### The one thing that is still unmeasured

Everything above is the *pipeline*. The acoustic round trip — the clap test in
Method 1 — has **not** been measured, and it is the only number a user actually
feels. It needs OBS capturing two devices at once. Until it appears here, the
latency budget above is still a plan.

## M0 acceptance criteria

| Metric | Pass |
|---|---|
| `loss` on a wired LAN, 10 minutes | < 0.1% |
| `busy`, and `under` while audio flows | 0 over 10 minutes |
| `queue` | Holds near `--buffer-ms` and stays flat, not climbing |
| `slip` | **0** — a nonzero `slip` means the drift loop lost. `trim` may be nonzero; that is the loop working |
| `drift` per window | A few hundred ppm or better across many windows, not wandering |
| End-to-end clap test | Measured and written down, even if disappointing |

Soak for **at least five minutes**. Ten seconds is not a soak. `drift` is now
meaningful within a single window, but `loss`, `slip` and `under` are totals, and a
total needs time behind it before it can tell you anything.

If Method 1 works at all and the clap test is under ~150 ms, the architecture is
sound and M1 begins. If it exceeds ~400 ms, the problem is almost certainly the
cable's internal buffer or WASAPI's default buffer, not the network — check both
before touching any code.

