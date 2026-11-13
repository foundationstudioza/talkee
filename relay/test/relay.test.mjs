import assert from "node:assert/strict";
import { test } from "node:test";

import { accept, singleLine } from "../lib/cleanup.mjs";
import { DEVICE_SECONDS_PER_DAY, handle, parseOptions } from "../lib/dictate.mjs";
import { oggOpusSeconds } from "../lib/ogg.mjs";
import { UpstreamError } from "../lib/providers.mjs";

/** An Ogg page with one packet (no checksum: the relay does not check it). */
function page(packet, granule, flags = 0, sequence = 0) {
  const lacing = [];
  for (let n = packet.length; ; n -= 255) {
    lacing.push(Math.min(n, 255));
    if (n < 255) break;
  }
  const header = new Uint8Array(27 + lacing.length);
  header.set([0x4f, 0x67, 0x67, 0x53], 0); // OggS
  header[5] = flags;
  new DataView(header.buffer).setBigUint64(6, BigInt(granule), true);
  new DataView(header.buffer).setUint32(18, sequence, true);
  header[26] = lacing.length;
  header.set(lacing, 27);
  return Buffer.concat([header, packet]);
}

/** A minimal Ogg Opus file `seconds` long, with a 312-sample pre-skip at 16 kHz (936 at 48 kHz). */
function ogg(seconds) {
  const head = Buffer.alloc(19);
  head.write("OpusHead", 0);
  head[8] = 1;
  head[9] = 1;
  head.writeUInt16LE(936, 10);
  head.writeUInt32LE(16000, 12);
  const tags = Buffer.from("OpusTags\u0006\u0000\u0000\u0000talkee\u0000\u0000\u0000\u0000", "latin1");
  return Buffer.concat([
    page(head, 0, 2, 0),
    page(tags, 0, 0, 1),
    page(Buffer.alloc(40, 1), Math.round(936 + seconds * 48000), 4, 2),
  ]);
}

test("reads an Ogg Opus file's length from its last page", () => {
  assert.equal(oggOpusSeconds(ogg(3)), 3);
  assert.equal(oggOpusSeconds(ogg(0.5)), 0.5);
});

test("refuses what is not Ogg Opus, or is cut short", () => {
  assert.equal(oggOpusSeconds(Buffer.from("RIFF....WAVEfmt ")), null);
  const file = ogg(2);
  assert.equal(oggOpusSeconds(file.subarray(0, file.length - 5)), null);
});

test("cleanup output is one line and answers are refused", () => {
  assert.equal(singleLine("a:\n1. b\r\n2. c\n"), "a: 1. b 2. c");
  assert.equal(accept("what is the capital of france", "What is the capital of France?"), "What is the capital of France?");
  assert.equal(
    accept("what is the capital of france", "The capital of France is Paris. It has been the capital since the 10th century and is home to the Eiffel Tower."),
    null,
  );
});

test("options are read defensively", () => {
  const header = Buffer.from(JSON.stringify({ vocabulary: ["Compaq", 3, " Talkee "], language: "en", cleanup: false })).toString("base64url");
  assert.deepEqual(parseOptions(header), { vocabulary: ["Compaq", "Talkee"], language: "en", cleanup: false, instructions: "" });
  assert.deepEqual(parseOptions("not base64 json"), {});
  assert.equal(parseOptions(Buffer.from('{"language":"en; rm -rf"}').toString("base64url")).language, undefined);
});

function memoryUsage() {
  const map = new Map();
  return { map, get: async (k) => map.get(k) ?? 0, add: async (k, s) => map.set(k, (map.get(k) ?? 0) + s) };
}

