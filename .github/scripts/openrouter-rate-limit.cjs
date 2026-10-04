// .github/scripts/openrouter-rate-limit.cjs
// Rate limiter for OpenRouter free models (enforces <= 19 RPM to stay safely under the 20 RPM limit)

const origFetch = globalThis.fetch;

// 3.3 seconds minimum between requests = max ~18.2 requests/minute (safe under 20 RPM limit)
const MIN_INTERVAL_MS = 3300;
let lastRequestTime = 0;
let queue = Promise.resolve();

function delay(ms) {
  return new Promise((resolve) => setTimeout(resolve, ms));
}

/**
 * Resolve the request URL from whatever `fetch` was called with: a string,
 * a `URL`, or a `Request`-like object carrying a `url` property.
 * Returns "" when no URL can be determined.
 */
function extractUrl(input) {
  if (typeof input === "string") {
    return input;
  }
  if (input instanceof URL) {
    return input.href;
  }
  if (input && typeof input.url === "string") {
    return input.url;
  }
  return "";
}

globalThis.fetch = async function (input, init) {
  const url = extractUrl(input);

  // Only rate-limit requests targeting OpenRouter API
  if (!url.includes("openrouter.ai")) {
    return origFetch(input, init);
  }

  // Queue all OpenRouter requests so they are paced sequentially
  return new Promise((resolve, reject) => {
    queue = queue
      .then(async () => {
        let attempts = 0;
        const maxAttempts = 8;

        while (attempts < maxAttempts) {
          attempts++;

          // Enforce minimum interval between consecutive requests
          const now = Date.now();
          const elapsed = now - lastRequestTime;
          if (elapsed < MIN_INTERVAL_MS) {
            await delay(MIN_INTERVAL_MS - elapsed);
          }
          lastRequestTime = Date.now();

          try {
            const res = await origFetch(input, init);

            if (res.status === 429) {
              const resetHeader = res.headers.get("x-ratelimit-reset");
              let waitMs = 25000; // default 25s
              if (resetHeader) {
                const resetTime = Number(resetHeader);
                // Number.isFinite rejects both NaN and +/-Infinity, so a malformed
                // or unparseable header falls back to the default wait.
                if (Number.isFinite(resetTime)) {
                  const resetMs =
                    resetTime > 1e11 ? resetTime : resetTime * 1000;
                  waitMs = Math.max(3000, resetMs - Date.now() + 1000);
                }
              }
              console.log(
                `[RateLimiter] OpenRouter 429 received. Waiting ${(waitMs / 1000).toFixed(1)}s before retry (attempt ${attempts}/${maxAttempts})...`
              );
              await delay(waitMs);
              continue;
            }

            resolve(res);
            return;
          } catch (err) {
            if (attempts >= maxAttempts) {
              reject(err);
              return;
            }
            console.log(
              `[RateLimiter] Network error (${err.message}). Retrying in 3s...`
            );
            await delay(3000);
          }
        }
        reject(
          new Error("[RateLimiter] Exceeded max retries for OpenRouter request")
        );
      })
      .catch(reject);
  });
};

console.log(
  "[RateLimiter] OpenRouter request rate-limiter initialized (<= 16 RPM safe pacing)."
);
