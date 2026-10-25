# Talkee wire protocol

This document is the contract. It is written down **once**, here, and implemented
twice — currently in Rust on both sides, later in Dart/Kotlin for the phone. The
alternative is discovering that two implementations disagree, in the field, in the
shape of audio that sounds fine but clips.

The Rust types live in `rust/talkee-proto`. Anything stated here that the code does
not do is a bug in the code. The cryptographic construction is stated in full in
`rust/talkee-crypto`, including known-answer vectors, so a second implementation can
be checked rather than trusted.

## What is being protected

A phone microphone on a desktop is a device anyone else on the same network would
like to write to, because Zoom, Discord and OBS read it as a live microphone. Three
properties are therefore part of the protocol rather than of any one implementation:

* **Only the holder of the pairing key can make the desktop hear anything.** A host
  in its default mode drops every audio datagram that is not authenticated, before
  it looks at the audio at all.
* **The key never crosses the wire.** It is transferred once, out of band, in a QR
  code, and afterwards only proved.
* **A recorded datagram is worthless.** Payloads are AEAD-sealed and a frame whose
  sequence number is not ahead of the highest already authenticated is discarded.

## Versioning

`PROTOCOL_VERSION` is a single byte in every audio header, and is exchanged in
`HELLO`. It is bumped for any change that is not backwards compatible. A peer that
receives a version it does not speak **drops the datagram**; it does not attempt a
best-effort parse. The failure mode of guessing is garbage audio, which is far
harder to diagnose than silence.

It is still `1`: the Opus tag (`format = 2`) and the `ENCRYPTED` flag were added
without a bump because an older peer already rejects both through rules it has — an
unknown sample format tag is refused, and an encrypted frame's `payload_len` cannot
equal the plaintext length its other fields imply. Compatibility was preserved by
the existing rules, and inventing a new version would have cut the phone app off for
no gain.

Unknown *flag bits* must be ignored. Unknown *sample format tags* must not be.

## Transport

| Channel | Protocol | Port | Notes |
|---|---|---|---|
| Audio | UDP | 47801 | One frame per datagram. No retransmission |
| Control | TCP | 47800 | Newline-delimited JSON. Carries the pairing handshake |
| Discovery | mDNS | — | `_talkee._tcp.local.` SRV port = control, TXT `ap` = audio |

UDP for audio because a late packet is worthless. An audio frame that arrives after
its deadline must be discarded, not queued — retransmitting it only adds latency to
the frames behind it. This is why the design carries `seq` and infers loss rather
than leaning on TCP.

Discovery is **discovery, not authorisation**. An mDNS advertisement says a host
exists so a user does not have to type an address; it grants nothing. A host may be
advertised and still refuse every datagram until a pairing verifies.
## Audio datagram

A fixed **24-byte** header, little-endian throughout, followed by the payload.
The header size is fixed so that the payload encoding can change without changing
the framing — which is exactly what happened when Opus and encryption arrived.

| Offset | Size | Field | Type | Meaning |
|---|---|---|---|---|
| 0 | 2 | `magic` | u16 le | Always `0x4B54`, which renders on the wire as ASCII `TK` |
| 2 | 1 | `version` | u8 | `PROTOCOL_VERSION` |
| 3 | 1 | `flags` | u8 | Bitfield, below |
| 4 | 4 | `seq` | u32 le | Monotonic per sender, wraps |
| 8 | 4 | `ts_ms` | u32 le | Sender's monotonic clock, milliseconds |
| 12 | 4 | `sample_rate` | u32 le | Hz |
| 16 | 2 | `samples` | u16 le | Frames **per channel** in this payload |
| 18 | 1 | `channels` | u8 | Interleaved channel count, must be non-zero |
| 19 | 1 | `format` | u8 | `0` = i16 le, `1` = f32 le, `2` = Opus |
| 20 | 4 | `payload_len` | u32 le | Bytes following the header |