function fakeApi({ transcribeFails = 0, cleaned = "Meet at 6." } = {}) {
  const calls = [];
  let failures = transcribeFails;
  return {
    calls,
    providers: () => [{ name: "groq", key: "g" }, { name: "openrouter", key: "o" }],
    withFallback: async (list, step) => {
      let last;
      for (const p of list) {
        try {
          return { result: await step(p), provider: p.name };
        } catch (e) {
          last = e;
          if (!(e instanceof UpstreamError) || !e.retryable) throw e;
        }
      }
      throw last;
    },
    transcribe: async (p) => {
      calls.push(`transcribe:${p.name}`);
      if (failures-- > 0) throw new UpstreamError(p.name, 429, "rate limited");
      return "um meet at five actually six";
    },
    chat: async (p) => {
      calls.push(`chat:${p.name}`);
      return cleaned;
    },
  };
}

const device = "0123456789abcdef0123";
const request = (body, headers = {}) =>
  new Request("https://relay.example/api/dictate", {
    method: "POST",
    body,
    headers: { "x-talkee-device": device, "content-type": "audio/ogg", ...headers },
  });

test("a dictation is transcribed, cleaned, and charged to the device and the address", async () => {
  const usage = memoryUsage();
  const api = fakeApi();
  const response = await handle(request(ogg(4)), { env: {}, ip: "1.2.3.4", usage, api, now: new Date("2026-11-13T10:00:00Z") });
  assert.equal(response.status, 200);
  const body = await response.json();
  assert.equal(body.text, "Meet at 6.");
  assert.equal(body.raw, "um meet at five actually six");
  assert.equal(body.usedSeconds, 4);
  assert.equal(usage.map.get(`device/2026-11-13/${device}`), 4);
  assert.equal(usage.map.get("address/2026-11-13/1.2.3.4"), 4);
});

test("Groq out of quota falls back to OpenRouter", async () => {
  const api = fakeApi({ transcribeFails: 1 });
  const response = await handle(request(ogg(2)), { env: {}, ip: "1.2.3.4", usage: memoryUsage(), api });
  assert.equal(response.status, 200);
  assert.deepEqual(api.calls.slice(0, 2), ["transcribe:groq", "transcribe:openrouter"]);
});

test("a device over its daily minutes is refused before anything is sent upstream", async () => {
  const usage = memoryUsage();
  const now = new Date("2026-11-13T23:00:00Z");
  usage.map.set(`device/2026-11-13/${device}`, DEVICE_SECONDS_PER_DAY - 1);
  const api = fakeApi();
  const response = await handle(request(ogg(5)), { env: {}, ip: "1.2.3.4", usage, api, now });
  assert.equal(response.status, 429);
  assert.equal(response.headers.get("retry-after"), "3600");
  assert.equal(api.calls.length, 0);
});

test("a failed transcription costs nothing", async () => {
  const usage = memoryUsage();
  const api = fakeApi({ transcribeFails: 2 });
  const response = await handle(request(ogg(3)), { env: {}, ip: "1.2.3.4", usage, api });
  assert.equal(response.status, 502);
  assert.equal(usage.map.size, 0);
});

test("an answer from the cleanup model is replaced by the raw transcript", async () => {
  const api = fakeApi({ cleaned: "Sure! Here is a detailed plan for your meeting, with an agenda, timings, and a summary of what to bring." });
  const body = await (await handle(request(ogg(2)), { env: {}, ip: "1.2.3.4", usage: memoryUsage(), api })).json();
  assert.equal(body.text, "um meet at five actually six");
  assert.ok(body.cleanupNote);
});

test("bad requests are refused", async () => {
  const opts = { env: {}, ip: "1.2.3.4", usage: memoryUsage(), api: fakeApi() };
  assert.equal((await handle(new Request("https://r/api/dictate"), opts)).status, 405);
  assert.equal((await handle(request(ogg(2), { "x-talkee-device": "short" }), opts)).status, 400);
  assert.equal((await handle(request(Buffer.from("not audio at all, really not")), opts)).status, 415);
  assert.equal((await handle(request(ogg(400)), opts)).status, 413);
});
