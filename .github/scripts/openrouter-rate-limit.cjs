// .github/scripts/openrouter-rate-limit.cjs
// Rate limiter, prompt-batching helper, and request-efficiency telemetry
// for OpenRouter requests (enforces <= 18.2 RPM safely under the 20 RPM free-tier limit).

const origFetch = globalThis.fetch;

// 3.3 seconds minimum between requests = max ~18.2 requests/minute (safe under 20 RPM limit)
const MIN_INTERVAL_MS = (() => {
  const envVal = Number(process.env.OIFS_OPENROUTER_MIN_INTERVAL_MS);
  return Number.isFinite(envVal) && envVal >= 0 ? envVal : 3300;
})();

// Never sleep longer than 2 minutes on a 429; longer reset windows indicate a daily
// RPD quota exhaustion rather than a per-minute RPM limit and would hit the 6h CI timeout.
const MAX_RETRY_WAIT_MS = 120000;
const SHORT_OUTPUT_THRESHOLD = 100;

let lastRequestTime = 0;
let queue = Promise.resolve();

const stats = {
  totalRequests: 0,
  http2xx: 0,
  http429: 0,
  httpOther: 0,
  networkErrors: 0,
  chatCompletions: 0,
  plannerTurns: 0,
  plannerBudgetNudges: 0,
  usageObserved: 0,
  shortOutputTurns: 0,
  singleToolCallTurns: 0,
  multiToolCallTurns: 0,
  totalPromptTokens: 0,
  totalCompletionTokens: 0,
};

let summaryPrinted = false;

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

/**
 * Remove any Content-Length header after mutating a request JSON body so undici
 * recomputes the exact byte length.
 */
function stripContentLengthHeader(headers) {
  if (!headers) {
    return headers;
  }
  if (typeof Headers !== "undefined" && headers instanceof Headers) {
    const next = new Headers(headers);
    next.delete("content-length");
    return next;
  }
  if (Array.isArray(headers)) {
    return headers.filter(
      (entry) =>
        !(
          Array.isArray(entry) &&
          typeof entry[0] === "string" &&
          entry[0].toLowerCase() === "content-length"
        )
    );
  }
  if (typeof headers === "object") {
    const next = {};
    for (const [key, value] of Object.entries(headers)) {
      if (key.toLowerCase() !== "content-length") {
        next[key] = value;
      }
    }
    return next;
  }
  return headers;
}

const BATCH_RULE_MARKER = "[OIFS_BATCH_RULE]";
const PLANNER_SOFT_TURN_CAP = 14;

function hasToolNamed(tools, targetName) {
  if (!Array.isArray(tools)) return false;
  return tools.some((t) => {
    const name = t?.function?.name || t?.name || "";
    return name === targetName;
  });
}

function messagesContainToolCall(messages, targetName) {
  if (!Array.isArray(messages)) return false;
  return messages.some(
    (m) =>
      m &&
      m.role === "assistant" &&
      Array.isArray(m.tool_calls) &&
      m.tool_calls.some((tc) => (tc?.function?.name || tc?.name || "") === targetName)
  );
}

function appendTextToMessageContent(msg, extraText) {
  if (!msg || typeof msg !== "object") return false;
  if (typeof msg.content === "string") {
    if (!msg.content.includes(BATCH_RULE_MARKER)) {
      msg.content += extraText;
      return true;
    }
    return false;
  }
  if (Array.isArray(msg.content)) {
    const already = msg.content.some(
      (part) => typeof part?.text === "string" && part.text.includes(BATCH_RULE_MARKER)
    );
    if (!already) {
      msg.content.push({ type: "text", text: extraText });
      return true;
    }
  }
  return false;
}

/**
 * For OpenRouter /chat/completions payloads that declare `tools`:
 * 1. Inject a concise batching & incremental-planning rule into the system prompt.
 * 2. Track planning turns (`submit_plan` tool present) and inject an urgent wrap-up
 *    nudge if the planner exceeds `PLANNER_SOFT_TURN_CAP` turns, preventing
 *    "Repository planning worker exited without submit_plan" failures.
 */