`payload_len` counts the bytes **after** the header, not the plaintext. When
`ENCRYPTED` is set those bytes are `[12-byte nonce][ciphertext][16-byte tag]`, so the
declared length is the plaintext length plus 28, and the plaintext length is what
`samples`, `channels` and `format` imply. Both ends derive the same number from the
same fields, which is what turns a lying `payload_len` into a rejected datagram
rather than an out-of-bounds read.

### Validation rules

A receiver **must** reject a datagram when:

* it is shorter than 24 bytes;
* `magic` is wrong — this is how we ignore unrelated traffic on the port;
* `version` is not ours;
* `channels` or `sample_rate` is zero;
* `format` is a tag we do not know;
* `channels` exceeds 2 for Opus — this protocol carries mono or stereo audio;
* `payload_len` exceeds `MAX_PAYLOAD_LEN` (8 KiB);
* for a PCM format, `payload_len` is not exactly
  `samples × channels × bytes_per_sample` (plus 28 when `ENCRYPTED` is set);
* for Opus, `payload_len` is zero, or (when `ENCRYPTED` is set) smaller than 29 —
  a nonce, a tag and at least one byte of packet.

A payload that is *shorter* than `samples` claims is not rejected: it is decoded
as far as the bytes go and then stops. A truncated datagram should degrade, not
crash the stream.

### Flag bits

| Bit | Name | Meaning |
|---|---|---|
| 0 | `FEC` | Payload carries Opus in-band FEC |
| 1 | `DTX` | Encoder was in discontinuous transmission |
| 2 | `SILENCE` | Sender deliberately sent this as no-voice |
| 3 | `REDUNDANT` | Duplicate of an earlier frame, for loss resilience |
| 4 | `ENCRYPTED` | Payload is `[12-byte nonce][ciphertext][16-byte tag]`, AES-256-GCM |
### Sequence numbers and loss

`seq` increments by one per frame. Receivers infer loss from gaps rather than
being told:

| Condition | Interpretation | Action |
|---|---|---|
| First datagram seen | Session start | Establish baseline |
| `seq == expected` | On time | Accept |
| `expected < seq ≤ expected + 1000` | Lost `seq - expected` frames | Accept, bill the loss |
| `seq > expected + 1000` | Sender restarted | Resync, clear the buffer, **do not** bill 1000+ as loss |
| `seq < expected` | Duplicate or reordered | Discard, count as `late` |

The 1000-frame threshold (10 seconds at 10 ms frames) exists because a
reconnection would otherwise poison the loss statistic with a nonsense number and
make the health line useless.

All comparisons use **wrapping** arithmetic, so a sender that runs long enough to
wrap from `u32::MAX` to `0` is seen as on time rather than as a four-billion-frame
gap.

