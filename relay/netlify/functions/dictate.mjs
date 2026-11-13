// POST /api/dictate: the Talkee apps' dictation endpoint. See ../../lib/dictate.mjs.
//
// The provider keys are Netlify environment variables (GROQ_API_KEY, OPENROUTER_API_KEY), set in the
// Netlify dashboard and nowhere else: not in this repository, not in the apps.

import { getStore } from "@netlify/blobs";
import { handle } from "../../lib/dictate.mjs";

/** Seconds used, per key, in a Blobs store. Read-modify-write: two simultaneous dictations from one
 * device can under-count by one recording, which is fine for a daily allowance. */
function usageStore() {
  const store = getStore("usage");
  return {
    async get(key) {
      const value = await store.get(key, { type: "json" });
      return typeof value?.seconds === "number" ? value.seconds : 0;
    },
    async add(key, seconds) {
      const current = await this.get(key);
      await store.setJSON(key, { seconds: current + seconds });
    },
  };
}

export default async (request, context) =>
  handle(request, {
    env: {
      GROQ_API_KEY: Netlify.env.get("GROQ_API_KEY"),
      OPENROUTER_API_KEY: Netlify.env.get("OPENROUTER_API_KEY"),
    },
    ip: context.ip,
    usage: usageStore(),
  });

export const config = {
  path: "/api/dictate",
  method: "POST",
  // A second fence beside the daily minutes: nobody dictates more than once every couple of
  // seconds, so 30 requests a minute from one address is already generous.
  rateLimit: { action: "rate_limit", aggregateBy: ["ip", "domain"], windowSize: 60, windowLimit: 30 },
};