function shapeChatCompletionInit(url, init) {
  if (!url.includes("/chat/completions")) {
    return { init, isPlanner: false };
  }
  if (!init || typeof init.body !== "string") {
    return { init, isPlanner: false };
  }

  let parsed;
  try {
    parsed = JSON.parse(init.body);
  } catch {
    return { init, isPlanner: false };
  }

  if (
    !parsed ||
    typeof parsed !== "object" ||
    !Array.isArray(parsed.tools) ||
    parsed.tools.length === 0
  ) {
    return { init, isPlanner: false };
  }

  let mutated = false;

  const isPlanner = hasToolNamed(parsed.tools, "submit_plan");
  if (isPlanner) {
    stats.plannerTurns++;
  }

  if (Array.isArray(parsed.messages) && parsed.messages.length > 0) {
    const sysMsg =
      parsed.messages.find((m) => m && m.role === "system") || parsed.messages[0];
    const ruleText = isPlanner
      ? `\n\n${BATCH_RULE_MARKER} CRITICAL EFFICIENCY & COMPLETION RULES:\n1. Issue multiple independent tool calls in parallel in a SINGLE turn whenever possible.\n2. During --update planning, do NOT read unmodified existing openwiki/*.md pages or full test suites. Inspect only the changed source files since the last commit and keep the existing page structure.\n3. Finish by calling \`submit_plan\` once (never end before calling \`submit_plan\`, and once \`submit_plan\` is accepted, stop immediately without calling it again).`
      : `\n\n${BATCH_RULE_MARKER} CRITICAL EFFICIENCY RULES:\n1. Issue multiple independent read_file/grep tool calls in parallel in a SINGLE turn instead of one tool call per turn.\n2. Read large contiguous line ranges (400-800 lines) per read_file call rather than small snippets.`;

    if (appendTextToMessageContent(sysMsg, ruleText)) {
      mutated = true;
    }

    if (
      isPlanner &&
      stats.plannerTurns >= PLANNER_SOFT_TURN_CAP &&
      stats.plannerBudgetNudges === 0 &&
      !messagesContainToolCall(parsed.messages, "submit_plan")
    ) {
      stats.plannerBudgetNudges++;
      parsed.messages.push({
        role: "user",
        content: `[SYSTEM TURN-BUDGET ALERT]: You have already used ${stats.plannerTurns} planning turns. Stop calling exploratory read/grep tools and call \`submit_plan\` once using the evidence gathered so far.`,
      });
      mutated = true;
    }
  }

  if (!mutated) {
    return { init, isPlanner };
  }

  return {
    init: {
      ...init,
      headers: stripContentLengthHeader(init.headers),
      body: JSON.stringify(parsed),
    },
    isPlanner,
  };
}

function recordUsageAndToolCalls(usage, toolCallCount) {
  if (usage && typeof usage === "object") {
    const promptTokens = Number(usage.prompt_tokens);
    const completionTokens = Number(usage.completion_tokens);
    if (Number.isFinite(promptTokens) && promptTokens >= 0) {
      stats.totalPromptTokens += promptTokens;
    }
    if (Number.isFinite(completionTokens) && completionTokens >= 0) {
      stats.usageObserved++;
      stats.totalCompletionTokens += completionTokens;
      if (completionTokens < SHORT_OUTPUT_THRESHOLD) {
        stats.shortOutputTurns++;
      }
    }
  }

  if (toolCallCount === 1) {
    stats.singleToolCallTurns++;
  } else if (toolCallCount >= 2) {
    stats.multiToolCallTurns++;
  }
}

function logPlannerZeroToolResponse(isPlanner, finishReason, contentText) {
  if (!isPlanner) return;
  const compact = String(contentText || "")
    .replace(/\s+/g, " ")
    .trim();
  const preview = compact.length > 240 ? `${compact.slice(0, 240)}...` : compact;
  console.log(
    `[RateLimiter] Planner turn #${stats.plannerTurns} returned 0 tool_calls | finish_reason=${finishReason || "unknown"} | preview=${JSON.stringify(preview)}`
  );
}

/**
 * Parse a cloned /chat/completions response (JSON or SSE) for efficiency telemetry.
 * Runs off the hot path via `res.clone()` so it never blocks or mutates the caller's stream.
 */
