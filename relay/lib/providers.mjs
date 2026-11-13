// Talking to Groq, with OpenRouter as the fallback when Groq is down or out of quota.

const GROQ = "https://api.groq.com/openai/v1";
const OPENROUTER = "https://openrouter.ai/api/v1";

const MODELS = {
  groq: { transcribe: "whisper-large-v3-turbo", cleanup: "qwen/qwen3.8-27b" },
  openrouter: { transcribe: "openai/whisper-large-v3-turbo", cleanup: "qwen/qwen3.8-27b" },
};

/** An upstream failure, with whether trying the other provider could help. */
export class UpstreamError extends Error {
  constructor(provider, status, detail) {
    super(`${provider} answered ${status}: ${detail}`);
    this.provider = provider;
    this.status = status;
    // Rate limits, outages, and a bad or missing key are all worth a second provider.
    this.retryable = status === 0 || status === 401 || status === 403 || status === 429 || status >= 500;
  }
}

async function check(provider, response) {
  if (response.ok) return response.json();
  let detail = "";
  try {
    const body = await response.text();
    detail = (JSON.parse(body)?.error?.message ?? body).slice(0, 300);
  } catch {
    // keep it empty
  }
  throw new UpstreamError(provider, response.status, detail);
}

async function call(provider, url, init) {
  try {
    return await check(provider, await fetch(url, { ...init, redirect: "error" }));
  } catch (error) {
    if (error instanceof UpstreamError) throw error;
    throw new UpstreamError(provider, 0, String(error?.message ?? error));
  }
}

/** The providers to try, in order, given which keys the relay has. */
export function providers(env) {
  const list = [];
  if (env.GROQ_API_KEY) list.push({ name: "groq", key: env.GROQ_API_KEY });
  if (env.OPENROUTER_API_KEY) list.push({ name: "openrouter", key: env.OPENROUTER_API_KEY });
  return list;
}

/** Runs `step` against each provider until one succeeds. */
export async function withFallback(list, step) {
  let last;
  for (const provider of list) {
    try {
      return { result: await step(provider), provider: provider.name };
    } catch (error) {
      last = error;
      if (!(error instanceof UpstreamError) || !error.retryable) throw error;
      console.warn(`${provider.name} failed, trying the next provider: ${error.message}`);
    }
  }
  throw last ?? new UpstreamError("relay", 503, "no provider keys are configured");
}

export async function transcribe({ name, key }, audio, { language, hint }) {
  const auth = { Authorization: `Bearer ${key}` };
  if (name === "groq") {
    const form = new FormData();
    form.append("model", MODELS.groq.transcribe);
    form.append("response_format", "json");
    form.append("temperature", "0");
    if (language) form.append("language", language);
    if (hint) form.append("prompt", hint);
    form.append("file", new Blob([audio], { type: "audio/ogg" }), "dictation.ogg");
    const json = await call(name, `${GROQ}/audio/transcriptions`, { method: "POST", headers: auth, body: form });
    return String(json.text ?? "").trim();
  }
  const body = {
    model: MODELS.openrouter.transcribe,
    input_audio: { data: Buffer.from(audio).toString("base64"), format: "ogg" },
    temperature: 0,
    ...(language ? { language } : {}),
  };
  const json = await call(name, `${OPENROUTER}/audio/transcriptions`, {
    method: "POST",
    headers: { ...auth, "Content-Type": "application/json" },
    body: JSON.stringify(body),
  });
  return String(json.text ?? "").trim();
}

export async function chat({ name, key }, system, user, maxTokens) {
  const body = {
    model: MODELS[name].cleanup,
    temperature: 0,
    max_tokens: maxTokens,
    messages: [
      { role: "system", content: system },
      { role: "user", content: user },
    ],
    // A rewrite needs no thinking time, and on Groq's free tier the cap counts against a per-minute
    // output budget, so thinking is switched off.
    ...(name === "groq" ? { reasoning_effort: "none" } : { reasoning: { effort: "none" } }),
  };
  const json = await call(name, `${name === "groq" ? GROQ : OPENROUTER}/chat/completions`, {
    method: "POST",
    headers: { Authorization: `Bearer ${key}`, "Content-Type": "application/json" },
    body: JSON.stringify(body),
  });
  return String(json?.choices?.[0]?.message?.content ?? "");
}