The replay rule for encrypted audio is **stricter**: see
[M2 payload: sealed audio](#m2-payload-sealed-audio). It is not a window, and it
does not use `expected`.

### M0 payload: raw PCM, `format = 0` and `format = 1`

Raw interleaved PCM. At 48 kHz mono f32 that is **~1.6 Mbps**, which is fine on a
LAN and indefensible over anything else. That number is the entire argument for the
Opus tag.

Multi-channel payloads are averaged down to mono on receipt. Averaging rather than
taking channel 0 is deliberate: a stereo stream with one dead channel still
produces usable audio.

### M1 payload: Opus, `format = 2`

One Opus packet per datagram, encoded by the sender at the requested sample rate
(48 kHz), channels (1) and frame duration (10 ms), targeting 64 kbps.

* `samples` is the number of frames per channel the packet **decodes to** — 480 at
  48 kHz for a 10 ms frame. A receiver knows how much audio to expect before it
  decodes anything, which is what lets a decoder size its output buffer once.
* `payload_len` is the length of the packet. It cannot be derived from `samples`,
  because Opus is variable rate, so it is carried in the header and range-checked
  instead.
* An empty payload is invalid. Packet loss concealment is signalled by the *absence*
  of a datagram, never by an empty packet: a zero-length packet is a bug, and
  silently treating it as a lost frame would hide that bug.
* `FEC` means the packet carries in-band forward error correction for the previous
  frame, `DTX` means the encoder was in discontinuous transmission and the packet is
  a comfort-noise marker, and `REDUNDANT` means this is a duplicate of an earlier
  frame sent to survive loss. None of the three changes how a packet is parsed.
### M2 payload: sealed audio

With `ENCRYPTED` set the payload is `[12-byte nonce][ciphertext][16-byte tag]`:
AES-256-GCM, authenticated with the 24-byte header as associated data, encrypted
with the session key from the pairing handshake. Nothing about the audio is in the
clear, and **nothing about the header is malleable**: sample rate, channel count,
format, flags, `seq` and the length itself are all authenticated, so a frame cannot
be relabelled in flight.

#### The nonce

```text
byte  0      direction: 0 = phone -> desktop, 1 = desktop -> phone
bytes 1..5   seq, little-endian u32
bytes 5..12  zero
```

Twelve bytes is the size GCM is fastest and best analysed at. A GCM nonce needs
uniqueness, not unpredictability, and under a session key the pair
`(direction, seq)` is unique by construction.

#### The session key

| Value | Construction |
|---|---|
| session id | 4 random bytes, chosen by the **sender**, sent in `PAIR` as 8 hex characters |
| session key | `HMAC-SHA256(psk, "talkee-session-v1" ‖ session_id)` |

There is a per-session key because "the nonce is just `seq`" is only safe if `seq`
never repeats under one key, and it does repeat: a phone that restarts begins again
at zero, while a stored pairing key does not change. Reusing a GCM nonce under one
key leaks the XOR of two plaintexts *and* allows tag forgery, so the session key is
what makes the cheap nonce legitimate. The domain separator keeps this HMAC from
ever being interchangeable with the pairing proof.

#### The replay rule

A receiver **must** drop a frame whose `seq` is not greater than the highest it has
already authenticated (wrapping comparison, for a session that runs for months).

This is not a replay *window*. A window would have to be sized, tested and justified,
and the reason it is unnecessary is in the product: audio that arrives after a newer
frame has already been played is worthless, so a reordered frame is dropped anyway.
The rule has three consequences worth stating plainly:

* the high-water mark advances **only after the tag verifies**, so a forged frame
  cannot push it forward and silence the real sender;
* a new `PAIR` installs a new session with a fresh key and a reset high-water mark,
  which is how a phone that reconnected at `seq = 0` is heard at all;
* the receiver applies the rule before it does any cryptography *except* that it
  must authenticate first if the nonce does not match the one it would have derived —
  there is only one nonce per `(direction, seq)`, so a mismatch is refused outright.

Sealed frames are accepted from any source address. The address is not the access
control — the tag is — and pinning it would break a phone that roams between Wi-Fi
and its own hotspot.

#### Known-answer vector

For `psk = 00 01 .. 1f`, `nonce = 00 01 .. 1f` and `session_id = 01 02 03 04`:

```text
proof       = 8cd6e1b4ba154c4521f0de5d302208a1f10478bf708bcae98038aeb5b1c60593
session key = acbcb7a70f5e3ba04af029614c9d5959ab97d297fc807264dc30e6f4d3b9af49
```

The session key is never sent; the vector is published so that a second
implementation can prove it derives the same key rather than assuming it does.
## Control channel (M2)

Newline-delimited JSON over TCP, port 47800. `type` is the discriminant and matches
the enum in `talkee-proto::ControlMsg`. One JSON object per line, `\n` terminated,
UTF-8, and **at most 4096 bytes** — a peer that sends bytes without ever sending a
newline is refused rather than buffered, because the alternative is unbounded memory
growth on a connection we are holding open on purpose.

```json
{"type":"HELLO","ver":1,"dev_id":"...","name":"Pixel 8","caps":["aes-gcm","opus"]}
{"type":"CHALLENGE","nonce":"<32 random bytes, hex>"}
{"type":"PAIR","proof":"<HMAC-SHA256(psk, nonce ‖ session), hex>","session":"<4 random bytes, hex>"}
{"type":"CONFIG","sample_rate":48000,"channels":1,"format":2,"frame_ms":10,
 "bitrate":64000,"fec":true,"source":"unprocessed"}
{"type":"REJECTED","reason":"the pairing proof did not verify against this host key"}
{"type":"START"}
{"type":"STOP"}
{"type":"PING","ts":12345}
{"type":"STATS","frames_sent":41230,"frames_dropped":0,"battery_pct":78}
```

An unknown `type` is a hard error, not a silent skip: a newer peer speaking a
message we do not understand is a version mismatch, and pretending otherwise
would let the two sides diverge silently.

`REJECTED` exists so that a wrong key reads as a refusal rather than a timeout. It
is sent before the connection closes, and its `reason` never contains key material.

`START` and `STOP` control the phone's capture loop; they do not gate the desktop,
which accepts any sealed frame whose sequence number is ahead of what it has seen.
`PING` is echoed with the same `ts`. A host that itself sends `PING` must not also
answer its own echo, or two peers will ping each other until one of them is closed —
the reference implementation ignores a `PING` whose `ts` it has just echoed.

## Pairing (M2)

The desktop generates a 32-byte key from the OS random number generator and shows it
once as a QR code / deep link:

```
talkee://pair?v=1&h=192.168.1.42&p=47800&fpid=66687aad&name=DESKTOP&k=<base64url>
```

| Parameter | Required | Meaning |
|---|---|---|
| `v` | yes | Link format version. `1`. Independent of `PROTOCOL_VERSION`: this is an artefact handed between apps, not a wire format |
| `h` | yes | Host address or DNS name: digits, letters, `.`, `-`, `:`, `[`, `]`, `_` |
| `p` | yes | Control port. Audio is `p + 1` by convention; a sender with an explicit `--to` overrides it |
| `fpid` | yes | The host's fingerprint: 8 to 64 hex characters. The reference host publishes the first 4 bytes of `SHA-256(key)` |
| `name` | yes | Human-readable host name, 1-64 bytes, percent-encoded, printable ASCII only |
| `k` | yes | The 32-byte key, base64url, RFC 4648 §5, unpadded |

Strictness, and why:

* a **repeated** parameter is an error, never "last one wins". Two parsers
disagreeing about which `k` is real is a pairing failure that presents as a wrong
key, and an attacker who can smuggle one past a lenient parser wins;
* `k` must decode to exactly 32 bytes, and must be **canonical** base64url — a
final symbol with non-zero unused bits is rejected, because two encodings of the
same key is a fingerprint-comparison bug waiting to happen;
* `name` is restricted to printable ASCII so that a hostile link cannot inject
terminal escape sequences into the console or the UI that displays it;
* unknown parameters are ignored, so a newer host can add fields without breaking an
older phone;
* nothing in the parser may panic: a link arrives from a QR code that somebody else
may have printed.
### The handshake, in order

```text
phone                                     desktop
  |  TCP connect to h:p                     |
  |  HELLO{ver, dev_id, name, caps}  -----> |
  |  <-----  CHALLENGE{nonce}                |  32 fresh random bytes
  |  PAIR{proof, session}  ----------------> |
  |               (verify)                   |
  |  <-----  CONFIG{...}                     |  accepted: the session is installed
  |  <-----  REJECTED{reason}                |  refused: the connection closes
  |  audio datagrams (sealed)  ----------->  |
```

* The **receiver** picks the challenge, for every connection. A peer that supplied
  its own nonce could answer one it had already seen answered.
* The **sender** picks the session id, at random, for every stream. It is the one
  that must promise never to reuse a nonce, and it can only promise that about a
  value it owns.
* The proof is `HMAC-SHA256(psk, "talkee-pair-v1" ‖ nonce ‖ session_id)`, over the
  challenge's **raw 32 bytes**, not over its hex text. Two implementations
  disagreeing about that is how a phone and a desktop end up unable to pair for no
  visible reason, which is why the known-answer vector above exists.
* The session is installed only after the proof verifies. Before that, a peer has no
  influence on the audio path at all: an unauthenticated datagram is counted and
  dropped, and cannot even reach the jitter buffer.
* The session lives as long as the control connection. Closing it — including the
  phone's process exiting — drops the session, so a phone that goes away stops being
  able to inject frames rather than merely stopping of its own accord. A phone that
  reconnects pairs again, with a new session id.

### Key handling

* `k` is displayed once, in the deep link, and nowhere else. The reference host draws
  a fresh key for every run and does not persist it: a process that never writes the
  key to disk is a process that cannot leak it from disk. Persisting a pairing, so a
  phone can reconnect without scanning a new code, is the platform keystore's job —
  Windows DPAPI on the desktop, Android Keystore on the phone — and never a plain
  config file.
* Anything that prints a key prints a redacted form. Both the `Debug` and `Display`
  of the key type in `talkee-crypto` print the fingerprint and not the key, and
  nothing in the pairing path echoes an input that contains one.
* `fpid` is a short, non-public-in-meaning digest of the key (4 bytes of SHA-256) so
  a phone can recognise a host it has paired with before. It is published in the
  link, where it costs an attacker 2^32 work per candidate key to invert — and a
  candidate that matches is a key they would have had to guess outright.

### `--secure`: how the desktop is told to accept nothing unpaired

The host accepts plaintext audio **by default**: no key is generated, no link is
printed, and the startup banner says so in plain words. The default is the path that
works, and that is a deliberate, temporary trade rather than a finished decision. The
phone app cannot encrypt yet, so a host that demanded a pairing it can never receive
would present a working-looking screen attached to a dead microphone - and a user has
no way to tell that apart from "my phone is broken".

Passing `--secure` (or `--key`) changes it: the host generates or loads a key, opens
the pairing channel, prints the deep link, and refuses any datagram that is not sealed
with a proof of that key. The phone must pair before a single frame is played.

Both defaults have a failure mode, which is why the startup banner states which one is
in force:

| Mode | Failure mode |
|---|---|
| plaintext (default) | A stranger on a network you do not control can speak through your microphone |
| `--secure` | The phone cannot pair yet, so the microphone stays silent |

**This default flips to `--secure` as soon as the phone side can pair.** It exists in
this shape only so that the working path keeps working while that lands.

## What changes later

| Milestone | Change |
|---|---|
| M1 | `format = 2` (Opus); `FEC`, `DTX` and `REDUNDANT` become meaningful; effective payload drops to ~200 bytes |
| M2 | `ENCRYPTED` comes into use; control channel and pairing go live |
| M4 | USB transport (TCP over `adb reverse`, or over a tethered interface) reuses identical framing — only the socket changes |

## What is deliberately *not* here

* **No session resumption without a handshake.** A pairing is a fresh challenge and
a fresh session id every time. There is no ticket to steal, and a stored key does
not by itself let a recorder inject frames.
* **No replay window.** Sealed audio is accepted strictly forward. A frame that has
been overtaken is worthless, so the simpler rule is also the better one.
* **No key derivation from user input.** The link carries 256 bits of OS entropy or
nothing.
* **No plaintext fallback in the phone path.** A client that cannot pair must report
that, not degrade into M0.
* **No forward secrecy.** The key is a long-lived shared secret, so a device that is
compromised *and* was recording earlier traffic can decrypt it. Closing that would
need a Diffie-Hellman exchange, which needs an authenticated channel to run over —
the deep link is that channel, and building an ephemeral exchange on top of it is a
larger piece of work than this milestone. It is written down here so nobody assumes
it was overlooked.