function observeChatCompletionResponse(res, isPlanner) {
  stats.chatCompletions++;

  let cloned;
  try {
    cloned = res.clone();
  } catch {
    return;
  }

  const contentType = (res.headers.get("content-type") || "").toLowerCase();

  if (contentType.includes("text/event-stream")) {
    cloned
      .text()
      .then((text) => {
        let usage = null;
        let finishReason = null;
        let contentText = "";
        const toolIndices = new Set();
        for (const rawLine of text.split(/\r?\n/)) {
          const line = rawLine.trim();
          if (!line.startsWith("data:")) continue;
          const payload = line.slice(5).trim();
          if (!payload || payload === "[DONE]") continue;
          let chunk;
          try {
            chunk = JSON.parse(payload);
          } catch {
            continue;
          }
          if (chunk && chunk.usage) {
            usage = chunk.usage;
          }
          const choice = chunk?.choices?.[0];
          if (choice?.finish_reason) {
            finishReason = choice.finish_reason;
          }
          if (typeof choice?.delta?.content === "string") {
            contentText += choice.delta.content;
          }
          if (typeof choice?.message?.content === "string") {
            contentText += choice.message.content;
          }
          const deltaTools = choice?.delta?.tool_calls;
          if (Array.isArray(deltaTools)) {
            for (let i = 0; i < deltaTools.length; i++) {
              const tc = deltaTools[i];
              toolIndices.add(tc && tc.index !== undefined ? tc.index : i);
            }
          }
          const msgTools = choice?.message?.tool_calls;
          if (Array.isArray(msgTools)) {
            for (let i = 0; i < msgTools.length; i++) {
              toolIndices.add(i);
            }
          }
        }
        recordUsageAndToolCalls(usage, toolIndices.size);
        if (toolIndices.size === 0) {
          logPlannerZeroToolResponse(isPlanner, finishReason, contentText);
        }
      })
      .catch(() => {});
    return;
  }

  if (contentType.includes("application/json") || contentType === "") {
    cloned
      .json()
      .then((body) => {
        const choice = body?.choices?.[0];
        const msgTools = choice?.message?.tool_calls;
        const toolCount = Array.isArray(msgTools) ? msgTools.length : 0;
        recordUsageAndToolCalls(body?.usage, toolCount);
        if (toolCount === 0) {
          logPlannerZeroToolResponse(
            isPlanner,
            choice?.finish_reason,
            choice?.message?.content
          );
        }
      })
      .catch(() => {});
  }
}

function printSummary() {
  if (summaryPrinted || stats.totalRequests === 0) {
    return;
  }
  summaryPrinted = true;

  const shortPct =
    stats.usageObserved > 0
      ? ((stats.shortOutputTurns / stats.usageObserved) * 100).toFixed(1)
      : "n/a";
  const avgOut =
    stats.usageObserved > 0
      ? (stats.totalCompletionTokens / stats.usageObserved).toFixed(1)
      : "n/a";

  console.log(
    [
      "[RateLimiter] OpenRouter Run Summary:",
      `requests=${stats.totalRequests} (2xx=${stats.http2xx}, 429=${stats.http429}, other=${stats.httpOther}, netErr=${stats.networkErrors})`,
      `chat=${stats.chatCompletions}`,
      `planner_turns=${stats.plannerTurns}(nudges=${stats.plannerBudgetNudges})`,
      `tool_turns(single=${stats.singleToolCallTurns}, multi=${stats.multiToolCallTurns})`,
      `short_output(<${SHORT_OUTPUT_THRESHOLD}tok)=${stats.shortOutputTurns}/${stats.usageObserved} (${shortPct}%)`,
      `tokens(prompt=${stats.totalPromptTokens}, completion=${stats.totalCompletionTokens}, avg_out=${avgOut})`,
    ].join(" | ")
  );
}

process.on("beforeExit", printSummary);
process.on("exit", printSummary);

globalThis.fetch = async function (input, init) {
  const url = extractUrl(input);

  // Only intercept requests targeting OpenRouter API
  if (!url.includes("openrouter.ai")) {
    return origFetch(input, init);
  }

  const { init: effectiveInit, isPlanner } = shapeChatCompletionInit(url, init);
  const isChatCompletion = url.includes("/chat/completions");

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
          stats.totalRequests++;

          try {
            const res = await origFetch(input, effectiveInit);

            if (res.status === 429) {
              stats.http429++;
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
              if (waitMs > MAX_RETRY_WAIT_MS) {
                console.log(
                  `[RateLimiter] OpenRouter 429 reset is ${(waitMs / 1000).toFixed(1)}s away (> ${MAX_RETRY_WAIT_MS / 1000}s; likely daily quota). Aborting retry so partial progress can be saved.`
                );
                resolve(res);
                return;
              }
              console.log(
                `[RateLimiter] OpenRouter 429 received. Waiting ${(waitMs / 1000).toFixed(1)}s before retry (attempt ${attempts}/${maxAttempts})...`
              );
              await delay(waitMs);
              continue;
            }

            if (res.ok) {
              stats.http2xx++;
              if (isChatCompletion) {
                observeChatCompletionResponse(res, isPlanner);
              }
            } else {
              stats.httpOther++;
            }

            resolve(res);
            return;
          } catch (err) {
            stats.networkErrors++;
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
  "[RateLimiter] OpenRouter rate-limiter & telemetry initialized (<= 18.2 RPM safe pacing)."
);
