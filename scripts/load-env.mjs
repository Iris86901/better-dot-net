import fs from "node:fs";
import path from "node:path";
import { fileURLToPath } from "node:url";

const __dirname = path.dirname(fileURLToPath(import.meta.url));
const ROOT = path.resolve(__dirname, "..");

/**
 * Loads `.env.local` and `.env` from the repository root without overwriting
 * variables already set in `process.env`. Zero external dependencies.
 */
export function loadLocalEnv() {
  for (const name of [".env.local", ".env"]) {
    const file = path.join(ROOT, name);
    if (!fs.existsSync(file)) continue;
    const content = fs.readFileSync(file, "utf8");
    for (const rawLine of content.split(/\r?\n/)) {
      const line = rawLine.trim();
      if (!line || line.startsWith("#")) continue;
      const eq = line.indexOf("=");
      if (eq <= 0) continue;
      const key = line.slice(0, eq).trim();
      let val = line.slice(eq + 1).trim();
      if (
        (val.startsWith('"') && val.endsWith('"')) ||
        (val.startsWith("'") && val.endsWith("'"))
      ) {
        val = val.slice(1, -1);
      }
      if (!(key in process.env) || process.env[key] === "") {
        process.env[key] = val;
      }
    }
  }
}

export function getVerifyConfig() {
  loadLocalEnv();
  const key = (process.env.ACCESS_KEY || "").trim();
  const base = (
    process.env.BASE_URL ||
    (process.env.DOT_PROXY_DOMAIN ? `https://${process.env.DOT_PROXY_DOMAIN}` : "")
  )
    .trim()
    .replace(/\/+$/, "");
  const chromePath =
    (process.env.CHROME_PATH || "").trim() ||
    "C:\\Program Files\\Google\\Chrome\\Application\\chrome.exe";
  if (!key || !base) {
    throw new Error(
      "Missing BASE_URL (or DOT_PROXY_DOMAIN) / ACCESS_KEY. Copy .env.example to .env.local and configure them."
    );
  }
  return { key, base, chromePath };
}
