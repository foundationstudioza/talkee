// One dictation: Ogg Opus in, clean text out, charged against the device's and the address's day.
//
// The handler is kept apart from Netlify so it can be tested with a fake store and fake providers.

import { accept, maxTokens, messages, singleLine } from "./cleanup.mjs";
import { oggOpusSeconds } from "./ogg.mjs";
import * as upstream from "./providers.mjs";

/** Seconds of dictation one device gets per UTC day. */
export const DEVICE_SECONDS_PER_DAY = 60 * 60;
/** Seconds one network address gets per UTC day: a household, or someone making up device ids. */
export const ADDRESS_SECONDS_PER_DAY = 4 * 60 * 60;
/** The longest recording the apps make (5 minutes), plus slack for the encoder's padding. */
const MAX_SECONDS = 305;
/** 5 minutes of 24 kbps Opus is about 1 MB; anything far bigger is not a dictation. */
const MAX_BYTES = 3 * 1024 * 1024;

const json = (status, body, headers = {}) =>
  new Response(JSON.stringify(body), {
    status,
    headers: { "Content-Type": "application/json", "Cache-Control": "no-store", ...headers },
  });

/** Options the app sends as base64url JSON in `X-Talkee-Options`, all optional. */
export function parseOptions(header) {
  if (!header) return {};
  try {
    const value = JSON.parse(Buffer.from(header, "base64url").toString("utf8"));
    const vocabulary = Array.isArray(value.vocabulary)
      ? value.vocabulary.filter((t) => typeof t === "string").map((t) => t.trim().slice(0, 60)).filter(Boolean).slice(0, 100)
      : [];
    const language = typeof value.language === "string" && /^[a-z]{2,3}$/.test(value.language) ? value.language : undefined;
    return {
      vocabulary,
      language,
      cleanup: value.cleanup !== false,
      instructions: typeof value.instructions === "string" ? value.instructions.slice(0, 500) : "",
    };
  } catch {
    return {};
  }
}

const today = (now) => now.toISOString().slice(0, 10);

/**
 * Handles `POST /api/dictate`.
 *
 * `usage` is `{ get(key) -> number, add(key, seconds) }`; `env` holds the provider keys; `ip` is the
 * caller's address; `api` replaces the upstream calls in tests.
 */
export async function handle(request, { env, ip, usage, now = new Date(), api = upstream }) {
  if (request.method !== "POST") return json(405, { error: "use POST" }, { Allow: "POST" });

  const device = request.headers.get("x-talkee-device") ?? "";
  if (!/^[A-Za-z0-9-]{16,64}$/.test(device)) return json(400, { error: "missing or malformed device id" });

  const length = Number(request.headers.get("content-length") ?? 0);
  if (length > MAX_BYTES) return json(413, { error: "recording too large" });
  const audio = new Uint8Array(await request.arrayBuffer());
  if (audio.length > MAX_BYTES) return json(413, { error: "recording too large" });

  const seconds = oggOpusSeconds(audio);
  if (seconds === null) return json(415, { error: "send Ogg Opus audio" });
  if (seconds > MAX_SECONDS) return json(413, { error: "recordings are limited to 5 minutes" });
  if (seconds < 0.2) return json(422, { error: "the recording was too short" });

  const day = today(now);
  const deviceKey = `device/${day}/${device}`;
  const addressKey = `address/${day}/${ip || "unknown"}`;
  const [deviceUsed, addressUsed] = await Promise.all([usage.get(deviceKey), usage.get(addressKey)]);
  if (deviceUsed + seconds > DEVICE_SECONDS_PER_DAY || addressUsed + seconds > ADDRESS_SECONDS_PER_DAY) {
    const limit = Math.round(DEVICE_SECONDS_PER_DAY / 60);
    return json(
      429,
      {
        error: `Today's ${limit} minutes of dictation are used up. It resets at midnight UTC.`,
        usedSeconds: Math.round(deviceUsed),
        limitSeconds: DEVICE_SECONDS_PER_DAY,
      },
      { "Retry-After": String(secondsToMidnight(now)) },
    );
  }

  const options = parseOptions(request.headers.get("x-talkee-options"));
  const list = api.providers(env);
  if (list.length === 0) return json(503, { error: "dictation is not configured on the relay" });

  let raw;
  try {
    ({ result: raw } = await api.withFallback(list, (p) =>
      api.transcribe(p, audio, { language: options.language, hint: options.vocabulary?.join(", ").slice(0, 800) }),
    ));
  } catch (error) {
    console.error(`transcription failed: ${error.message}`);
    return json(502, { error: "transcription is unavailable right now; try again in a moment" });
  }

  // Charged once a transcript exists: a failed upstream call costs the user nothing.
  await Promise.all([usage.add(deviceKey, seconds), usage.add(addressKey, seconds)]);

  let text = raw;
  let cleanupNote = null;
  if (options.cleanup !== false && raw) {
    const { system, user } = messages(raw, options.vocabulary ?? [], options.instructions);
    try {
      const { result } = await api.withFallback(list, (p) => api.chat(p, system, user, maxTokens(raw)));
      const accepted = accept(raw, result);
      if (accepted === null) cleanupNote = "cleanup changed the text too much; used the raw transcript";
      else text = accepted;
    } catch (error) {
      console.error(`cleanup failed: ${error.message}`);
      cleanupNote = "cleanup is unavailable right now; used the raw transcript";
    }
  }

  return json(200, {
    raw,
    text: singleLine(text),
    cleanupNote,
    seconds: Math.round(seconds * 10) / 10,
    usedSeconds: Math.round(deviceUsed + seconds),
    limitSeconds: DEVICE_SECONDS_PER_DAY,
  });
}

function secondsToMidnight(now) {
  const midnight = Date.UTC(now.getUTCFullYear(), now.getUTCMonth(), now.getUTCDate() + 1);
  return Math.ceil((midnight - now.getTime()) / 1000);
}
