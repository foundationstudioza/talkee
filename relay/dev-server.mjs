// The relay on this machine, for development: `node dev-server.mjs`, then
// `talkee dictate --relay-url http://localhost:8888`.
//
// Runs the same handler as Netlify, with keys from the repository's .env file (which is gitignored)
// and usage kept in memory.

import { readFileSync } from "node:fs";
import { createServer } from "node:http";
import { handle } from "./lib/dictate.mjs";

const env = {};
for (const line of readFileSync(new URL("../.env", import.meta.url), "utf8").split(/\r?\n/)) {
  const match = line.match(/^\s*(?:export\s+)?([A-Z_]+)\s*=\s*"?([^"]*)"?\s*$/);
  if (match && match[2]) env[match[1]] = match[2];
}

const used = new Map();
const usage = { get: async (k) => used.get(k) ?? 0, add: async (k, s) => used.set(k, (used.get(k) ?? 0) + s) };

const port = Number(process.env.PORT ?? 8888);
createServer(async (req, res) => {
  const chunks = [];
  for await (const chunk of req) chunks.push(chunk);
  const request = new Request(`http://localhost:${port}${req.url}`, {
    method: req.method,
    headers: req.headers,
    body: req.method === "POST" ? Buffer.concat(chunks) : undefined,
  });
  const response = req.url.startsWith("/api/dictate")
    ? await handle(request, { env, ip: req.socket.remoteAddress, usage })
    : new Response("not found", { status: 404 });
  res.writeHead(response.status, Object.fromEntries(response.headers));
  res.end(Buffer.from(await response.arrayBuffer()));
}).listen(port, "127.0.0.1", () => {
  console.log(`relay on http://localhost:${port} (keys: ${Object.keys(env).filter((k) => k.endsWith("_API_KEY")).join(", ") || "none"})`);
});
