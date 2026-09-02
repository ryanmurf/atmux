import assert from "node:assert/strict";
import { spawn } from "node:child_process";
import { EventEmitter } from "node:events";
import { mkdtemp, readFile, rm } from "node:fs/promises";
import { createServer } from "node:http";
import { tmpdir } from "node:os";
import { extname, join, resolve } from "node:path";
import test from "node:test";

const WEB_ROOT = resolve(import.meta.dirname, "../web");
const CDP_COMMAND_TIMEOUT_MS = 15_000;
const CHROME_START_TIMEOUT_MS = 30_000;
const CLEANUP_TIMEOUT_MS = 5_000;
let transcriptFixture = null;
let paneSnapshotContent = "";
const paneStreams = new Set();
const overviewStreams = new Set();
const launchRequests = [];
const launchSessionRequests = [];
let failLiveModels = false;
let launchOptionsDelayMs = 0;
let launchResponseDelayMs = 0;
let launchMachinesUnavailable = false;
let overviewRevision = 1;
let delayProjectFilePane = null;
let delayFileSavePane = null;
let delayGitSummaryPane = null;
let delayGitDiffPane = null;
let nextFileSaveConflict = false;
const fileSaveRequests = [];
const messageRequests = [];
const projectFileContents = new Map();
const projectFileVersions = new Map();
const LONG_KERNEL_VERSION = "k".repeat(160);
const LONG_OS_VERSION = "o".repeat(160);

function fixtureProjectFile(paneId) {
  if (!projectFileContents.has(paneId)) {
    projectFileContents.set(
      paneId,
      Array.from({ length: 320 }, (_, index) => `const line${index} = "${paneId} <script>safe ${index}</script> ${"x".repeat(100)}";`).join("\n"),
    );
  }
  if (!projectFileVersions.has(paneId)) projectFileVersions.set(paneId, 1);
  return projectFileContents.get(paneId);
}

function fixtureProjectHash(paneId) {
  fixtureProjectFile(paneId);
  return String(projectFileVersions.get(paneId)).repeat(64);
}

function mockSession(machine, pane, name, status, extra = {}) {
  return {
    id: `${machine}~${pane}`, pane_id: pane, machine, name, status,
    agent: "claude", profile: "max", path: "/workspace", command: "claude",
    ...extra,
  };
}

function emitOverviewPatch(upsert, remove = []) {
  const baseRevision = overviewRevision;
  overviewRevision += 1;
  const payload = `event: sessions.patch\ndata: ${JSON.stringify({
    base_revision: baseRevision,
    revision: overviewRevision,
    upsert,
    remove,
    health: null,
    machines: [],
  })}\n\n`;
  for (const response of overviewStreams) response.write(payload);
}

function emitPanePatch(patch) {
  const payload = `event: pane.patch\ndata: ${JSON.stringify(patch)}\n\n`;
  for (const response of paneStreams) response.write(payload);
}

function json(response, value) {
  response.writeHead(200, { "content-type": "application/json", "cache-control": "no-store" });
  response.end(JSON.stringify(value));
}

function errorJson(response, status, message) {
  response.writeHead(status, { "content-type": "application/json", "cache-control": "no-store" });
  response.end(JSON.stringify({ error: message }));
}

function mockApi(url, response, request) {
  const { pathname } = url;
  if (pathname === "/api/v1/events") {
    response.writeHead(200, { "content-type": "text/event-stream", "cache-control": "no-store" });
    response.write(`event: sessions.snapshot\ndata: ${JSON.stringify({
      revision: overviewRevision,
      sessions: [{
        id: "tron~%100", pane_id: "%100", machine: "tron", name: "codex-main",
        status: "waiting", agent: "codex", profile: "codex-max", path: "/workspace", command: "codex",
      },
      mockSession("midnight", "%5", "alpha-planner", "working"),
      mockSession("midnight", "%7", "beta-planner", "waiting"),
      ],
      machines: [
        {
          id: "tron", label: "Tron", kind: "local", online: true, sessions: 1,
          metrics: {
            uptime_seconds: 183_840,
            kernel_version: LONG_KERNEL_VERSION,
            os_version: LONG_OS_VERSION,
          },
        },
        { id: "midnight", label: "Midnight", kind: "remote", online: true, sessions: 2 },
      ],
      health: null,
    })}\n\n`);
    overviewStreams.add(response);
    request.once("close", () => { overviewStreams.delete(response); });
    return true;
  }
  if (/^\/api\/v1\/panes\/[^/]+\/events$/.test(pathname)) {
    response.writeHead(200, { "content-type": "text/event-stream", "cache-control": "no-store" });
    response.write(`event: pane.snapshot\ndata: ${JSON.stringify({ revision: 1, content: paneSnapshotContent })}\n\n`);
    paneStreams.add(response);
    request.once("close", () => { paneStreams.delete(response); });
    return true;
  }
  if (/^\/api\/v1\/panes\/[^/]+\/transcript$/.test(pathname)) {
    json(response, transcriptFixture || { available: false, source: "codex", changed: false, messages: [] });
    return true;
  }
  if (/^\/api\/v1\/panes\/[^/]+\/files$/.test(pathname)) {
    const paneId = decodeURIComponent(pathname.split("/")[4]);
    const path = url.searchParams.get("path") || "";
    if (request.method === "PUT") {
      let body = "";
      request.setEncoding("utf8");
      request.on("data", (chunk) => { body += chunk; });
      request.on("end", () => {
        let parsed = null;
        try { parsed = JSON.parse(body); } catch { /* asserted below */ }
        fileSaveRequests.push({ paneId, path, body: parsed });
        const reply = () => {
          if (nextFileSaveConflict) {
            nextFileSaveConflict = false;
            const version = projectFileVersions.get(paneId) + 1;
            projectFileVersions.set(paneId, version);
            projectFileContents.set(paneId, `${fixtureProjectFile(paneId)}\n// external edit`);
            errorJson(response, 409, "file changed since it was opened");
            return;
          }
          if (!parsed || parsed.path !== path || typeof parsed.content !== "string"
            || parsed.expected_hash !== fixtureProjectHash(paneId)) {
            errorJson(response, 400, "invalid save fixture");
            return;
          }
          projectFileContents.set(paneId, parsed.content);
          projectFileVersions.set(paneId, projectFileVersions.get(paneId) + 1);
          json(response, {
            kind: "file", path, language: "javascript", size: Buffer.byteLength(parsed.content),
            truncated: false, content: parsed.content, content_hash: fixtureProjectHash(paneId),
            line_count: parsed.content.split("\n").length,
          });
        };
        if (delayFileSavePane === paneId) {
          delayFileSavePane = null;
          setTimeout(reply, 250);
        } else reply();
      });
      return true;
    }
    if (path === "") {
      json(response, {
        kind: "directory", path: "", truncated: false,
        entries: [
          { kind: "directory", name: "src", path: "src" },
          { kind: "file", name: "README <img onerror=boom>.md", path: "README <img onerror=boom>.md", size: 4096 },
          { kind: "file", name: "image.bin", path: "image.bin", size: 2048 },
        ],
      });
    } else if (path === "src") {
      json(response, {
        kind: "directory", path: "src", truncated: false,
        entries: [{ kind: "file", name: "app.js", path: "src/app.js", size: 8192 }],
      });
    } else if (path === "src/app.js") {
      const content = fixtureProjectFile(paneId);
      const payload = {
        kind: "file", path, language: "javascript", size: Buffer.byteLength(content), truncated: false,
        content, content_hash: fixtureProjectHash(paneId), line_count: content.split("\n").length,
      };
      if (delayProjectFilePane === paneId) {
        delayProjectFilePane = null;
        setTimeout(() => json(response, payload), 250);
      } else json(response, payload);
    } else if (path === "README <img onerror=boom>.md") {
      json(response, { kind: "file", path, language: "markdown", size: 25, truncated: false, content: "# <img onerror=boom>" });
    } else if (path === "image.bin") {
      json(response, { kind: "file", path, language: "text", size: 2048, truncated: false, binary: true, content: "" });
    } else errorJson(response, 404, "file fixture missing");
    return true;
  }
  if (/^\/api\/v1\/panes\/[^/]+\/git$/.test(pathname)) {
    const paneId = decodeURIComponent(pathname.split("/")[4]);
    const path = url.searchParams.get("path");
    if (!path) {
      const payload = {
        available: true, branch: `feature/${paneId}/<script>alert(1)</script>`, detached: false,
        clean: false, truncated: false,
        changes: [
          { status: "M", path: "src/app.js" },
          { status: "R", old_path: "old name.js", path: "new #name.js" },
        ],
      };
      if (delayGitSummaryPane === paneId) {
        delayGitSummaryPane = null;
        setTimeout(() => json(response, payload), 250);
      } else json(response, payload);
    } else {
      const payload = {
        path, truncated: false,
        diff: `diff --git a/${path} b/${path}\n@@ -1 +1 @@\n-const unsafe = "<img onerror=boom>";\n+const safe = "text";`,
      };
      if (delayGitDiffPane === paneId) {
        delayGitDiffPane = null;
        setTimeout(() => json(response, payload), 250);
      } else json(response, payload);
    }
    return true;
  }
  if (/^\/api\/v1\/panes\/[^/]+\/messages$/.test(pathname) && request.method === "POST") {
    let body = "";
    request.setEncoding("utf8");
    request.on("data", (chunk) => { body += chunk; });
    request.on("end", () => { messageRequests.push(body); json(response, {}); });
    return true;
  }
  if (/^\/api\/v1\/panes\/[^/]+\/models$/.test(pathname)) {
    if (failLiveModels) {
      errorJson(response, 503, "live model capability fixture failed");
      return true;
    }
    const paneId = decodeURIComponent(pathname.split("/")[4]);
    json(response, {
      pane_id: paneId, harness: "codex", current: "gpt-5.6-sol", effort: "xhigh",
      current_mode: "sol-fast", version: "0.147.0",
      models: [
        { id: "terra-high", label: "Terra · high", switchable: true },
        { id: "sol-fast", label: "Sol · xhigh · fast", switchable: true },
      ],
      note: null, resume_available: false, resume_note: null,
    });
    return true;
  }
  if (pathname === "/api/v1/launch-options") {
    const value = {
      directories: ["/workspace/discovered"],
      profiles: [{ id: "profile-0", name: "Default", harness: "codex" }],
      project_preferences: {},
      machines: [
        {
          id: "local", label: "This machine", online: true,
          directories: [],
          profiles: [],
          project_preferences: {}, note: null,
        },
        {
          id: "tron", label: "Tron", online: true,
          directories: ["/workspace", "/workspace/discovered"],
          profiles: [{
            id: "profile-codex-max", name: "codex-max", harness: "codex",
            modes: [
              { id: "terra-high", label: "Terra · high", model: "gpt-5.6-terra", effort: "high", service_tier: null },
              { id: "sol-fast", label: "Sol · xhigh · fast", model: "gpt-5.6-sol", effort: "xhigh", service_tier: "fast" },
            ],
          }],
          memory: {
            supported: true,
            default_bytes: 17179869184,
            override_max_bytes: 25769803776,
            presets_bytes: [8589934592, 17179869184, 25769803776],
            note: "Changes apply on the next launch or relaunch.",
          },
          project_preferences: {}, note: null,
        },
      ],
    };
    if (launchMachinesUnavailable) {
      value.machines = value.machines.map((machine) => ({
        ...machine,
        directories: [],
        profiles: [],
        note: "No launch configuration is available on this owner.",
      }));
    }
    if (launchOptionsDelayMs > 0) setTimeout(() => json(response, value), launchOptionsDelayMs);
    else json(response, value);
    return true;
  }
  if (pathname === "/api/v1/launch-directories") {
    const path = url.searchParams.get("path");
    json(response, path === "/workspace/custom"
      ? {
        machine: "tron", current: "/workspace/custom", parent: null,
        directories: [], truncated: false,
      }
      : {
        machine: "tron", current: null, parent: null,
        directories: [{ path: "/workspace/custom", name: "custom" }], truncated: false,
      });
    return true;
  }
  if (pathname === "/api/v1/launch-sessions") {
    const directory = url.searchParams.get("directory");
    const profileId = url.searchParams.get("profile_id");
    launchSessionRequests.push({ directory, profileId, machine: url.searchParams.get("machine") });
    json(response, {
      machine: "tron", directory, profile_id: profileId, truncated: false,
      sessions: directory === "/workspace/custom" && profileId === "profile-codex-max"
        ? [{
          id: "saved-0123456789abcdef0123456789abcdef",
          harness: "codex", updated_ms: 1_786_993_200_000,
          preview: "Continue the mobile launch flow",
        }]
        : [],
    });
    return true;
  }
  if (pathname === "/api/v1/sessions" && request.method === "POST") {
    let body = "";
    request.setEncoding("utf8");
    request.on("data", (chunk) => { body += chunk; });
    request.on("end", () => {
      let parsed = null;
      try { parsed = JSON.parse(body); } catch { /* asserted by the browser test */ }
      launchRequests.push({ method: request.method, pathname, body: parsed });
      const reply = () => {
        if (String(parsed?.name || "").includes("-copy")) {
          errorJson(response, 409, "duplicate fixture intentionally not persisted");
        } else json(response, { ok: true });
      };
      if (launchResponseDelayMs > 0) {
        const delayMs = launchResponseDelayMs;
        launchResponseDelayMs = 0;
        setTimeout(reply, delayMs);
      } else reply();
    });
    return true;
  }
  if (pathname === "/api/v1/pulse/accounts") {
    json(response, [{ id: 4, identity: "ryanmurf@gmail.com", display_name: "Ryan" }]);
    return true;
  }
  if (/^\/api\/v1\/pulse\/accounts\/4\/events$/.test(pathname)) {
    response.writeHead(200, { "content-type": "text/event-stream", "cache-control": "no-store" });
    response.end("id: 1\nevent: pulse\ndata: {\"revision\":1}\n\n");
    return true;
  }
  if (/^\/api\/v1\/pulse\/accounts\/4\/limits$/.test(pathname)) {
    json(response, { capabilities: { collect: true, serve: true, receive: false }, delivery: {} });
    return true;
  }
  if (/^\/api\/v1\/pulse\/accounts\/4\/profiles$/.test(pathname)) {
    json(response, {
      items: [
        {
          account_id: 4, name: "claude-max", vendor: "anthropic-oauth",
          poll_interval_minutes: 15, monthly_budget_usd: null, refresh: "in-memory",
          hidden: false, origin: "local", has_config_dir: true, credential_source: null,
        },
        {
          account_id: 4, name: "codex-max", vendor: "openai-codex",
          poll_interval_minutes: 15, monthly_budget_usd: null, refresh: "in-memory",
          hidden: false, origin: "local", has_config_dir: true, credential_source: null,
        },
      ],
      next_cursor: null,
    });
    return true;
  }
  if (/^\/api\/v1\/pulse\/accounts\/4\/usage$/.test(pathname)) {
    json(response, {
      items: [
        {
          profile: "claude-max", vendor: "anthropic-oauth",
          window: { kind: "five_hour", used_percent: 62.5, resets_at: "2026-08-10T01:00:00Z" },
          polled_at: "2026-08-09T20:00:00Z",
          contributors: [
            { machine: "max", reporter_version: "atmux-fixture", polled_at: "2026-08-09T20:00:00Z", chosen: true },
            { machine: "midnight", reporter_version: "atmux-fixture", polled_at: "2026-08-09T19:55:00Z", chosen: false },
          ],
        },
        {
          profile: "codex-max", vendor: "openai-codex",
          window: { kind: "fixed_weekly", used_percent: 38, resets_at: "2026-08-16T00:00:00Z" },
          polled_at: "2026-08-09T20:00:00Z",
          contributors: [
            { machine: "max", reporter_version: "atmux-fixture", polled_at: "2026-08-09T20:00:00Z", chosen: true },
          ],
        },
      ],
      next_cursor: null,
    });
    return true;
  }
  if (/^\/api\/v1\/pulse\/accounts\/4\/pace$/.test(pathname)) {
    json(response, {
      items: [
        {
          profile: "claude-max", window: "five_hour", used_percent: 62.5,
          capacity_percent: 37.5, remaining_ms: 18_000_000, elapsed_percent: 50,
          projected_used_percent: 100, band: "slightly_fast", chosen_machines: ["max"],
        },
        {
          profile: "codex-max", window: "fixed_weekly", used_percent: 38,
          capacity_percent: 62, remaining_ms: 561_600_000, elapsed_percent: 10,
          projected_used_percent: 100, band: "slightly_fast", chosen_machines: ["max"],
        },
      ],
      next_cursor: null,
    });
    return true;
  }
  if (/^\/api\/v1\/pulse\/accounts\/4\/reports$/.test(pathname)) {
    json(response, {
      range: { since_day: "2026-07-11", through_day: "2026-08-09" },
      total: {
        tokens_in: 1_000_000, tokens_out: 500_000, cache_write_5m: 0,
        cache_write_1h: 0, cache_read: 0, total_tokens: 1_500_000, cost_usd: 4,
      },
      profiles: [{
        profile: "claude-max", tokens_in: 1_000_000, tokens_out: 500_000,
        cache_write_5m: 0, cache_write_1h: 0, cache_read: 0,
        total_tokens: 1_500_000, cost_usd: 4,
        by_period: [{ day: "2026-08-09", total_tokens: 1_500_000, cost_usd: 4 }],
        by_machine: [{ key: "max", total_tokens: 1_500_000, cost_usd: 4 }],
        drill: [{ key: "claude-opus-5", total_tokens: 1_500_000, cost_usd: 4 }],
      }],
      rows_scanned: 1, fallback_priced_rows: 0,
    });
    return true;
  }
  if (/^\/api\/v1\/pulse\/accounts\/4\//.test(pathname)) {
    json(response, { items: [], next_cursor: null });
    return true;
  }
  return false;
}

async function startServer() {
  const server = createServer(async (request, response) => {
    const url = new URL(request.url || "/", "http://atmux.test");
    if (mockApi(url, response, request)) return;
    const name = url.pathname === "/" || url.pathname === "/index.html"
      ? "index.html"
      : url.pathname.slice(1);
    if (!new Set(["index.html", "app.js", "app.css", "atmux-logo.jpg"]).has(name)) {
      response.writeHead(404).end();
      return;
    }
    const body = await readFile(join(WEB_ROOT, name));
    const contentType = ({ ".html": "text/html", ".js": "text/javascript", ".css": "text/css", ".jpg": "image/jpeg" })[extname(name)];
    response.writeHead(200, { "content-type": contentType, "cache-control": "no-store" });
    response.end(body);
  });
  await new Promise((resolveListen) => server.listen(0, "127.0.0.1", resolveListen));
  return { server, port: server.address().port };
}

async function waitFor(check, message, timeoutMs = 10_000) {
  const deadline = Date.now() + timeoutMs;
  while (Date.now() < deadline) {
    const value = await check();
    if (value) return value;
    await new Promise((resolveWait) => setTimeout(resolveWait, 50));
  }
  throw new Error(message);
}

async function withDeadline(promise, timeoutMs, message) {
  let timer;
  try {
    return await Promise.race([
      promise,
      new Promise((_, reject) => {
        timer = setTimeout(() => reject(new Error(message)), timeoutMs);
        timer.unref?.();
      }),
    ]);
  } finally {
    clearTimeout(timer);
  }
}

function childExit(child) {
  if (child.exitCode !== null || child.signalCode !== null) return Promise.resolve();
  return new Promise((resolveExit) => child.once("exit", resolveExit));
}

async function stopChrome(chrome) {
  if (!chrome || chrome.exitCode !== null || chrome.signalCode !== null) return;
  const exited = childExit(chrome);
  chrome.kill("SIGTERM");
  try {
    await withDeadline(exited, CLEANUP_TIMEOUT_MS, "Chrome ignored SIGTERM during browser-test cleanup");
    return;
  } catch {
    // A wedged browser must not pin the CI worker indefinitely. Escalate only
    // after giving normal shutdown a bounded opportunity to preserve profiles.
  }
  if (chrome.exitCode === null && chrome.signalCode === null) chrome.kill("SIGKILL");
  await withDeadline(exited, CLEANUP_TIMEOUT_MS, "Chrome did not exit after SIGKILL during browser-test cleanup");
}

async function stopServer(server) {
  if (!server?.listening) return;
  const closed = new Promise((resolveClose, rejectClose) => {
    server.close((error) => {
      if (error) rejectClose(error);
      else resolveClose();
    });
  });
  // Node's server.close() does not wait out active SSE responses on every
  // supported runtime. Force only this disposable fixture's connections.
  server.closeAllConnections?.();
  await withDeadline(closed, CLEANUP_TIMEOUT_MS, "fixture HTTP server did not close");
}

async function cleanupBrowserHarness({ cdp, chrome, server, profileDirectory }) {
  const errors = [];
  try { cdp?.socket.close(); } catch (error) { errors.push(error); }
  try { await stopChrome(chrome); } catch (error) { errors.push(error); }
  for (const response of [...paneStreams, ...overviewStreams]) {
    try { response.end(); } catch (error) { errors.push(error); }
  }
  try { await stopServer(server); } catch (error) { errors.push(error); }
  try {
    await withDeadline(
      rm(profileDirectory, { recursive: true, force: true, maxRetries: 10, retryDelay: 50 }),
      CLEANUP_TIMEOUT_MS,
      "temporary Chrome profile cleanup timed out",
    );
  } catch (error) { errors.push(error); }
  if (errors.length) throw new AggregateError(errors, "browser-test cleanup failed");
}

test("Chrome cleanup cannot miss an exit emitted while signaling", async () => {
  class SynchronousExitChild extends EventEmitter {
    exitCode = null;
    signalCode = null;
    signals = [];

    kill(signal) {
      this.signals.push(signal);
      this.signalCode = signal;
      this.emit("exit", null, signal);
      return true;
    }
  }

  const chrome = new SynchronousExitChild();
  await stopChrome(chrome);
  assert.deepEqual(chrome.signals, ["SIGTERM"]);
});

async function launchChrome(profileDirectory) {
  const chrome = spawn("google-chrome", [
    "--headless=new", "--no-sandbox", "--disable-gpu", "--disable-dev-shm-usage",
    "--window-size=390,844", "--force-device-scale-factor=1",
    "--remote-debugging-port=0", `--user-data-dir=${profileDirectory}`, "about:blank",
  ], { stdio: ["ignore", "pipe", "pipe"] });
  let chromeOutput = "";
  chrome.stdout.setEncoding("utf8");
  chrome.stderr.setEncoding("utf8");
  const captureChromeOutput = (chunk) => {
    chromeOutput = `${chromeOutput}${chunk}`.slice(-16_384);
  };
  chrome.stdout.on("data", captureChromeOutput);
  chrome.stderr.on("data", captureChromeOutput);
  try {
    const browserSocket = await waitFor(() => {
      if (chrome.exitCode !== null || chrome.signalCode !== null) {
        throw new Error(`Chrome exited before exposing DevTools: ${chromeOutput || "no output"}`);
      }
      const match = chromeOutput.match(/DevTools listening on (ws:\/\/[^\s]+)/);
      return match?.[1] || null;
    }, "Chrome did not expose its DevTools endpoint", CHROME_START_TIMEOUT_MS);
    return { chrome, browserSocket };
  } catch (error) {
    if (chromeOutput) error.message = `${error.message}\nChrome output:\n${chromeOutput}`;
    try { await stopChrome(chrome); } catch (cleanupError) {
      error.cleanupError = cleanupError;
    }
    throw error;
  }
}

async function openCdp(browserSocket, pageUrl) {
  const endpoint = new URL(browserSocket);
  const target = await fetch(`http://${endpoint.host}/json/new?${encodeURIComponent(pageUrl)}`, {
    method: "PUT",
    signal: AbortSignal.timeout(CDP_COMMAND_TIMEOUT_MS),
  })
    .then((response) => response.json());
  const socket = new WebSocket(target.webSocketDebuggerUrl);
  await withDeadline(new Promise((resolveOpen, rejectOpen) => {
    socket.addEventListener("open", resolveOpen, { once: true });
    socket.addEventListener("error", rejectOpen, { once: true });
  }), CDP_COMMAND_TIMEOUT_MS, "CDP WebSocket did not open");
  let nextId = 1;
  const pending = new Map();
  const rejectPending = (reason) => {
    for (const { rejectMessage } of pending.values()) rejectMessage(reason);
    pending.clear();
  };
  socket.addEventListener("close", () => rejectPending(new Error("CDP WebSocket closed")));
  socket.addEventListener("error", () => rejectPending(new Error("CDP WebSocket failed")));
  socket.addEventListener("message", (event) => {
    const message = JSON.parse(event.data);
    if (!message.id || !pending.has(message.id)) return;
    const { resolveMessage, rejectMessage } = pending.get(message.id);
    pending.delete(message.id);
    if (message.error) rejectMessage(new Error(message.error.message));
    else resolveMessage(message.result);
  });
  const send = (method, params = {}) => withDeadline(new Promise((resolveMessage, rejectMessage) => {
    const id = nextId++;
    pending.set(id, { resolveMessage, rejectMessage });
    try {
      socket.send(JSON.stringify({ id, method, params }));
    } catch (error) {
      pending.delete(id);
      rejectMessage(error);
    }
  }), CDP_COMMAND_TIMEOUT_MS, `CDP command timed out: ${method}`);
  const evaluate = async (expression) => {
    const result = await send("Runtime.evaluate", { expression, returnByValue: true, awaitPromise: true });
    if (result.exceptionDetails) throw new Error(JSON.stringify(result.exceptionDetails));
    return result.result.value;
  };
  return { socket, send, evaluate };
}

test("mobile browser Back stays inside atmux and Usage auto-loads its Pulse dashboard", { timeout: 120_000 }, async () => {
  launchRequests.length = 0;
  launchSessionRequests.length = 0;
  fileSaveRequests.length = 0;
  messageRequests.length = 0;
  projectFileContents.clear();
  projectFileVersions.clear();
  delayFileSavePane = null;
  nextFileSaveConflict = false;
  failLiveModels = false;
  launchOptionsDelayMs = 0;
  launchResponseDelayMs = 0;
  overviewRevision = 1;
  const transcript = (start, count, hash) => ({
    available: true,
    source: "codex",
    changed: true,
    content_hash: hash,
    truncated: true,
    messages: Array.from({ length: count }, (_, offset) => ({
      id: `message-${start + offset}`,
      role: "assistant",
      markdown: `Message ${start + offset}\n\n${"Reader position must remain stable while output streams. ".repeat(4)}`,
    })),
  });
  const profileDirectory = await mkdtemp(join(tmpdir(), "atmux-web-browser-"));
  paneSnapshotContent = Array.from(
    { length: 220 },
    (_, index) => `pane-line-${String(index).padStart(3, "0")}`,
  ).join("\n");
  let server;
  let port;
  let chrome;
  let cdp;
  let testError = null;
  try {
    ({ server, port } = await startServer());
    const browser = await launchChrome(profileDirectory);
    chrome = browser.chrome;
    const { browserSocket } = browser;
    cdp = await openCdp(browserSocket, `http://127.0.0.1:${port}/?session=tron~%25100`);
    await cdp.send("Page.enable");
    await waitFor(
      () => cdp.evaluate("document.readyState === 'complete' && Boolean(document.getElementById('agent-view')) && !document.getElementById('agent-view').hidden"),
      "agent detail did not render",
    );
    await waitFor(
      () => cdp.evaluate("document.getElementById('agent-branch').textContent.includes('feature/tron~%100')"),
      "agent header did not discover its Git branch",
    );
    assert.equal(await cdp.evaluate("new URL(location.href).searchParams.get('session')"), "tron~%100");
    const mobile = await cdp.evaluate(`({
      viewport: window.innerHeight,
      body: document.body.getBoundingClientRect().height,
      shell: document.querySelector('.terminal-shell').getBoundingClientRect().height,
      agent: document.getElementById('agent-view').getBoundingClientRect().height,
      detail: document.querySelector('.detail').getBoundingClientRect().height,
      workspace: document.querySelector('.workspace').getBoundingClientRect().height,
      header: document.querySelector('.agent-head').getBoundingClientRect().height,
      composer: document.getElementById('composer').getBoundingClientRect().height,
      profileInHeader: document.getElementById('agent-meta').textContent.includes('codex-max'),
      profileInRail: document.querySelector('.session-sub').textContent.includes('codex-max'),
      wordmarkDisplay: getComputedStyle(document.querySelector('.brand-wordmark')).display,
      logoDisplay: getComputedStyle(document.querySelector('.brand-logo')).display,
      branch: document.getElementById('agent-branch').textContent,
      branchVisible: !document.getElementById('agent-branch').hidden,
      overflowX: document.documentElement.scrollWidth - innerWidth,
    })`);
    assert.ok(mobile.shell >= mobile.viewport * 0.45, JSON.stringify(mobile));
    assert.ok(mobile.header <= 40, JSON.stringify(mobile));
    assert.equal(mobile.profileInHeader, true);
    assert.equal(mobile.profileInRail, true);
    assert.equal(mobile.wordmarkDisplay, "none", JSON.stringify(mobile));
    assert.notEqual(mobile.logoDisplay, "none", JSON.stringify(mobile));
    assert.equal(mobile.branchVisible, true, JSON.stringify(mobile));
    assert.equal(mobile.branch, "Git · feature/tron~%100/<script>alert(1)</script>");
    assert.equal(mobile.overflowX, 0, JSON.stringify(mobile));

    // The first launch option is online but cannot launch. The federated
    // `tron~pane` owner remains the contextual target and Home/local cannot be
    // selected accidentally.
    await cdp.evaluate("document.getElementById('launch-open').click(); true");
    await waitFor(
      () => cdp.evaluate("document.getElementById('launch-dialog').open"),
      "contextual launch dialog did not open",
    );
    assert.equal(await cdp.evaluate("document.getElementById('launch-machine').value"), "tron");
    assert.equal(
      await cdp.evaluate("document.querySelector('#launch-machine option[value=local]').disabled"),
      true,
    );
    await cdp.evaluate("document.querySelector('#launch-dialog .dialog-cancel').click(); true");

    // Machine details expose owner-sampled system identity without inventing a
    // coordinator Home machine. Values are rendered as text, not owner markup.
    await cdp.evaluate("document.getElementById('mobile-back').click(); true");
    await waitFor(
      () => cdp.evaluate("!document.body.classList.contains('has-selection')"),
      "agent menu did not open for machine telemetry",
    );
    const machineLabels = await cdp.evaluate(
      "[...document.querySelectorAll('.machine-label')].map((node) => node.textContent)",
    );
    assert.deepEqual(machineLabels, ["Tron", "Midnight"]);
    await cdp.evaluate(`(() => {
      [...document.querySelectorAll('.machine-header')]
        .find((node) => node.querySelector('.machine-label')?.textContent === 'Tron')
        .click();
      return true;
    })()`);
    await waitFor(
      () => cdp.evaluate("!document.getElementById('machine-view').hidden"),
      "machine detail did not open",
    );
    const systemCard = await cdp.evaluate(`(() => {
      const cards = [...document.querySelectorAll('#machine-metrics .metric-card')];
      const card = cards.find((node) => node.querySelector('h2')?.textContent === 'System');
      return {
        lines: [...card.querySelectorAll('li')].map((node) => node.textContent),
        injectedMarkup: Boolean(card.querySelector('script, img')),
        documentOverflow: document.documentElement.scrollWidth - innerWidth,
        viewOverflow: document.getElementById('machine-view').scrollWidth
          - document.getElementById('machine-view').clientWidth,
        cardOverflow: card.scrollWidth - card.clientWidth,
      };
    })()`);
    assert.deepEqual(systemCard.lines, [
      "Uptime · 2d 3h 4m",
      `Kernel · ${LONG_KERNEL_VERSION}`,
      `OS · ${LONG_OS_VERSION}`,
    ]);
    assert.equal(systemCard.injectedMarkup, false);
    assert.ok(systemCard.documentOverflow <= 1, JSON.stringify(systemCard));
    assert.ok(systemCard.viewOverflow <= 1, JSON.stringify(systemCard));
    assert.ok(systemCard.cardOverflow <= 1, JSON.stringify(systemCard));
    await cdp.evaluate("document.getElementById('machine-mobile-back').click(); true");
    await cdp.evaluate("document.querySelector('.session-button[data-session-id=\"tron~%100\"]').click(); true");
    await waitFor(
      () => cdp.evaluate("document.getElementById('agent-name').textContent === 'codex-main'"),
      "original test agent did not reopen after machine telemetry",
    );

    await cdp.evaluate("document.getElementById('quick-actions-open').click(); true");
    await waitFor(
      () => cdp.evaluate("document.getElementById('quick-actions-dialog').open"),
      "mobile quick-actions popover did not open",
    );
    const quickActions = await cdp.evaluate(`({
      modelControl: !document.getElementById('quick-model-control').hidden,
      actions: [...document.querySelectorAll('#quick-actions-dialog .quick-actions-grid button')].map((button) => button.textContent),
      compactInComposer: document.getElementById('compact') !== null,
    })`);
    assert.equal(quickActions.modelControl, true, JSON.stringify(quickActions));
    assert.deepEqual(quickActions.actions, ["Duplicate agent", "Relaunch & resume", "Compact", "Ctrl+B ×2", "Interrupt", "Kill agent"]);
    assert.equal(quickActions.compactInComposer, false, JSON.stringify(quickActions));

    // A cached model observation must never authorize Duplicate when the
    // owner's live capability endpoint fails.
    failLiveModels = true;
    await cdp.evaluate("document.getElementById('quick-duplicate').click(); true");
    await waitFor(
      () => cdp.evaluate("document.getElementById('toast').textContent.includes('live model capability fixture failed')"),
      "Duplicate did not fail closed when the live model request failed",
    );
    assert.equal(await cdp.evaluate("document.getElementById('launch-dialog').open"), false);
    failLiveModels = false;

    // Starting ordinary Launch after a delayed Duplicate invalidates the old
    // request. Only the newest request may populate/show the shared dialog.
    launchOptionsDelayMs = 150;
    await cdp.evaluate("document.getElementById('quick-actions-open').click(); true");
    await cdp.evaluate("document.getElementById('quick-duplicate').click(); true");
    await cdp.evaluate("document.getElementById('launch-open').click(); true");
    await waitFor(
      () => cdp.evaluate("document.getElementById('launch-dialog').open && document.getElementById('launch-dialog-title').textContent === 'Launch agent'"),
      "newer ordinary Launch did not win the overlapping dialog requests",
    );
    await cdp.evaluate("document.querySelector('#launch-dialog .dialog-cancel').click(); true");

    // Even with a valid response token, the captured pane must still have the
    // same immutable launch identity after the GETs complete.
    launchOptionsDelayMs = 500;
    await cdp.evaluate("document.getElementById('quick-actions-open').click(); true");
    await cdp.evaluate("document.getElementById('quick-duplicate').click(); true");
    emitOverviewPatch([{
      id: "tron~%100", pane_id: "%100", machine: "tron", name: "codex-main",
      status: "waiting", agent: "codex", profile: "codex-max",
      path: "/workspace/changed", command: "codex",
    }]);
    await waitFor(
      () => cdp.evaluate("document.getElementById('agent-meta').title === '/workspace/changed'"),
      "source pane fixture did not change during the delayed Duplicate request",
    );
    await waitFor(
      () => cdp.evaluate("document.getElementById('toast').textContent.includes('source agent changed')"),
      "Duplicate accepted a source pane whose launch identity changed while loading",
    );
    assert.equal(await cdp.evaluate("document.getElementById('launch-dialog').open"), false);
    emitOverviewPatch([{
      id: "tron~%100", pane_id: "%100", machine: "tron", name: "codex-main",
      status: "waiting", agent: "codex", profile: "codex-max",
      path: "/workspace", command: "codex",
    }]);
    launchOptionsDelayMs = 0;
    await waitFor(
      () => cdp.evaluate("document.getElementById('agent-meta').title === '/workspace'"),
      "source pane fixture did not restore after the stale Duplicate check",
    );

    await cdp.evaluate("document.getElementById('quick-actions-open').click(); true");
    await cdp.evaluate("document.getElementById('quick-duplicate').click(); true");
    await waitFor(
      () => cdp.evaluate("document.getElementById('launch-dialog').open"),
      "mobile duplicate launcher did not open",
    );
    const duplicate = await cdp.evaluate(`({
      title: document.getElementById('launch-dialog-title').textContent,
      machine: document.getElementById('launch-machine').value,
      directory: document.getElementById('launch-directory').value,
      harness: document.getElementById('launch-harness').value,
      profile: document.getElementById('launch-profile').value,
      mode: document.getElementById('launch-mode').value,
      memory: document.getElementById('launch-memory').value,
      memoryOverflow: document.getElementById('launch-memory-group').scrollWidth
        > document.getElementById('launch-memory-group').clientWidth,
      name: document.getElementById('launch-name').value,
      conversation: document.getElementById('launch-session').value,
      submit: document.querySelector('#launch-form button[type=submit]').textContent,
    })`);
    assert.deepEqual(duplicate, {
      title: "Duplicate agent",
      machine: "tron",
      directory: "/workspace",
      harness: "codex",
      profile: "profile-codex-max",
      mode: "sol-fast",
      memory: "",
      memoryOverflow: false,
      name: "codex-main-copy",
      conversation: "",
      submit: "Launch duplicate",
    });

    const originalMemoryViewport = await cdp.evaluate("({ width: innerWidth, height: innerHeight })");
    const memorySelect = await cdp.evaluate(`(() => {
      const select = document.getElementById('launch-memory');
      select.focus({ preventScroll: true });
      const box = select.getBoundingClientRect();
      return {
        fontSize: parseFloat(getComputedStyle(select).fontSize),
        height: box.height,
        focused: document.activeElement === select,
        label: [...select.labels].map((node) => node.textContent).join(' '),
        documentOverflow: document.documentElement.scrollWidth - innerWidth,
      };
    })()`);
    assert.ok(memorySelect.fontSize >= 16, JSON.stringify(memorySelect));
    assert.ok(memorySelect.height >= 44, JSON.stringify(memorySelect));
    assert.equal(memorySelect.focused, true, JSON.stringify(memorySelect));
    assert.match(memorySelect.label, /Memory limit/);
    assert.ok(memorySelect.documentOverflow <= 1, JSON.stringify(memorySelect));

    await cdp.send("Emulation.setDeviceMetricsOverride", {
      width: 390, height: 430, deviceScaleFactor: 1, mobile: false,
    });
    await waitFor(
      () => cdp.evaluate("getComputedStyle(document.documentElement).getPropertyValue('--app-height').trim() === '430px'"),
      "focused memory select did not follow the keyboard-sized viewport",
    );
    await waitFor(
      () => cdp.evaluate(`(() => {
        const box = document.getElementById('launch-memory').getBoundingClientRect();
        return box.top >= 0 && box.bottom <= (window.visualViewport?.height || innerHeight) + 1;
      })()`),
      "focused memory select was not revealed inside the keyboard-sized viewport",
    );
    const keyboardSelect = await cdp.evaluate(`(() => {
      const select = document.getElementById('launch-memory');
      const box = select.getBoundingClientRect();
      return {
        viewport: window.visualViewport?.height || innerHeight,
        top: box.top, bottom: box.bottom, right: box.right,
        focused: document.activeElement === select,
        documentOverflow: document.documentElement.scrollWidth - innerWidth,
      };
    })()`);
    assert.equal(keyboardSelect.focused, true, JSON.stringify(keyboardSelect));
    assert.ok(keyboardSelect.top >= 0, JSON.stringify(keyboardSelect));
    assert.ok(keyboardSelect.bottom <= keyboardSelect.viewport + 1, JSON.stringify(keyboardSelect));
    assert.ok(keyboardSelect.right <= 390, JSON.stringify(keyboardSelect));
    assert.ok(keyboardSelect.documentOverflow <= 1, JSON.stringify(keyboardSelect));

    await cdp.evaluate(`(() => {
      const select = document.getElementById('launch-memory');
      select.value = 'custom';
      select.dispatchEvent(new Event('change', { bubbles: true }));
      const input = document.getElementById('launch-memory-custom');
      input.value = '20';
      input.dispatchEvent(new Event('input', { bubbles: true }));
      input.focus({ preventScroll: true });
      return true;
    })()`);
    await waitFor(
      () => cdp.evaluate(`(() => {
        const input = document.getElementById('launch-memory-custom');
        const box = input.getBoundingClientRect();
        return document.activeElement === input && box.top >= 0
          && box.bottom <= (window.visualViewport?.height || innerHeight) + 1;
      })()`),
      "focused custom memory input was not revealed inside the keyboard-sized viewport",
    );
    const customMemory = await cdp.evaluate(`(() => {
      const input = document.getElementById('launch-memory-custom');
      const box = input.getBoundingClientRect();
      return {
        visible: !document.getElementById('launch-memory-custom-row').hidden,
        fontSize: parseFloat(getComputedStyle(input).fontSize),
        height: box.height,
        focused: document.activeElement === input,
        label: [...input.labels].map((node) => node.textContent).join(' '),
        viewport: window.visualViewport?.height || innerHeight,
        top: box.top, bottom: box.bottom, right: box.right,
        documentOverflow: document.documentElement.scrollWidth - innerWidth,
        scrollHeight: document.documentElement.scrollHeight,
        clientHeight: document.documentElement.clientHeight,
      };
    })()`);
    assert.equal(customMemory.visible, true, JSON.stringify(customMemory));
    assert.ok(customMemory.fontSize >= 16, JSON.stringify(customMemory));
    assert.ok(customMemory.height >= 44, JSON.stringify(customMemory));
    assert.equal(customMemory.focused, true, JSON.stringify(customMemory));
    assert.match(customMemory.label, /Custom GiB/);
    assert.ok(customMemory.top >= 0, JSON.stringify(customMemory));
    assert.ok(customMemory.bottom <= customMemory.viewport + 1, JSON.stringify(customMemory));
    assert.ok(customMemory.right <= 390, JSON.stringify(customMemory));
    assert.ok(customMemory.documentOverflow <= 1, JSON.stringify(customMemory));
    assert.ok(customMemory.scrollHeight <= customMemory.clientHeight, JSON.stringify(customMemory));
    await cdp.send("Emulation.setDeviceMetricsOverride", {
      width: originalMemoryViewport.width, height: originalMemoryViewport.height,
      deviceScaleFactor: 1, mobile: false,
    });
    await waitFor(
      () => cdp.evaluate(`getComputedStyle(document.documentElement).getPropertyValue('--app-height').trim() === '${originalMemoryViewport.height}px'`),
      "memory controls did not restore after the keyboard-sized viewport",
    );
    await cdp.evaluate(`(() => {
      const select = document.getElementById('launch-memory');
      select.value = '';
      select.dispatchEvent(new Event('change', { bubbles: true }));
      return true;
    })()`);
    assert.equal(launchRequests.length, 0, "opening Duplicate must not launch or resume a session");
    assert.equal(launchSessionRequests.length, 0, "Duplicate must skip saved-session discovery");
    await cdp.evaluate(`(() => {
      const conversation = document.getElementById('launch-session');
      const forged = document.createElement('option');
      forged.value = 'saved-ffffffffffffffffffffffffffffffff';
      forged.textContent = 'forged saved conversation';
      forged.dataset.harness = 'codex';
      forged.dataset.preview = 'must not resume';
      conversation.append(forged);
      conversation.value = forged.value;
      document.getElementById('launch-form').requestSubmit();
      return true;
    })()`);
    await waitFor(() => launchRequests.length === 1, "explicit Duplicate submit was not observed");
    assert.equal(launchRequests[0].body.resume_session_id, null, JSON.stringify(launchRequests[0]));
    assert.equal(launchRequests[0].body.profile_id, "profile-codex-max");
    assert.equal(launchRequests[0].body.mode_id, "sol-fast");
    assert.equal(launchRequests[0].body.memory_max_bytes, null);
    await cdp.evaluate("document.querySelector('#launch-dialog .dialog-cancel').click(); true");
    launchRequests.length = 0;

    // Midnight's activity heuristic can alternate working/waiting on adjacent
    // samples. Agent buttons must keep their physical order and identity while
    // the visible status holds through short quiet gaps, or a mobile tap can
    // land on the row that jumped into the original target's coordinates.
    await cdp.evaluate("document.getElementById('mobile-back').click(); true");
    await waitFor(
      () => cdp.evaluate("!document.body.classList.contains('has-selection') && Boolean(document.querySelector('.session-button[data-session-id=\"midnight~%5\"]'))"),
      "agent menu did not open for the Midnight status regression",
    );
    const beforeOscillation = await cdp.evaluate(`(() => {
      const alpha = document.querySelector('.session-button[data-session-id="midnight~%5"]');
      const beta = document.querySelector('.session-button[data-session-id="midnight~%7"]');
      beta.scrollIntoView({ block: 'center' });
      window.__midnightAlphaNode = alpha;
      window.__midnightBetaNode = beta;
      const bounds = beta.getBoundingClientRect();
      const rail = document.querySelector('.rail');
      return {
        order: [...document.querySelectorAll('.session-button[data-session-id^="midnight~"]')].map((node) => node.dataset.sessionId),
        betaX: bounds.left + bounds.width / 2,
        betaY: bounds.top + bounds.height / 2,
        windowY: window.scrollY,
        railY: rail.scrollTop,
      };
    })()`);
    assert.deepEqual(beforeOscillation.order, ["midnight~%5", "midnight~%7"]);

    for (const statuses of [
      ["waiting", "working"],
      ["working", "waiting"],
      ["waiting", "working"],
    ]) {
      emitOverviewPatch([
        mockSession("midnight", "%5", "alpha-planner", statuses[0]),
        mockSession("midnight", "%7", "beta-planner", statuses[1]),
      ]);
      await new Promise((resolveDelay) => setTimeout(resolveDelay, 150));
    }
    await waitFor(
      () => cdp.evaluate(`(() => {
        const rows = [...document.querySelectorAll('.session-button[data-session-id^="midnight~"]')];
        return rows.length === 2 && rows.every((node) => node.classList.contains('working'));
      })()`),
      "brief Midnight quiet samples were not held as working",
    );
    const duringOscillation = await cdp.evaluate(`(() => {
      const beta = document.querySelector('.session-button[data-session-id="midnight~%7"]');
      const bounds = beta.getBoundingClientRect();
      const rail = document.querySelector('.rail');
      return {
        order: [...document.querySelectorAll('.session-button[data-session-id^="midnight~"]')].map((node) => node.dataset.sessionId),
        sameAlpha: window.__midnightAlphaNode === document.querySelector('.session-button[data-session-id="midnight~%5"]'),
        sameBeta: window.__midnightBetaNode === beta,
        betaX: bounds.left + bounds.width / 2,
        betaY: bounds.top + bounds.height / 2,
        windowY: window.scrollY,
        railY: rail.scrollTop,
      };
    })()`);
    assert.deepEqual(duringOscillation.order, beforeOscillation.order);
    assert.equal(duringOscillation.sameAlpha, true);
    assert.equal(duringOscillation.sameBeta, true);
    assert.ok(Math.abs(duringOscillation.betaX - beforeOscillation.betaX) <= 1, JSON.stringify({ beforeOscillation, duringOscillation }));
    assert.ok(Math.abs(duringOscillation.betaY - beforeOscillation.betaY) <= 1, JSON.stringify({ beforeOscillation, duringOscillation }));
    assert.equal(duringOscillation.windowY, beforeOscillation.windowY);
    assert.equal(duringOscillation.railY, beforeOscillation.railY);

    await waitFor(
      () => cdp.evaluate(`document.querySelector('.session-button[data-session-id="midnight~%5"]').classList.contains('waiting')`),
      "a continuous Midnight quiet period did not become waiting",
      4_000,
    );
    await cdp.send("Input.dispatchMouseEvent", {
      type: "mousePressed", x: beforeOscillation.betaX, y: beforeOscillation.betaY,
      button: "left", buttons: 1, clickCount: 1,
    });
    await cdp.send("Input.dispatchMouseEvent", {
      type: "mouseReleased", x: beforeOscillation.betaX, y: beforeOscillation.betaY,
      button: "left", buttons: 0, clickCount: 1,
    });
    await waitFor(
      () => cdp.evaluate("document.getElementById('agent-name').textContent === 'beta-planner'"),
      "tap at beta-planner's stable coordinates selected a different agent",
    );
    assert.equal(await cdp.evaluate("new URL(location.href).searchParams.get('session')"), "midnight~%7");
    await cdp.evaluate("document.getElementById('mobile-back').click(); true");
    await waitFor(
      () => cdp.evaluate("!document.body.classList.contains('has-selection')"),
      "agent menu did not reopen after the Midnight tap regression",
    );
    await cdp.evaluate("document.querySelector('.session-button[data-session-id=\"tron~%100\"]').click(); true");
    await waitFor(
      () => cdp.evaluate("document.getElementById('agent-name').textContent === 'codex-main'"),
      "original test agent did not reopen",
    );

    // Files and Git share the terminal band without moving the composer. On a
    // phone they drill from list to source/diff, keep internal scroll, and
    // never interpret owner-provided names or source as markup.
    const projectTabs = await cdp.evaluate(`({
      labels: [...document.querySelectorAll('.view-switch [role="tab"]')].map((tab) => tab.textContent),
      composerTop: document.getElementById('composer').getBoundingClientRect().top,
      bodyOverflowX: document.documentElement.scrollWidth - innerWidth,
    })`);
    assert.deepEqual(projectTabs.labels, ["Conversation", "Raw pane", "Files", "Git"]);
    assert.equal(projectTabs.bodyOverflowX, 0);
    await cdp.evaluate("document.getElementById('files-view').click(); true");
    await waitFor(
      () => cdp.evaluate("document.querySelectorAll('#files-list .project-entry').length === 3"),
      "project root did not load lazily",
    );
    assert.equal(await cdp.evaluate("Boolean(document.querySelector('#files-panel img, #files-panel script'))"), false);
    await cdp.evaluate(`(() => {
      [...document.querySelectorAll('#files-list .project-entry')].find((entry) => entry.textContent.includes('image.bin')).click();
      return true;
    })()`);
    await waitFor(
      () => cdp.evaluate("document.getElementById('file-viewer').textContent.includes('binary or unsupported')"),
      "binary file did not render an explicit unsupported state",
    );
    assert.equal(await cdp.evaluate("document.querySelectorAll('#file-viewer .code-line').length"), 0);
    await cdp.evaluate("document.querySelector('#file-viewer .project-viewer-back').click(); true");
    await cdp.evaluate(`(() => {
      [...document.querySelectorAll('#files-list .project-entry')].find((entry) => entry.textContent.includes('src')).click();
      return true;
    })()`);
    await waitFor(
      () => cdp.evaluate("document.querySelector('#files-list .project-entry')?.textContent.includes('app.js')"),
      "file breadcrumb navigation did not load src",
    );
    await cdp.evaluate("document.querySelector('#files-list .project-entry').click(); true");
    await waitFor(
      () => cdp.evaluate("document.querySelectorAll('#file-viewer .code-line').length === 320"),
      "source preview did not render",
    );
    const mobileFileDefaults = await cdp.evaluate(`(() => {
      const viewer = document.getElementById('file-viewer');
      const source = viewer.querySelector('.code-source');
      const line = viewer.querySelector('.code-line-content');
      const head = viewer.querySelector('.code-viewer-head');
      const controls = viewer.querySelector('.file-display-controls');
      return {
        wrap: viewer.querySelector('.file-wrap-toggle').getAttribute('aria-pressed'),
        size: viewer.querySelector('.file-text-size').value,
        fontSize: getComputedStyle(source).fontSize,
        lineWhiteSpace: getComputedStyle(line).whiteSpace,
        sourceOverflow: source.scrollWidth - viewer.clientWidth,
        documentOverflow: document.documentElement.scrollWidth - innerWidth,
        controlsInHeader: controls.closest('.code-viewer-head') === head,
        wrapLabel: viewer.querySelector('.file-wrap-toggle').getAttribute('title'),
        sizeLabel: viewer.querySelector('.file-text-size').getAttribute('aria-label'),
      };
    })()`);
    assert.deepEqual(mobileFileDefaults, {
      wrap: "true",
      size: "small",
      fontSize: "10.5px",
      lineWhiteSpace: "pre-wrap",
      sourceOverflow: 0,
      documentOverflow: 0,
      controlsInHeader: true,
      wrapLabel: "Wrap long file lines",
      sizeLabel: "File text size",
    });
    const noWrapFile = await cdp.evaluate(`(() => {
      const viewer = document.getElementById('file-viewer');
      viewer.querySelector('.file-wrap-toggle').click();
      const size = viewer.querySelector('.file-text-size');
      size.value = 'large';
      size.dispatchEvent(new Event('change', { bubbles: true }));
      const source = viewer.querySelector('.code-source');
      return {
        wrap: viewer.querySelector('.file-wrap-toggle').getAttribute('aria-pressed'),
        size: size.value,
        fontSize: getComputedStyle(source).fontSize,
        sourceOverflow: source.scrollWidth - viewer.clientWidth,
        documentOverflow: document.documentElement.scrollWidth - innerWidth,
        stored: localStorage.getItem('atmux.file-reader-preferences'),
      };
    })()`);
    assert.equal(noWrapFile.wrap, "false", JSON.stringify(noWrapFile));
    assert.equal(noWrapFile.size, "large", JSON.stringify(noWrapFile));
    assert.equal(noWrapFile.fontSize, "15px", JSON.stringify(noWrapFile));
    assert.ok(noWrapFile.sourceOverflow > 0, JSON.stringify(noWrapFile));
    assert.equal(noWrapFile.documentOverflow, 0, JSON.stringify(noWrapFile));
    assert.equal(noWrapFile.stored, '{"wrap":false,"size":"large"}');
    await cdp.evaluate(`(() => {
      const viewer = document.getElementById('file-viewer');
      viewer.querySelector('.file-wrap-toggle').click();
      const size = viewer.querySelector('.file-text-size');
      size.value = 'small';
      size.dispatchEvent(new Event('change', { bubbles: true }));
      return true;
    })()`);
    const messagesBeforeReference = messageRequests.length;
    const referenceState = await cdp.evaluate(`(() => {
      const viewer = document.getElementById('file-viewer');
      viewer.scrollTop = 540;
      viewer.scrollLeft = 80;
      const before = { top: viewer.scrollTop, left: viewer.scrollLeft, outer: scrollY };
      const lines = viewer.querySelectorAll('button.code-line-number');
      lines[4].click();
      lines[6].click();
      const input = document.getElementById('message');
      input.value = 'Please inspect';
      input.setSelectionRange(input.value.length, input.value.length);
      viewer.querySelector('.file-reference').click();
      return new Promise((resolve) => requestAnimationFrame(() => resolve({
        message: input.value,
        focused: document.activeElement === input,
        top: viewer.scrollTop,
        left: viewer.scrollLeft,
        outer: scrollY,
        before,
      })));
    })()`);
    const referenceLines = fixtureProjectFile("tron~%100").split("\n").slice(4, 7).join("\n");
    assert.equal(
      referenceState.message,
      `Please inspect\n\nSelected \`src/app.js:5-7\`:\n\n\`\`\`javascript\n${referenceLines}\n\`\`\``,
    );
    assert.equal(referenceState.focused, true, JSON.stringify(referenceState));
    assert.equal(referenceState.top, referenceState.before.top, JSON.stringify(referenceState));
    assert.equal(referenceState.left, referenceState.before.left, JSON.stringify(referenceState));
    assert.equal(referenceState.outer, referenceState.before.outer, JSON.stringify(referenceState));
    await new Promise((resolveWait) => setTimeout(resolveWait, 100));
    assert.equal(messageRequests.length, messagesBeforeReference, "referencing source must not POST a message");

    const mobileEditorEntry = await cdp.evaluate(`(() => {
      document.querySelector('#file-viewer .file-edit').click();
      const viewer = document.getElementById('file-viewer');
      const editor = viewer.querySelector('.file-editor');
      const size = viewer.querySelector('.file-text-size');
      const fonts = {};
      for (const value of ['small', 'medium', 'large']) {
        size.value = value;
        size.dispatchEvent(new Event('change', { bubbles: true }));
        fonts[value] = getComputedStyle(editor).fontSize;
      }
      size.value = 'small';
      size.dispatchEvent(new Event('change', { bubbles: true }));
      editor.focus({ preventScroll: true });
      return {
        fonts,
        focused: document.activeElement === editor,
        transform: getComputedStyle(editor).transform,
        wrap: editor.getAttribute('wrap'),
        documentOverflow: document.documentElement.scrollWidth - innerWidth,
        editorOverflow: editor.scrollWidth - editor.clientWidth,
      };
    })()`);
    assert.deepEqual(mobileEditorEntry.fonts, {
      small: "16px", medium: "17px", large: "19px",
    });
    assert.equal(mobileEditorEntry.focused, true, JSON.stringify(mobileEditorEntry));
    assert.equal(mobileEditorEntry.transform, "none", JSON.stringify(mobileEditorEntry));
    assert.equal(mobileEditorEntry.wrap, "soft", JSON.stringify(mobileEditorEntry));
    assert.equal(mobileEditorEntry.documentOverflow, 0, JSON.stringify(mobileEditorEntry));
    assert.equal(mobileEditorEntry.editorOverflow, 0, JSON.stringify(mobileEditorEntry));

    const originalEditorViewport = await cdp.evaluate("({ width: innerWidth, height: innerHeight })");
    await cdp.send("Emulation.setDeviceMetricsOverride", {
      width: 390, height: 430, deviceScaleFactor: 1, mobile: false,
    });
    await waitFor(
      () => cdp.evaluate("getComputedStyle(document.documentElement).getPropertyValue('--app-height').trim() === '430px'"),
      "focused file editor did not follow the keyboard-sized viewport",
    );
    const keyboardSizedEditor = await cdp.evaluate(`(() => {
      const viewer = document.getElementById('file-viewer');
      const editor = viewer.querySelector('.file-editor');
      const box = viewer.getBoundingClientRect();
      return {
        viewport: window.visualViewport?.height || innerHeight,
        viewerTop: box.top,
        viewerBottom: box.bottom,
        viewerRight: box.right,
        fontSize: getComputedStyle(editor).fontSize,
        focused: document.activeElement === editor,
        documentOverflow: document.documentElement.scrollWidth - innerWidth,
        scrollHeight: document.documentElement.scrollHeight,
        clientHeight: document.documentElement.clientHeight,
      };
    })()`);
    assert.equal(keyboardSizedEditor.fontSize, "16px", JSON.stringify(keyboardSizedEditor));
    assert.equal(keyboardSizedEditor.focused, true, JSON.stringify(keyboardSizedEditor));
    assert.ok(keyboardSizedEditor.viewerTop >= 0, JSON.stringify(keyboardSizedEditor));
    assert.ok(keyboardSizedEditor.viewerBottom <= keyboardSizedEditor.viewport + 1, JSON.stringify(keyboardSizedEditor));
    assert.ok(keyboardSizedEditor.viewerRight <= 390, JSON.stringify(keyboardSizedEditor));
    assert.equal(keyboardSizedEditor.documentOverflow, 0, JSON.stringify(keyboardSizedEditor));
    assert.ok(keyboardSizedEditor.scrollHeight <= keyboardSizedEditor.clientHeight, JSON.stringify(keyboardSizedEditor));
    await cdp.send("Emulation.setDeviceMetricsOverride", {
      width: originalEditorViewport.width, height: originalEditorViewport.height,
      deviceScaleFactor: 1, mobile: false,
    });
    await waitFor(
      () => cdp.evaluate(`getComputedStyle(document.documentElement).getPropertyValue('--app-height').trim() === '${originalEditorViewport.height}px'`),
      "file editor did not restore after the keyboard-sized viewport",
    );

    // Every route that would drop a dirty editor asks first. Cancelling keeps
    // the exact file, tab, and agent selected; accepting once discards it.
    await cdp.evaluate(`(() => {
      const editor = document.querySelector('#file-viewer .file-editor');
      editor.value += '\\n// guarded draft';
      editor.dispatchEvent(new Event('input', { bubbles: true }));
      window.__discardPrompts = [];
      window.confirm = (message) => { window.__discardPrompts.push(message); return false; };
      document.querySelector('#file-viewer .project-viewer-back').click();
      document.querySelector('#files-breadcrumbs button').click();
      document.getElementById('conversation-view').click();
      document.getElementById('mobile-back').click();
      return {
        editor: document.querySelector('#file-viewer .file-editor').value,
        editorFontSize: getComputedStyle(document.querySelector('#file-viewer .file-editor')).fontSize,
        editorWrap: document.querySelector('#file-viewer .file-editor').getAttribute('wrap'),
        persistedSize: document.querySelector('#file-viewer .file-text-size').value,
        persistedWrap: document.querySelector('#file-viewer .file-wrap-toggle').getAttribute('aria-pressed'),
        mode: document.getElementById('files-view').getAttribute('aria-selected'),
        agent: document.getElementById('agent-name').textContent,
        prompts: window.__discardPrompts,
      };
    })()`).then((guarded) => {
      assert.match(guarded.editor, /\/\/ guarded draft$/);
      assert.equal(guarded.editorFontSize, "16px", JSON.stringify(guarded));
      assert.equal(guarded.editorWrap, "soft", JSON.stringify(guarded));
      assert.equal(guarded.persistedSize, "small", JSON.stringify(guarded));
      assert.equal(guarded.persistedWrap, "true", JSON.stringify(guarded));
      assert.equal(guarded.mode, "true");
      assert.equal(guarded.agent, "codex-main");
      assert.equal(guarded.prompts.length, 4, JSON.stringify(guarded));
    });
    const editorOrigin = await cdp.evaluate("location.origin");
    await cdp.evaluate(`(() => {
      window.confirm = (message) => { window.__discardPrompts.push(message); return false; };
      history.back();
      return true;
    })()`);
    await waitFor(
      () => cdp.evaluate("window.__discardPrompts.length === 5 && document.querySelector('#file-viewer .file-editor')?.value.includes('// guarded draft')"),
      "rejected browser Back did not restore the exact dirty editor",
    );
    const rejectedBack = await cdp.evaluate(`({
      origin: location.origin,
      session: new URL(location.href).searchParams.get('session'),
      editor: document.querySelector('#file-viewer .file-editor').value,
      filesTab: document.getElementById('files-view').getAttribute('aria-selected'),
      agent: document.getElementById('agent-name').textContent,
    })`);
    assert.equal(rejectedBack.origin, editorOrigin, JSON.stringify(rejectedBack));
    assert.equal(rejectedBack.session, "tron~%100", JSON.stringify(rejectedBack));
    assert.match(rejectedBack.editor, /\/\/ guarded draft$/);
    assert.equal(rejectedBack.filesTab, "true");
    assert.equal(rejectedBack.agent, "codex-main");

    await cdp.evaluate(`(() => { window.confirm = () => true; history.back(); return true; })()`);
    await waitFor(
      () => cdp.evaluate("!document.body.classList.contains('has-selection')"),
      "accepted browser Back did not land on the in-app Agents menu",
    );
    const acceptedBack = await cdp.evaluate(`({
      origin: location.origin,
      session: new URL(location.href).searchParams.get('session'),
      menuVisible: getComputedStyle(document.getElementById('session-rail')).display !== 'none',
      external: !location.href.startsWith(location.origin),
    })`);
    assert.equal(acceptedBack.origin, editorOrigin, JSON.stringify(acceptedBack));
    assert.equal(acceptedBack.session, null, JSON.stringify(acceptedBack));
    assert.equal(acceptedBack.menuVisible, true, JSON.stringify(acceptedBack));
    assert.equal(acceptedBack.external, false, JSON.stringify(acceptedBack));

    await cdp.evaluate("document.querySelector('.session-button[data-session-id=\"tron~%100\"]').click(); true");
    await waitFor(() => cdp.evaluate("document.getElementById('agent-name').textContent === 'codex-main'"), "agent did not reopen after accepted browser Back");
    await waitFor(() => cdp.evaluate("document.querySelectorAll('#files-list .project-entry').length === 3"), "project root did not reload after browser Back");
    await cdp.evaluate(`(() => { [...document.querySelectorAll('#files-list .project-entry')].find((entry) => entry.textContent.includes('src')).click(); return true; })()`);
    await waitFor(() => cdp.evaluate("document.querySelector('#files-list .project-entry')?.textContent.includes('app.js')"), "src folder did not reopen after browser Back");
    await cdp.evaluate("document.querySelector('#files-list .project-entry').click(); true");
    await waitFor(() => cdp.evaluate("document.querySelectorAll('#file-viewer .code-line').length === 320"), "file did not reopen after browser Back discard");

    // A delayed PUT snapshots exactly what it sends. Typing while it is in
    // flight remains in the editor after success, while the fresh response
    // hash becomes the base for the next Save.
    delayFileSavePane = "tron~%100";
    await cdp.evaluate(`(() => {
      document.querySelector('#file-viewer .file-edit').click();
      const editor = document.querySelector('#file-viewer .file-editor');
      editor.value += '\\n// sent edit';
      editor.dispatchEvent(new Event('input', { bubbles: true }));
      document.querySelector('#file-viewer .file-save').click();
      return true;
    })()`);
    await waitFor(() => delayFileSavePane === null, "delayed file save did not reach the owner");
    await cdp.evaluate(`(() => {
      const editor = document.querySelector('#file-viewer .file-editor');
      editor.value += '\\n// newer while saving';
      editor.dispatchEvent(new Event('input', { bubbles: true }));
      return true;
    })()`);
    await waitFor(
      () => cdp.evaluate("document.querySelector('#file-viewer .file-editor')?.value.includes('// newer while saving') && !document.querySelector('#file-viewer .file-save').disabled"),
      "newer typing was lost when the delayed Save completed",
    );
    assert.equal(fileSaveRequests.at(-1).body.expected_hash, "1".repeat(64));
    assert.match(fileSaveRequests.at(-1).body.content, /\/\/ sent edit$/);
    assert.doesNotMatch(fileSaveRequests.at(-1).body.content, /newer while saving/);
    await cdp.evaluate("document.querySelector('#file-viewer .file-save').click(); true");
    await waitFor(
      () => cdp.evaluate("Boolean(document.querySelector('#file-viewer .file-edit')) && document.getElementById('file-viewer').textContent.includes('// newer while saving')"),
      "follow-up save did not commit the preserved newer draft",
    );
    assert.equal(fileSaveRequests.at(-1).body.expected_hash, "2".repeat(64));
    assert.match(fileSaveRequests.at(-1).body.content, /\/\/ newer while saving$/);

    // A 409 remains sticky through further typing and disables Save against
    // the stale hash. Reload is explicit, cancellable, and obtains a new base.
    nextFileSaveConflict = true;
    await cdp.evaluate(`(() => {
      document.querySelector('#file-viewer .file-edit').click();
      const editor = document.querySelector('#file-viewer .file-editor');
      editor.value += '\\n// conflict draft';
      editor.dispatchEvent(new Event('input', { bubbles: true }));
      document.querySelector('#file-viewer .file-save').click();
      return true;
    })()`);
    await waitFor(
      () => cdp.evaluate("document.querySelector('#file-viewer .file-edit-status')?.textContent.includes('Conflict:')"),
      "409 save did not preserve an explicit conflict state",
    );
    const conflictState = await cdp.evaluate(`(() => {
      const editor = document.querySelector('#file-viewer .file-editor');
      editor.value += '\\n// typed after 409';
      editor.dispatchEvent(new Event('input', { bubbles: true }));
      window.confirm = () => false;
      document.querySelector('#file-viewer .file-cancel').click();
      document.querySelector('#file-viewer .file-reload').click();
      return {
        draft: editor.value,
        saveDisabled: document.querySelector('#file-viewer .file-save').disabled,
        conflict: document.querySelector('#file-viewer .file-edit-status').textContent,
        reloadVisible: Boolean(document.querySelector('#file-viewer .file-reload')),
      };
    })()`);
    assert.match(conflictState.draft, /\/\/ typed after 409$/);
    assert.equal(conflictState.saveDisabled, true);
    assert.equal(conflictState.reloadVisible, true);
    assert.match(conflictState.conflict, /Reload latest/);
    assert.equal(await cdp.evaluate("document.querySelector('#file-viewer .file-editor').value.includes('// typed after 409')"), true);
    await cdp.evaluate(`(() => {
      window.confirm = () => true;
      document.querySelector('#file-viewer .file-reload').click();
      return true;
    })()`);
    await waitFor(
      () => cdp.evaluate("!document.querySelector('#file-viewer .file-editor') && document.getElementById('file-viewer').textContent.includes('// external edit')"),
      "confirmed conflict reload did not fetch the owner's latest file",
    );
    await cdp.evaluate(`(() => {
      document.querySelector('#file-viewer .file-edit').click();
      const editor = document.querySelector('#file-viewer .file-editor');
      editor.value += '\\n// after reload';
      editor.dispatchEvent(new Event('input', { bubbles: true }));
      document.querySelector('#file-viewer .file-save').click();
      return true;
    })()`);
    await waitFor(() => cdp.evaluate("document.getElementById('file-viewer').textContent.includes('// after reload') && !document.querySelector('#file-viewer .file-editor')"), "post-reload save did not complete");
    assert.equal(fileSaveRequests.at(-1).body.expected_hash, "4".repeat(64));
    await cdp.evaluate("document.getElementById('message').value = ''; true");
    const filesBeforeStatus = await cdp.evaluate(`(() => {
      const viewer = document.getElementById('file-viewer');
      viewer.scrollTop = 720;
      viewer.scrollLeft = 120;
      viewer.dispatchEvent(new Event('scroll'));
      return {
        top: viewer.scrollTop, left: viewer.scrollLeft,
        internalY: viewer.scrollHeight > viewer.clientHeight,
        internalX: viewer.scrollWidth > viewer.clientWidth,
        outerY: scrollY,
        source: viewer.textContent,
      };
    })()`);
    assert.equal(filesBeforeStatus.internalY, true);
    assert.equal(filesBeforeStatus.internalX, false);
    assert.equal(filesBeforeStatus.left, 0);
    assert.ok(filesBeforeStatus.source.includes('<script>safe 1</script>'));
    assert.equal(await cdp.evaluate("Boolean(document.querySelector('#file-viewer script, #file-viewer img'))"), false);
    emitOverviewPatch([{
      id: "tron~%100", pane_id: "%100", machine: "tron", name: "codex-main",
      status: "working", agent: "codex", profile: "codex-max", path: "/workspace", command: "codex",
    }]);
    await new Promise((resolveWait) => setTimeout(resolveWait, 150));
    const filesAfterStatus = await cdp.evaluate(`({
      top: document.getElementById('file-viewer').scrollTop,
      left: document.getElementById('file-viewer').scrollLeft,
      outerY: scrollY,
      composerTop: document.getElementById('composer').getBoundingClientRect().top,
    })`);
    assert.equal(filesAfterStatus.top, filesBeforeStatus.top);
    assert.equal(filesAfterStatus.left, filesBeforeStatus.left);
    assert.equal(filesAfterStatus.outerY, filesBeforeStatus.outerY);
    assert.equal(filesAfterStatus.composerTop, projectTabs.composerTop);

    await cdp.evaluate("document.getElementById('git-view').click(); true");
    await waitFor(
      () => cdp.evaluate("document.querySelectorAll('#git-changes .git-change').length === 2"),
      "Git status did not load lazily",
    );
    const gitSummary = await cdp.evaluate(`({
      branch: document.querySelector('.git-branch').textContent,
      rename: document.querySelectorAll('.git-change-path')[1].textContent,
      selected: document.getElementById('git-view').getAttribute('aria-selected'),
      hasInjectedMarkup: Boolean(document.querySelector('#git-panel script, #git-panel img')),
    })`);
    assert.equal(gitSummary.branch, "feature/tron~%100/<script>alert(1)</script>");
    assert.equal(gitSummary.rename, "old name.js → new #name.js");
    assert.equal(gitSummary.selected, "true");
    assert.equal(gitSummary.hasInjectedMarkup, false);
    await cdp.evaluate("document.querySelector('#git-changes .git-change').click(); true");
    await waitFor(
      () => cdp.evaluate("document.querySelectorAll('#git-diff .code-line').length >= 4"),
      "unified diff did not render",
    );
    assert.equal(await cdp.evaluate("Boolean(document.querySelector('#git-diff .diff-line-added') && document.querySelector('#git-diff .diff-line-removed') && document.querySelector('#git-diff .diff-line-hunk'))"), true);
    assert.equal(await cdp.evaluate("Boolean(document.querySelector('#git-diff script, #git-diff img'))"), false);

    await cdp.evaluate("document.getElementById('files-view').click(); true");
    await new Promise((resolveWait) => setTimeout(resolveWait, 50));
    const restoredFile = await cdp.evaluate(`({
      top: document.getElementById('file-viewer').scrollTop,
      left: document.getElementById('file-viewer').scrollLeft,
      selected: document.getElementById('files-view').getAttribute('aria-selected'),
      bodyOverflowX: document.documentElement.scrollWidth - innerWidth,
    })`);
    assert.equal(restoredFile.top, filesBeforeStatus.top);
    assert.equal(restoredFile.left, filesBeforeStatus.left);
    assert.equal(restoredFile.selected, "true");
    assert.equal(restoredFile.bodyOverflowX, 0);

    await cdp.evaluate("document.getElementById('mobile-back').click(); true");
    await waitFor(() => cdp.evaluate("!document.body.classList.contains('has-selection')"), "Files test did not return to agent list");
    delayGitSummaryPane = "midnight~%5";
    const filesClearedOnPaneChange = await cdp.evaluate(`(() => {
      document.querySelector('.session-button[data-session-id="midnight~%5"]').click();
      return !document.getElementById('file-viewer').textContent.includes('tron~%100');
    })()`);
    assert.equal(filesClearedOnPaneChange, true, "pane A source remained visible under pane B");
    const branchDuringPaneSwitch = await cdp.evaluate(`({
      hidden: document.getElementById('agent-branch').hidden,
      text: document.getElementById('agent-branch').textContent,
    })`);
    assert.equal(branchDuringPaneSwitch.hidden, true, JSON.stringify(branchDuringPaneSwitch));
    assert.equal(branchDuringPaneSwitch.text.includes("tron~%100"), false, JSON.stringify(branchDuringPaneSwitch));
    await waitFor(
      () => cdp.evaluate("document.getElementById('agent-branch').textContent.includes('feature/midnight~%5')"),
      "pane B branch did not replace pane A after the delayed owner response",
    );
    await waitFor(
      () => cdp.evaluate("document.querySelectorAll('#files-list .project-entry').length === 3"),
      "pane B project root did not load",
    );
    await cdp.evaluate(`(() => { [...document.querySelectorAll('#files-list .project-entry')].find((entry) => entry.textContent.includes('src')).click(); return true; })()`);
    await waitFor(() => cdp.evaluate("document.querySelector('#files-list .project-entry')?.textContent.includes('app.js')"), "pane B src did not load");
    delayProjectFilePane = "midnight~%5";
    await cdp.evaluate("document.querySelector('#files-list .project-entry').click(); true");
    await waitFor(() => delayProjectFilePane === null, "delayed file request did not reach server");
    await cdp.evaluate("document.getElementById('conversation-view').click(); document.getElementById('files-view').click(); true");
    await new Promise((resolveWait) => setTimeout(resolveWait, 350));
    const abortedFileState = await cdp.evaluate(`({
      loading: document.getElementById('file-viewer').textContent.includes('Loading file'),
      stale: document.getElementById('file-viewer').textContent.includes('midnight~%5'),
      prompt: document.getElementById('file-viewer').textContent.includes('Choose a file'),
    })`);
    assert.deepEqual(abortedFileState, { loading: false, stale: false, prompt: true });
    await cdp.evaluate("document.querySelector('#files-list .project-entry').click(); true");
    await waitFor(() => cdp.evaluate("document.getElementById('file-viewer').textContent.includes('midnight~%5')"), "pane B file did not reload after abort");

    await cdp.evaluate("document.getElementById('git-view').click(); true");
    await waitFor(() => cdp.evaluate("document.querySelectorAll('#git-changes .git-change').length === 2"), "pane B Git status did not load");
    delayGitDiffPane = "midnight~%5";
    await cdp.evaluate("document.querySelector('#git-changes .git-change').click(); true");
    await waitFor(() => delayGitDiffPane === null, "delayed Git request did not reach server");
    await cdp.evaluate("document.getElementById('conversation-view').click(); document.getElementById('git-view').click(); true");
    await new Promise((resolveWait) => setTimeout(resolveWait, 350));
    const abortedGitState = await cdp.evaluate(`({
      loading: document.getElementById('git-diff').textContent.includes('Loading diff'),
      stale: document.getElementById('git-diff').textContent.includes('const safe'),
      prompt: document.getElementById('git-diff').textContent.includes('Choose a changed file'),
    })`);
    assert.deepEqual(abortedGitState, { loading: false, stale: false, prompt: true });

    await cdp.evaluate("document.getElementById('mobile-back').click(); true");
    await waitFor(() => cdp.evaluate("!document.body.classList.contains('has-selection')"), "Git test did not return to agent list");
    const gitClearedOnPaneChange = await cdp.evaluate(`(() => {
      document.querySelector('.session-button[data-session-id="midnight~%7"]').click();
      return !document.getElementById('git-summary').textContent.includes('midnight~%5');
    })()`);
    assert.equal(gitClearedOnPaneChange, true, "pane A Git data remained visible under pane B");
    await waitFor(() => cdp.evaluate("document.querySelector('.git-branch')?.textContent.includes('midnight~%7')"), "pane B Git status did not replace pane A");
    await cdp.evaluate("document.getElementById('mobile-back').click(); true");
    await waitFor(() => cdp.evaluate("!document.body.classList.contains('has-selection')"), "Git pane B did not return to menu");
    await cdp.evaluate("document.querySelector('.session-button[data-session-id=\"tron~%100\"]').click(); true");
    await waitFor(() => cdp.evaluate("document.querySelector('.git-branch')?.textContent.includes('tron~%100')"), "original pane Git status did not reload");
    await cdp.evaluate("document.getElementById('conversation-view').click(); true");

    // Claude can publish process metadata before its first native log. The
    // unavailable view must remain a safe Raw fallback, then map on a later
    // poll without requiring the user to reselect the pane.
    await waitFor(
      () => cdp.evaluate("document.getElementById('conversation').textContent.includes('No agent session log is mapped yet')"),
      "delayed native conversation did not retain the Raw fallback",
    );
    transcriptFixture = transcript(0, 80, "first-transcript");
    await waitFor(
      () => cdp.evaluate("document.querySelectorAll('#conversation [data-transcript-id]').length === 80"),
      "delayed native conversation did not map on a later poll",
      5_000,
    );

    // A bounded transcript normally drops old cards while fresh agent output
    // arrives. Keep the same visible message anchored rather than preserving
    // only its old pixel position (which changes when leading cards vanish).
    const readingAnchor = await cdp.evaluate(`(() => {
      const conversation = document.getElementById('conversation');
      const target = [...conversation.querySelectorAll('[data-transcript-id]')]
        .find((node) => node.dataset.transcriptId === 'message-30');
      const bounds = conversation.getBoundingClientRect();
      conversation.scrollTop += target.getBoundingClientRect().top - bounds.top - 12;
      conversation.dispatchEvent(new Event('scroll'));
      const visible = [...conversation.querySelectorAll('[data-transcript-id]')]
        .find((node) => node.getBoundingClientRect().bottom > bounds.top);
      const terminal = document.querySelector('.terminal-shell').getBoundingClientRect();
      const composer = document.getElementById('composer').getBoundingClientRect();
      return {
        id: visible.dataset.transcriptId,
        offset: visible.getBoundingClientRect().top - bounds.top,
        windowY: window.scrollY,
        terminalTop: terminal.top,
        composerTop: composer.top,
      };
    })()`);
    transcriptFixture = transcript(10, 80, "second-transcript");
    await waitFor(
      () => cdp.evaluate("document.querySelector('[data-transcript-id=\"message-89\"]') !== null"),
      "streamed transcript update did not render",
      5_000,
    );
    const anchoredAfterOutput = await cdp.evaluate(`(() => {
      const conversation = document.getElementById('conversation');
      const bounds = conversation.getBoundingClientRect();
      const visible = [...conversation.querySelectorAll('[data-transcript-id]')]
        .find((node) => node.getBoundingClientRect().bottom > bounds.top);
      const terminal = document.querySelector('.terminal-shell').getBoundingClientRect();
      const composer = document.getElementById('composer').getBoundingClientRect();
      return {
        id: visible.dataset.transcriptId,
        offset: visible.getBoundingClientRect().top - bounds.top,
        windowY: window.scrollY,
        terminalTop: terminal.top,
        composerTop: composer.top,
      };
    })()`);
    assert.equal(anchoredAfterOutput.id, readingAnchor.id, JSON.stringify({ readingAnchor, anchoredAfterOutput }));
    assert.ok(Math.abs(anchoredAfterOutput.offset - readingAnchor.offset) <= 1, JSON.stringify({ readingAnchor, anchoredAfterOutput }));
    assert.equal(anchoredAfterOutput.windowY, readingAnchor.windowY, JSON.stringify({ readingAnchor, anchoredAfterOutput }));
    assert.ok(Math.abs(anchoredAfterOutput.terminalTop - readingAnchor.terminalTop) <= 1, JSON.stringify({ readingAnchor, anchoredAfterOutput }));
    assert.ok(Math.abs(anchoredAfterOutput.composerTop - readingAnchor.composerTop) <= 1, JSON.stringify({ readingAnchor, anchoredAfterOutput }));

    await waitFor(
      () => cdp.evaluate("document.getElementById('pane').textContent.includes('pane-line-219')"),
      "initial raw pane snapshot did not render",
    );
    await cdp.evaluate("document.getElementById('raw-view').click(); true");
    const rawGeometry = await cdp.evaluate(`(() => {
      const pane = document.getElementById('pane');
      const paneBox = pane.getBoundingClientRect();
      const terminal = document.querySelector('.terminal-shell').getBoundingClientRect();
      const title = document.querySelector('.terminal-title').getBoundingClientRect();
      const composer = document.getElementById('composer').getBoundingClientRect();
      const style = getComputedStyle(pane);
      return {
        paneTop: paneBox.top,
        paneBottom: paneBox.bottom,
        paneCenterX: paneBox.left + paneBox.width / 2,
        paneCenterY: paneBox.top + paneBox.height / 2,
        terminalTop: terminal.top,
        terminalBottom: terminal.bottom,
        titleBottom: title.bottom,
        composerTop: composer.top,
        clientHeight: pane.clientHeight,
        scrollHeight: pane.scrollHeight,
        scrollTop: pane.scrollTop,
        overflowY: style.overflowY,
        minHeight: style.minHeight,
        rawSelected: document.getElementById('raw-view').classList.contains('selected'),
      };
    })()`);
    assert.equal(rawGeometry.rawSelected, true, JSON.stringify(rawGeometry));
    assert.equal(rawGeometry.overflowY, "auto", JSON.stringify(rawGeometry));
    assert.equal(rawGeometry.minHeight, "0px", JSON.stringify(rawGeometry));
    assert.ok(rawGeometry.clientHeight > 0, JSON.stringify(rawGeometry));
    assert.ok(rawGeometry.scrollHeight > rawGeometry.clientHeight, JSON.stringify(rawGeometry));
    assert.ok(Math.abs(rawGeometry.scrollTop - (rawGeometry.scrollHeight - rawGeometry.clientHeight)) <= 1, JSON.stringify(rawGeometry));
    assert.ok(Math.abs(rawGeometry.paneTop - rawGeometry.titleBottom) <= 1, JSON.stringify(rawGeometry));
    assert.ok(Math.abs(rawGeometry.paneBottom - rawGeometry.terminalBottom) <= 1, JSON.stringify(rawGeometry));
    assert.ok(rawGeometry.terminalBottom <= rawGeometry.composerTop, JSON.stringify(rawGeometry));
    await cdp.send("Input.dispatchMouseEvent", {
      type: "mouseWheel",
      x: rawGeometry.paneCenterX,
      y: rawGeometry.paneCenterY,
      deltaX: 0,
      deltaY: -140,
    });
    await waitFor(
      () => cdp.evaluate(`document.getElementById('pane').scrollTop < ${rawGeometry.scrollTop}`),
      "raw pane did not respond to reader scrolling",
    );
    const rawBeforeOutput = await cdp.evaluate(`(() => {
      const pane = document.getElementById('pane');
      pane.scrollTop = Math.floor((pane.scrollHeight - pane.clientHeight) * 0.45);
      pane.dispatchEvent(new Event('scroll'));
      return pane.scrollTop;
    })()`);
    assert.ok(rawBeforeOutput > 0, JSON.stringify({ rawBeforeOutput, rawGeometry }));
    emitPanePatch({
      base_revision: 1,
      revision: 2,
      start_line: 220,
      delete_lines: 0,
      lines: ["pane-line-220 streamed output"],
    });
    await waitFor(
      () => cdp.evaluate("document.getElementById('pane').textContent.includes('pane-line-220 streamed output')"),
      "raw pane patch did not render",
    );
    const rawAfterOutput = await cdp.evaluate("document.getElementById('pane').scrollTop");
    assert.ok(Math.abs(rawAfterOutput - rawBeforeOutput) <= 1, JSON.stringify({ rawBeforeOutput, rawAfterOutput }));
    // Once Raw is visible, unrelated render churn must never write scrollTop.
    // In particular, iOS may deliver its touch-scroll event after an overview
    // or transcript render; a render-time write races that deferred gesture.
    await cdp.evaluate(`new Promise((resolve) => requestAnimationFrame(() => requestAnimationFrame(resolve)))`);
    const rawBeforeRenderChurn = await cdp.evaluate(`(() => {
      const pane = document.getElementById('pane');
      let owner = pane;
      let descriptor = null;
      while (owner && !descriptor) {
        descriptor = Object.getOwnPropertyDescriptor(owner, 'scrollTop');
        owner = Object.getPrototypeOf(owner);
      }
      if (!descriptor?.get || !descriptor?.set) throw new Error('scrollTop descriptor unavailable');
      window.__rawScrollTopWrites = 0;
      Object.defineProperty(pane, 'scrollTop', {
        configurable: true,
        get() { return descriptor.get.call(this); },
        set(value) {
          window.__rawScrollTopWrites += 1;
          descriptor.set.call(this, value);
        },
      });
      pane.dispatchEvent(new Event('touchstart'));
      return pane.scrollTop;
    })()`);
    emitOverviewPatch([mockSession("tron", "%100", "codex-main", "working", {
      agent: "codex", profile: "codex-max", path: "/workspace", command: "codex",
    })]);
    transcriptFixture = transcript(20, 80, "third-transcript");
    await waitFor(
      () => cdp.evaluate("document.getElementById('agent-meta').textContent.includes('working')"),
      "overview churn did not render while Raw was visible",
    );
    await waitFor(
      () => cdp.evaluate("document.querySelector('[data-transcript-id=\"message-99\"]') !== null"),
      "transcript churn did not render while Raw was visible",
      5_000,
    );
    const rawAfterRenderChurn = await cdp.evaluate(`(() => {
      const pane = document.getElementById('pane');
      const result = { scrollTop: pane.scrollTop, writes: window.__rawScrollTopWrites };
      delete pane.scrollTop;
      delete window.__rawScrollTopWrites;
      return result;
    })()`);
    assert.equal(rawAfterRenderChurn.writes, 0, JSON.stringify({ rawBeforeRenderChurn, rawAfterRenderChurn }));
    assert.ok(Math.abs(rawAfterRenderChurn.scrollTop - rawBeforeRenderChurn) <= 1, JSON.stringify({ rawBeforeRenderChurn, rawAfterRenderChurn }));
    // Switching display modes is navigation, not a request to resume tail
    // following. Raw output keeps streaming while its DOM is hidden, so the
    // reader's state-level offset must survive that hidden redraw too.
    await cdp.evaluate("document.getElementById('conversation-view').click(); true");
    emitPanePatch({
      base_revision: 2,
      revision: 3,
      start_line: 221,
      delete_lines: 0,
      lines: ["pane-line-221 output while Raw is hidden"],
    });
    await waitFor(
      () => cdp.evaluate("document.getElementById('pane').textContent.includes('pane-line-221 output while Raw is hidden')"),
      "hidden raw pane patch did not render",
    );
    await cdp.evaluate("document.getElementById('raw-view').click(); true");
    const rawAfterViewNavigation = await cdp.evaluate(`(() => {
      const pane = document.getElementById('pane');
      return {
        scrollTop: pane.scrollTop,
        selected: document.getElementById('raw-view').classList.contains('selected'),
        bottom: pane.getBoundingClientRect().bottom,
        terminalBottom: document.querySelector('.terminal-shell').getBoundingClientRect().bottom,
      };
    })()`);
    assert.equal(rawAfterViewNavigation.selected, true, JSON.stringify(rawAfterViewNavigation));
    assert.ok(Math.abs(rawAfterViewNavigation.scrollTop - rawAfterOutput) <= 1, JSON.stringify({ rawAfterOutput, rawAfterViewNavigation }));
    assert.ok(Math.abs(rawAfterViewNavigation.bottom - rawAfterViewNavigation.terminalBottom) <= 1, JSON.stringify(rawAfterViewNavigation));
    await cdp.evaluate("document.getElementById('conversation-view').click(); true");

    // Conversation mode compacts only adjacent, low-signal coordination calls.
    // Prose and meaningful/error results remain prominent, and expanding a
    // compact run restores every original tool card without moving the reader.
    transcriptFixture = {
      available: true,
      source: "codex",
      changed: true,
      content_hash: "coordination-tools",
      truncated: false,
      messages: [
        ...Array.from({ length: 10 }, (_, index) => ({
          id: `tool-prefix-${index}`, role: "assistant", markdown: `Agent context ${index} ${"readable context ".repeat(8)}`,
        })),
        { id: "tool-human-before", role: "user", markdown: "Human request remains visible" },
        { id: "tool-agent-before", role: "assistant", markdown: "Agent explanation remains visible" },
        { id: "tool-wait-1", role: "tool", kind: "tool", tool_name: "wait_agent", tool_input: "agent-a" },
        { id: "tool-wait-2", role: "tool", kind: "tool", tool_name: "collaboration.wait_agent", tool_output: "timed out" },
        { id: "tool-send-1", role: "tool", kind: "tool", tool_name: "send_message", tool_input: "<img src=x onerror=alert(1)>", tool_output: "delivered" },
        { id: "tool-agent-middle", role: "assistant", markdown: "This prose splits coordination runs" },
        { id: "tool-exec-1", role: "tool", kind: "tool", tool_name: "functions.exec", tool_input: "<img src=x onerror=exec(1)>", tool_output: '{"exit_code":0,"output":"first command output"}' },
        { id: "tool-exec-2", role: "tool", kind: "tool", tool_name: "exec_command", tool_input: "second command", tool_output: "Process exited with code 0" },
        { id: "tool-exec-3", role: "tool", kind: "tool", tool_name: "tools/exec", tool_input: "third command", tool_output: '{"exit_code":0,"output":"third <script>safe</script> output"}' },
        { id: "tool-exec-4", role: "tool", kind: "tool", tool_name: "functions.exec_command", tool_input: "fourth command", tool_output: "ok" },
        { id: "tool-exec-timeout", role: "tool", kind: "tool", tool_name: "exec", tool_output: "timed out" },
        { id: "tool-exec-ok-after-timeout", role: "tool", kind: "tool", tool_name: "exec", tool_output: "ok" },
        { id: "tool-exec-json-error", role: "tool", kind: "tool", tool_name: "exec_command", tool_output: '{"exit_code":1}' },
        { id: "tool-exec-json-ok", role: "tool", kind: "tool", tool_name: "exec_command", tool_output: '{"exit_code":0}' },
        { id: "tool-exec-process-error", role: "tool", kind: "tool", tool_name: "exec", tool_output: "Process exited with code 1" },
        { id: "tool-apply-1", role: "tool", kind: "tool", tool_name: "apply_patch", tool_output: "ok" },
        { id: "tool-apply-2", role: "tool", kind: "tool", tool_name: "apply_patch", tool_output: "completed" },
        { id: "tool-web-1", role: "tool", kind: "tool", tool_name: "web.run", tool_output: "ok" },
        { id: "tool-web-2", role: "tool", kind: "tool", tool_name: "web.run", tool_output: "completed" },
        { id: "tool-plan-1", role: "tool", kind: "tool", tool_name: "update_plan", tool_output: "ok" },
        { id: "tool-plan-2", role: "tool", kind: "tool", tool_name: "update_plan", tool_output: "completed" },
        { id: "tool-exec-error", role: "tool", kind: "tool", tool_name: "exec", tool_output: "Error: command failed with status 1" },
        { id: "tool-exec-split-1", role: "tool", kind: "tool", tool_name: "exec", tool_output: "result before another tool" },
        { id: "tool-patch-split", role: "tool", kind: "tool", tool_name: "apply_patch", tool_output: "updated a different resource" },
        { id: "tool-exec-split-2", role: "tool", kind: "tool", tool_name: "exec", tool_output: "result after another tool" },
        { id: "tool-send-2", role: "tool", kind: "tool", tool_name: "send_message" },
        { id: "tool-follow-1", role: "tool", kind: "tool", tool_name: "followup_task", tool_output: '{"status":"completed"}' },
        { id: "tool-wait-error", role: "tool", kind: "tool", tool_name: "wait_agent", tool_output: "Error: failed to receive approval" },
        { id: "tool-wait-cancelled", role: "tool", kind: "tool", tool_name: "wait_agent", tool_output: '{"status":"cancelled"}' },
        { id: "tool-wait-numeric", role: "tool", kind: "tool", tool_name: "wait_agent", tool_output: '{"status":500}' },
        { id: "tool-wait-boolean", role: "tool", kind: "tool", tool_name: "wait_agent", tool_output: '{"status":false}' },
        { id: "tool-wait-null", role: "tool", kind: "tool", tool_name: "wait_agent", tool_output: '{"state":null}' },
        { id: "tool-wait-meaningful", role: "tool", kind: "tool", tool_name: "wait_agent", tool_output: "Agent completed the deployment and verified every session." },
        { id: "tool-list-1", role: "tool", kind: "tool", tool_name: "list_agents", tool_output: '[{"path":"/root/a","status":"waiting"}]' },
        { id: "tool-wait-3", role: "tool", kind: "tool", tool_name: "wait_agent", tool_output: "ok" },
        { id: "tool-human-after", role: "user", markdown: "Latest human follow-up remains visible" },
        ...Array.from({ length: 14 }, (_, index) => ({
          id: `tool-suffix-${index}`, role: "assistant", markdown: `Later agent answer ${index} ${"more visible prose ".repeat(8)}`,
        })),
      ],
    };
    await waitFor(
      () => cdp.evaluate("document.querySelectorAll('#conversation .tool-call-group').length === 4"),
      "internal tool runs did not collapse on mobile",
      5_000,
    );
    const compactTools = await cdp.evaluate(`(() => {
      const conversation = document.getElementById('conversation');
      const groups = [...conversation.querySelectorAll('.tool-call-group')];
      const first = groups[0];
      const bounds = conversation.getBoundingClientRect();
      conversation.scrollTop += first.getBoundingClientRect().top - bounds.top - 18;
      conversation.dispatchEvent(new Event('scroll'));
      const before = conversation.scrollTop;
      first.querySelector(':scope > summary').click();
      return new Promise((resolve) => requestAnimationFrame(() => requestAnimationFrame(() => resolve({
        groupSummaries: groups.map((group) => group.querySelector(':scope > summary').textContent),
        open: first.open,
        order: [...first.querySelectorAll('.tool-card-group-item')].map((node) => node.dataset.transcriptId),
        humanVisible: conversation.textContent.includes('Human request remains visible')
          && conversation.textContent.includes('Latest human follow-up remains visible'),
        agentVisible: conversation.textContent.includes('Agent explanation remains visible')
          && conversation.textContent.includes('This prose splits coordination runs'),
        errorSummary: conversation.querySelector('[data-transcript-id="tool-wait-error"] > summary')?.textContent,
        cancelledSummary: conversation.querySelector('[data-transcript-id="tool-wait-cancelled"] > summary')?.textContent,
        malformedStatusSummaries: ['numeric', 'boolean', 'null'].map((suffix) =>
          conversation.querySelector('[data-transcript-id="tool-wait-' + suffix + '"] > summary')?.textContent),
        meaningfulSeparate: Boolean(conversation.querySelector('[data-transcript-id="tool-wait-meaningful"]')),
        execErrorSummary: conversation.querySelector('[data-transcript-id="tool-exec-error"] > summary')?.textContent,
        execBoundarySummaries: ['timeout', 'ok-after-timeout', 'json-error', 'json-ok', 'process-error']
          .map((suffix) => conversation.querySelector('[data-transcript-id="tool-exec-' + suffix + '"] > summary')?.textContent),
        splitToolsSeparate: [
          'tool-exec-split-1', 'tool-patch-split', 'tool-exec-split-2',
          'tool-apply-1', 'tool-apply-2', 'tool-web-1', 'tool-web-2', 'tool-plan-1', 'tool-plan-2',
        ]
          .every((id) => {
            const node = conversation.querySelector('[data-transcript-id="' + id + '"]');
            return Boolean(node) && !node.closest('.tool-call-group');
          }),
        fileReaderPreferences: localStorage.getItem('atmux.file-reader-preferences'),
        markupInjected: Boolean(conversation.querySelector('img, script')),
        escapedInputVisible: first.textContent.includes('<img src=x onerror=alert(1)>'),
        before,
        after: conversation.scrollTop,
      }))));
    })()`);
    assert.equal(compactTools.open, true, JSON.stringify(compactTools));
    assert.deepEqual(compactTools.order, ["tool-wait-1", "tool-wait-2", "tool-send-1"]);
    assert.ok(compactTools.groupSummaries[0].includes("wait_agent ×2"), JSON.stringify(compactTools));
    assert.ok(compactTools.groupSummaries[0].includes("send_message ×1"), JSON.stringify(compactTools));
    assert.ok(compactTools.groupSummaries.includes("exec ×4"), JSON.stringify(compactTools));
    assert.equal(compactTools.humanVisible, true, JSON.stringify(compactTools));
    assert.equal(compactTools.agentVisible, true, JSON.stringify(compactTools));
    assert.equal(compactTools.errorSummary, "wait_agent · error", JSON.stringify(compactTools));
    assert.equal(compactTools.cancelledSummary, "wait_agent · error", JSON.stringify(compactTools));
    assert.deepEqual(compactTools.malformedStatusSummaries, [
      "wait_agent · error", "wait_agent · error", "wait_agent · error",
    ], JSON.stringify(compactTools));
    assert.equal(compactTools.meaningfulSeparate, true, JSON.stringify(compactTools));
    assert.equal(compactTools.execErrorSummary, "exec · error", JSON.stringify(compactTools));
    assert.deepEqual(compactTools.execBoundarySummaries, [
      "exec · error", "exec · result", "exec_command · error", "exec_command · result", "exec · error",
    ], JSON.stringify(compactTools));
    assert.equal(compactTools.splitToolsSeparate, true, JSON.stringify(compactTools));
    assert.equal(compactTools.fileReaderPreferences, '{"wrap":true,"size":"small"}');
    assert.equal(compactTools.markupInjected, false, JSON.stringify(compactTools));
    assert.equal(compactTools.escapedInputVisible, true, JSON.stringify(compactTools));
    assert.ok(Math.abs(compactTools.after - compactTools.before) <= 1, JSON.stringify(compactTools));

    const expandedExec = await cdp.evaluate(`(() => {
      const conversation = document.getElementById('conversation');
      const group = [...conversation.querySelectorAll('.tool-call-group')]
        .find((node) => node.querySelector(':scope > summary').textContent === 'exec ×4');
      const summary = group.querySelector(':scope > summary');
      summary.click();
      return new Promise((resolve) => requestAnimationFrame(() => resolve({
        open: group.open,
        label: summary.getAttribute('aria-label'),
        order: [...group.querySelectorAll('.tool-card-group-item')].map((node) => node.dataset.transcriptId),
        inputVisible: group.textContent.includes('<img src=x onerror=exec(1)>'),
        resultVisible: group.textContent.includes('third <script>safe</script> output'),
        markupInjected: Boolean(group.querySelector('img, script')),
      })));
    })()`);
    assert.equal(expandedExec.open, true, JSON.stringify(expandedExec));
    assert.equal(expandedExec.label, "exec ×4; 4 calls and results");
    assert.deepEqual(expandedExec.order, ["tool-exec-1", "tool-exec-2", "tool-exec-3", "tool-exec-4"]);
    assert.equal(expandedExec.inputVisible, true, JSON.stringify(expandedExec));
    assert.equal(expandedExec.resultVisible, true, JSON.stringify(expandedExec));
    assert.equal(expandedExec.markupInjected, false, JSON.stringify(expandedExec));

    // A stale expansion callback must not mutate a freshly reconnected
    // transcript even when it is still the same pane. Hold the queued callback,
    // force the pane stream's resync path, then invoke the old callback against
    // the new Conversation generation.
    const staleExpansionSetup = await cdp.evaluate(`(() => {
      const conversation = document.getElementById('conversation');
      const group = conversation.querySelector('.tool-call-group');
      group.dataset.oldGeneration = 'true';
      const original = window.requestAnimationFrame;
      window.__staleToolExpansionCallbacks = [];
      window.requestAnimationFrame = (callback) => {
        window.__staleToolExpansionCallbacks.push(callback);
        return window.__staleToolExpansionCallbacks.length;
      };
      const capturedScroll = conversation.scrollTop;
      group.querySelector(':scope > summary').click();
      window.requestAnimationFrame = original;
      return { capturedScroll, queued: window.__staleToolExpansionCallbacks.length };
    })()`);
    assert.equal(staleExpansionSetup.queued, 1, JSON.stringify(staleExpansionSetup));
    const currentPaneStream = [...paneStreams].at(-1);
    assert.ok(currentPaneStream, "same-pane reconnect test requires the current pane stream");
    currentPaneStream.write(`event: pane.patch\ndata: ${JSON.stringify({
      base_revision: 999, revision: 1_000, start_line: 0, delete_lines: 0, lines: [],
    })}\n\n`);
    await waitFor(
      () => cdp.evaluate("document.getElementById('stream-state').textContent === 'Live' && document.querySelectorAll('#conversation .tool-call-group:not([data-old-generation])').length === 4"),
      "same-pane reconnect did not replace the old tool group generation",
      5_000,
    );
    const staleExpansionResult = await cdp.evaluate(`(() => {
      const conversation = document.getElementById('conversation');
      const desired = ${staleExpansionSetup.capturedScroll} > 300 ? 120 : 600;
      conversation.scrollTop = desired;
      conversation.dispatchEvent(new Event('scroll'));
      const before = conversation.scrollTop;
      const callbacks = window.__staleToolExpansionCallbacks.splice(0);
      for (const callback of callbacks) callback(performance.now());
      return {
        before,
        after: conversation.scrollTop,
        oldConnected: Boolean(document.querySelector('[data-old-generation]')),
        groupCount: document.querySelectorAll('#conversation .tool-call-group').length,
      };
    })()`);
    assert.equal(staleExpansionResult.oldConnected, false, JSON.stringify(staleExpansionResult));
    assert.equal(staleExpansionResult.groupCount, 4, JSON.stringify(staleExpansionResult));
    assert.ok(Math.abs(staleExpansionResult.after - staleExpansionResult.before) <= 1, JSON.stringify({ staleExpansionSetup, staleExpansionResult }));

    // Conversation visibility is a same-row mobile control. Agent prose can
    // never be hidden; Human and Internal are independent, persistent filters.
    const filterDefaults = await cdp.evaluate(`(() => {
      const open = document.getElementById('conversation-filters-open');
      open.click();
      const dialog = document.getElementById('conversation-filters-dialog');
      const title = document.querySelector('.terminal-title').getBoundingClientRect();
      const openBounds = open.getBoundingClientRect();
      const labels = [...dialog.querySelectorAll('.conversation-filter-options label')];
      const inputs = [...dialog.querySelectorAll('.conversation-filter-options input')];
      return {
        open: dialog.open,
        expanded: open.getAttribute('aria-expanded'),
        indicator: document.getElementById('conversation-filters-indicator').textContent,
        active: open.classList.contains('active'),
        openHeight: openBounds.height,
        titleHeight: title.height,
        sameRow: openBounds.top >= title.top - 1 && openBounds.bottom <= title.bottom + 1,
        checked: inputs.map((input) => input.checked),
        disabled: inputs.map((input) => input.disabled),
        inputSizes: inputs.map((input) => {
          const box = input.getBoundingClientRect();
          return [box.width, box.height];
        }),
        targetHeights: labels.map((label) => label.getBoundingClientRect().height),
        buttonHeights: [...dialog.querySelectorAll('button')].map((button) => button.getBoundingClientRect().height),
        label: open.getAttribute('aria-label'),
        describedBy: dialog.getAttribute('aria-describedby'),
        overflowX: document.documentElement.scrollWidth - innerWidth,
      };
    })()`);
    assert.equal(filterDefaults.open, true, JSON.stringify(filterDefaults));
    assert.equal(filterDefaults.expanded, "true", JSON.stringify(filterDefaults));
    assert.equal(filterDefaults.indicator, "All", JSON.stringify(filterDefaults));
    assert.equal(filterDefaults.active, false, JSON.stringify(filterDefaults));
    assert.ok(filterDefaults.openHeight >= 44, JSON.stringify(filterDefaults));
    assert.ok(filterDefaults.titleHeight <= 48, JSON.stringify(filterDefaults));
    assert.equal(filterDefaults.sameRow, true, JSON.stringify(filterDefaults));
    assert.deepEqual(filterDefaults.checked, [true, true, true]);
    assert.deepEqual(filterDefaults.disabled, [true, false, false]);
    assert.ok(filterDefaults.inputSizes.every(([width, height]) => width >= 16 && height >= 16), JSON.stringify(filterDefaults));
    assert.ok(filterDefaults.targetHeights.every((height) => height >= 44), JSON.stringify(filterDefaults));
    assert.ok(filterDefaults.buttonHeights.every((height) => height >= 44), JSON.stringify(filterDefaults));
    assert.equal(filterDefaults.label, "Conversation visibility: showing all message types");
    assert.equal(filterDefaults.describedBy, "conversation-filters-note");
    assert.ok(filterDefaults.overflowX <= 1, JSON.stringify(filterDefaults));

    const filterReadingAnchor = await cdp.evaluate(`(() => {
      const conversation = document.getElementById('conversation');
      const target = conversation.querySelector('[data-transcript-id="tool-agent-middle"]');
      const bounds = conversation.getBoundingClientRect();
      conversation.scrollTop += target.getBoundingClientRect().top - bounds.top - 12;
      conversation.dispatchEvent(new Event('scroll'));
      return {
        id: target.dataset.transcriptId,
        offset: target.getBoundingClientRect().top - bounds.top,
        scrollTop: conversation.scrollTop,
      };
    })()`);
    const humanHidden = await cdp.evaluate(`(() => {
      document.getElementById('conversation-show-human').click();
      const conversation = document.getElementById('conversation');
      const bounds = conversation.getBoundingClientRect();
      const target = conversation.querySelector('[data-transcript-id="tool-agent-middle"]');
      return {
        humans: conversation.querySelectorAll('[data-transcript-visibility="human"]').length,
        agents: conversation.querySelectorAll('[data-transcript-visibility="agent"]').length,
        groups: conversation.querySelectorAll('.tool-call-group').length,
        targetOffset: target.getBoundingClientRect().top - bounds.top,
        indicator: document.getElementById('conversation-filters-indicator').textContent,
        active: document.getElementById('conversation-filters-open').classList.contains('active'),
        stored: localStorage.getItem('atmux.conversation-visibility'),
      };
    })()`);
    assert.equal(humanHidden.humans, 0, JSON.stringify(humanHidden));
    assert.ok(humanHidden.agents > 0, JSON.stringify(humanHidden));
    assert.equal(humanHidden.groups, 4, JSON.stringify(humanHidden));
    assert.ok(Math.abs(humanHidden.targetOffset - filterReadingAnchor.offset) <= 1, JSON.stringify({ filterReadingAnchor, humanHidden }));
    assert.equal(humanHidden.indicator, "1 off", JSON.stringify(humanHidden));
    assert.equal(humanHidden.active, true, JSON.stringify(humanHidden));
    assert.equal(humanHidden.stored, '{"human":false,"internal":true}');

    const internalHidden = await cdp.evaluate(`(() => {
      document.getElementById('conversation-show-human').click();
      document.getElementById('conversation-show-internal').click();
      const conversation = document.getElementById('conversation');
      return {
        humans: conversation.querySelectorAll('[data-transcript-visibility="human"]').length,
        agents: conversation.querySelectorAll('[data-transcript-visibility="agent"]').length,
        internals: conversation.querySelectorAll('[data-transcript-visibility="internal"]').length,
        errors: [...conversation.querySelectorAll('summary')].filter((node) => node.textContent.includes('error')).length,
        indicator: document.getElementById('conversation-filters-indicator').textContent,
        stored: localStorage.getItem('atmux.conversation-visibility'),
      };
    })()`);
    assert.ok(internalHidden.humans > 0, JSON.stringify(internalHidden));
    assert.ok(internalHidden.agents > 0, JSON.stringify(internalHidden));
    assert.equal(internalHidden.internals, 0, JSON.stringify(internalHidden));
    assert.equal(internalHidden.errors, 0, JSON.stringify(internalHidden));
    assert.equal(internalHidden.indicator, "1 off", JSON.stringify(internalHidden));
    assert.equal(internalHidden.stored, '{"human":true,"internal":false}');

    const agentOnly = await cdp.evaluate(`(() => {
      document.getElementById('conversation-show-human').click();
      const conversation = document.getElementById('conversation');
      return {
        humans: conversation.querySelectorAll('[data-transcript-visibility="human"]').length,
        agents: conversation.querySelectorAll('[data-transcript-visibility="agent"]').length,
        internals: conversation.querySelectorAll('[data-transcript-visibility="internal"]').length,
        indicator: document.getElementById('conversation-filters-indicator').textContent,
        label: document.getElementById('conversation-filters-open').getAttribute('aria-label'),
        resetDisabled: document.getElementById('conversation-filters-reset').disabled,
        stored: localStorage.getItem('atmux.conversation-visibility'),
      };
    })()`);
    assert.equal(agentOnly.humans, 0, JSON.stringify(agentOnly));
    assert.ok(agentOnly.agents > 0, JSON.stringify(agentOnly));
    assert.equal(agentOnly.internals, 0, JSON.stringify(agentOnly));
    assert.equal(agentOnly.indicator, "2 off", JSON.stringify(agentOnly));
    assert.equal(agentOnly.label, "Conversation visibility: 2 message types hidden");
    assert.equal(agentOnly.resetDisabled, false, JSON.stringify(agentOnly));
    assert.equal(agentOnly.stored, '{"human":false,"internal":false}');
    await cdp.evaluate("document.querySelector('#conversation-filters-dialog .primary').click(); true");

    const filteredBeforeIncoming = await cdp.evaluate(`(() => {
      const conversation = document.getElementById('conversation');
      const target = conversation.querySelector('[data-transcript-id="tool-agent-middle"]');
      const bounds = conversation.getBoundingClientRect();
      conversation.scrollTop += target.getBoundingClientRect().top - bounds.top - 10;
      conversation.dispatchEvent(new Event('scroll'));
      return { id: target.dataset.transcriptId, offset: target.getBoundingClientRect().top - bounds.top };
    })()`);
    transcriptFixture = {
      ...transcriptFixture,
      content_hash: "conversation-filter-incoming",
      messages: [
        ...transcriptFixture.messages,
        { id: "hidden-incoming-human", role: "user", markdown: "HIDDEN HUMAN <img src=x onerror=human()>" },
        { id: "hidden-incoming-tool", role: "tool", kind: "tool", tool_name: "exec", tool_output: "Error: HIDDEN TOOL" },
        { id: "visible-incoming-agent", role: "assistant", markdown: "VISIBLE AGENT <img src=x onerror=agent()>" },
      ],
    };
    await waitFor(
      () => cdp.evaluate("document.getElementById('conversation').textContent.includes('VISIBLE AGENT')"),
      "a visible incoming agent message did not render through agent-only mode",
      5_000,
    );
    const filteredIncoming = await cdp.evaluate(`(() => {
      const conversation = document.getElementById('conversation');
      const bounds = conversation.getBoundingClientRect();
      const target = conversation.querySelector('[data-transcript-id="tool-agent-middle"]');
      return {
        humanHidden: !conversation.textContent.includes('HIDDEN HUMAN'),
        toolHidden: !conversation.textContent.includes('HIDDEN TOOL'),
        agentVisible: conversation.textContent.includes('VISIBLE AGENT'),
        targetOffset: target.getBoundingClientRect().top - bounds.top,
        markupInjected: Boolean(conversation.querySelector('img, script')),
      };
    })()`);
    assert.equal(filteredIncoming.humanHidden, true, JSON.stringify(filteredIncoming));
    assert.equal(filteredIncoming.toolHidden, true, JSON.stringify(filteredIncoming));
    assert.equal(filteredIncoming.agentVisible, true, JSON.stringify(filteredIncoming));
    assert.equal(filteredIncoming.markupInjected, false, JSON.stringify(filteredIncoming));
    assert.ok(Math.abs(filteredIncoming.targetOffset - filteredBeforeIncoming.offset) <= 1, JSON.stringify({ filteredBeforeIncoming, filteredIncoming }));

    // Reconnect and a full document reload retain the same filters. Neither
    // operation can flash hidden transcript records from the selected pane.
    const filteredPaneStream = [...paneStreams].at(-1);
    assert.ok(filteredPaneStream, "conversation filter reconnect needs a pane stream");
    filteredPaneStream.write(`event: pane.patch\ndata: ${JSON.stringify({
      base_revision: 9_999, revision: 10_000, start_line: 0, delete_lines: 0, lines: [],
    })}\n\n`);
    await waitFor(
      () => cdp.evaluate(`document.getElementById('stream-state').textContent === 'Live'
        && document.getElementById('conversation').textContent.includes('VISIBLE AGENT')
        && !document.getElementById('conversation').textContent.includes('HIDDEN HUMAN')
        && document.getElementById('conversation-filters-indicator').textContent === '2 off'`),
      "agent-only visibility did not survive a pane reconnect",
      5_000,
    );
    await cdp.send("Page.reload");
    await waitFor(
      () => cdp.evaluate(`document.readyState === 'complete'
        && document.getElementById('conversation').textContent.includes('VISIBLE AGENT')
        && !document.getElementById('conversation').textContent.includes('HIDDEN HUMAN')
        && document.getElementById('conversation-filters-indicator').textContent === '2 off'`),
      "agent-only visibility did not restore from local storage",
      5_000,
    );

    // A pane change clears group expansion/content synchronously; an identical
    // transcript id from another owner cannot inherit the previous pane DOM.
    transcriptFixture = {
      available: true, source: "codex", changed: true, content_hash: "pane-b-prose", truncated: false,
      messages: [
        { id: "pane-b-human", role: "user", markdown: "Different pane human" },
        { id: "pane-b-tool", role: "tool", kind: "tool", tool_name: "exec", tool_output: "ok" },
        { id: "pane-b-agent", role: "assistant", markdown: "Different pane conversation" },
      ],
    };
    await cdp.evaluate("document.getElementById('mobile-back').click(); true");
    await waitFor(
      () => cdp.evaluate(`!document.body.classList.contains('has-selection')
        && document.querySelector('.session-button[data-session-id="midnight~%5"]') !== null`),
      "tool grouping test did not return to the populated agent list",
    );
    const groupClearedOnPaneChange = await cdp.evaluate(`(() => {
      document.querySelector('.session-button[data-session-id="midnight~%5"]').click();
      return !document.querySelector('#conversation .tool-call-group');
    })()`);
    assert.equal(groupClearedOnPaneChange, true, "pane A tool group remained visible under pane B");
    await waitFor(
      () => cdp.evaluate("document.getElementById('conversation').textContent.includes('Different pane conversation')"),
      "pane B conversation did not replace pane A tool groups",
      5_000,
    );
    const paneFilterPersistence = await cdp.evaluate(`(() => {
      const conversation = document.getElementById('conversation');
      return {
        agent: conversation.textContent.includes('Different pane conversation'),
        human: conversation.textContent.includes('Different pane human'),
        tool: conversation.querySelector('[data-transcript-id="pane-b-tool"]') !== null,
        indicator: document.getElementById('conversation-filters-indicator').textContent,
      };
    })()`);
    assert.deepEqual(paneFilterPersistence, {
      agent: true, human: false, tool: false, indicator: "2 off",
    });
    transcriptFixture = {
      available: true, source: "codex", changed: true, content_hash: "pane-b-hidden-only", truncated: false,
      messages: [
        { id: "pane-b-human", role: "user", markdown: "Different pane human" },
        { id: "pane-b-tool", role: "tool", kind: "tool", tool_name: "exec", tool_output: "ok" },
      ],
    };
    await waitFor(
      () => cdp.evaluate("document.querySelector('#conversation .conversation-empty')?.textContent.includes('No agent messages to show')"),
      "agent-only mode did not expose a recoverable filtered empty state",
      5_000,
    );
    await cdp.evaluate(`(() => {
      document.getElementById('conversation-filters-open').click();
      document.getElementById('conversation-filters-reset').click();
      document.querySelector('#conversation-filters-dialog .primary').click();
      return true;
    })()`);
    const filtersReset = await cdp.evaluate(`(() => ({
      human: document.getElementById('conversation').textContent.includes('Different pane human'),
      tool: document.getElementById('conversation').querySelector('[data-transcript-id="pane-b-tool"]') !== null,
      indicator: document.getElementById('conversation-filters-indicator').textContent,
      active: document.getElementById('conversation-filters-open').classList.contains('active'),
      stored: localStorage.getItem('atmux.conversation-visibility'),
    }))()`);
    assert.deepEqual(filtersReset, {
      human: true, tool: true, indicator: "All", active: false,
      stored: '{"human":true,"internal":true}',
    });

    // Hiding Human can merge two tool runs. Anchor restoration follows the
    // first underlying tool member when the group's generated outer id changes.
    transcriptFixture = {
      available: true, source: "codex", changed: true, content_hash: "filter-merged-tool-anchor", truncated: false,
      messages: [
        ...Array.from({ length: 10 }, (_, index) => ({
          id: `anchor-prefix-${index}`, role: "assistant",
          markdown: `Anchor prefix ${index} ${"stable reading context ".repeat(8)}`,
        })),
        { id: "anchor-exec-1", role: "tool", kind: "tool", tool_name: "exec", tool_output: "ok" },
        { id: "anchor-human", role: "user", markdown: "Human boundary between exec calls" },
        { id: "anchor-exec-2", role: "tool", kind: "tool", tool_name: "exec", tool_output: "ok" },
        { id: "anchor-exec-3", role: "tool", kind: "tool", tool_name: "exec", tool_output: "ok" },
        ...Array.from({ length: 12 }, (_, index) => ({
          id: `anchor-suffix-${index}`, role: "assistant",
          markdown: `Anchor suffix ${index} ${"more stable reading context ".repeat(8)}`,
        })),
      ],
    };
    await waitFor(
      () => cdp.evaluate("document.querySelector('[data-transcript-id=\"tool-group:anchor-exec-2\"]') !== null"),
      "pre-filter exec-2 group did not render",
      5_000,
    );
    const mergedGroupAnchorBefore = await cdp.evaluate(`(() => {
      const conversation = document.getElementById('conversation');
      const group = conversation.querySelector('[data-transcript-id="tool-group:anchor-exec-2"]');
      const bounds = conversation.getBoundingClientRect();
      conversation.scrollTop += group.getBoundingClientRect().top - bounds.top - 14;
      conversation.dispatchEvent(new Event('scroll'));
      return {
        offset: group.getBoundingClientRect().top - bounds.top,
        members: JSON.parse(group.dataset.transcriptMembers),
      };
    })()`);
    assert.deepEqual(mergedGroupAnchorBefore.members, ["anchor-exec-2", "anchor-exec-3"]);
    const mergedGroupAnchorAfter = await cdp.evaluate(`(() => {
      document.getElementById('conversation-filters-open').click();
      document.getElementById('conversation-show-human').click();
      const conversation = document.getElementById('conversation');
      const group = conversation.querySelector('[data-transcript-id="tool-group:anchor-exec-1"]');
      const bounds = conversation.getBoundingClientRect();
      return {
        offset: group.getBoundingClientRect().top - bounds.top,
        members: JSON.parse(group.dataset.transcriptMembers),
        summary: group.querySelector(':scope > summary').textContent,
        oldOuterGone: !conversation.querySelector('[data-transcript-id="tool-group:anchor-exec-2"]'),
      };
    })()`);
    assert.deepEqual(mergedGroupAnchorAfter.members, ["anchor-exec-1", "anchor-exec-2", "anchor-exec-3"]);
    assert.equal(mergedGroupAnchorAfter.summary, "exec ×3", JSON.stringify(mergedGroupAnchorAfter));
    assert.equal(mergedGroupAnchorAfter.oldOuterGone, true, JSON.stringify(mergedGroupAnchorAfter));
    assert.ok(Math.abs(mergedGroupAnchorAfter.offset - mergedGroupAnchorBefore.offset) <= 1, JSON.stringify({
      mergedGroupAnchorBefore, mergedGroupAnchorAfter,
    }));
    const splitGroupAnchorAfterReset = await cdp.evaluate(`(() => {
      document.getElementById('conversation-filters-reset').click();
      const conversation = document.getElementById('conversation');
      const singleton = conversation.querySelector('[data-transcript-id="anchor-exec-1"]');
      const bounds = conversation.getBoundingClientRect();
      return {
        offset: singleton.getBoundingClientRect().top - bounds.top,
        singletonOutsideGroup: !singleton.closest('.tool-call-group'),
        humanRestored: conversation.textContent.includes('Human boundary between exec calls'),
        splitGroupMembers: JSON.parse(
          conversation.querySelector('[data-transcript-id="tool-group:anchor-exec-2"]')
            .dataset.transcriptMembers,
        ),
      };
    })()`);
    assert.equal(splitGroupAnchorAfterReset.singletonOutsideGroup, true, JSON.stringify(splitGroupAnchorAfterReset));
    assert.equal(splitGroupAnchorAfterReset.humanRestored, true, JSON.stringify(splitGroupAnchorAfterReset));
    assert.deepEqual(splitGroupAnchorAfterReset.splitGroupMembers, ["anchor-exec-2", "anchor-exec-3"]);
    assert.ok(Math.abs(splitGroupAnchorAfterReset.offset - mergedGroupAnchorAfter.offset) <= 1, JSON.stringify({
      mergedGroupAnchorAfter, splitGroupAnchorAfterReset,
    }));
    await cdp.evaluate(`(() => {
      document.querySelector('#conversation-filters-dialog .primary').click();
      return true;
    })()`);

    const composerBeforeFocus = await cdp.evaluate(`(() => {
      const box = document.getElementById('composer').getBoundingClientRect();
      return { top: box.top, bottom: box.bottom };
    })()`);
    await cdp.evaluate("document.getElementById('message').focus(); true");
    const composerAfterFocus = await cdp.evaluate(`(() => {
      const box = document.getElementById('composer').getBoundingClientRect();
      return { top: box.top, bottom: box.bottom };
    })()`);
    assert.ok(Math.abs(composerAfterFocus.top - composerBeforeFocus.top) <= 1, JSON.stringify({ composerBeforeFocus, composerAfterFocus }));
    assert.ok(Math.abs(composerAfterFocus.bottom - composerBeforeFocus.bottom) <= 1, JSON.stringify({ composerBeforeFocus, composerAfterFocus }));
    await cdp.send("Emulation.setDeviceMetricsOverride", {
      width: 390, height: 430, deviceScaleFactor: 1, mobile: false,
    });
    await waitFor(
      () => cdp.evaluate("getComputedStyle(document.documentElement).getPropertyValue('--app-height').trim() === '430px'"),
      "focused composer did not follow the keyboard-sized visual viewport",
    );
    const focusedComposer = await cdp.evaluate(`(() => {
      const box = document.getElementById('composer').getBoundingClientRect();
      return {
        viewport: window.visualViewport?.height || window.innerHeight,
        top: box.top,
        bottom: box.bottom,
        width: box.width,
        fontSize: getComputedStyle(document.getElementById('message')).fontSize,
        topbarVisible: getComputedStyle(document.querySelector('.topbar')).display !== 'none',
      };
    })()`);
    assert.equal(focusedComposer.fontSize, "16px", JSON.stringify(focusedComposer));
    assert.equal(focusedComposer.topbarVisible, false, JSON.stringify(focusedComposer));
    assert.ok(focusedComposer.top >= 0, JSON.stringify(focusedComposer));
    assert.ok(focusedComposer.bottom <= focusedComposer.viewport + 1, JSON.stringify(focusedComposer));
    assert.ok(focusedComposer.width <= 390, JSON.stringify(focusedComposer));
    const documentViewport = await cdp.evaluate(`(() => ({
      bodyPosition: getComputedStyle(document.body).position,
      rootOverflow: getComputedStyle(document.documentElement).overflow,
      scrollHeight: document.documentElement.scrollHeight,
      clientHeight: document.documentElement.clientHeight,
    }))()`);
    assert.equal(documentViewport.bodyPosition, "fixed", JSON.stringify(documentViewport));
    assert.equal(documentViewport.rootOverflow, "hidden", JSON.stringify(documentViewport));
    assert.ok(documentViewport.scrollHeight <= documentViewport.clientHeight, JSON.stringify(documentViewport));
    await cdp.evaluate("document.getElementById('message').blur(); true");
    await cdp.send("Emulation.setDeviceMetricsOverride", {
      width: 390, height: 844, deviceScaleFactor: 1, mobile: false,
    });
    await cdp.send("Emulation.setDeviceMetricsOverride", {
      width: 1024, height: 768, deviceScaleFactor: 1, mobile: false,
    });
    const desktopActions = await cdp.evaluate(`({
      visible: getComputedStyle(document.getElementById('quick-actions-open')).display !== 'none',
      modelControlVisible: getComputedStyle(document.getElementById('model-control')).display !== 'none',
      directActionsVisible: getComputedStyle(document.querySelector('.agent-head .actions')).display !== 'none',
      wordmarkDisplay: getComputedStyle(document.querySelector('.brand-wordmark')).display,
    })`);
    assert.equal(desktopActions.visible, true, JSON.stringify(desktopActions));
    assert.equal(desktopActions.modelControlVisible, false, JSON.stringify(desktopActions));
    assert.equal(desktopActions.directActionsVisible, false, JSON.stringify(desktopActions));
    assert.notEqual(desktopActions.wordmarkDisplay, "none", JSON.stringify(desktopActions));
    await cdp.evaluate("document.getElementById('raw-view').click(); true");
    const desktopRawGeometry = await cdp.evaluate(`(() => {
      const pane = document.getElementById('pane');
      const box = pane.getBoundingClientRect();
      const title = document.querySelector('.terminal-title').getBoundingClientRect();
      const terminal = document.querySelector('.terminal-shell').getBoundingClientRect();
      return {
        top: box.top,
        bottom: box.bottom,
        titleBottom: title.bottom,
        terminalBottom: terminal.bottom,
        clientHeight: pane.clientHeight,
        scrollHeight: pane.scrollHeight,
      };
    })()`);
    assert.ok(desktopRawGeometry.clientHeight > 0, JSON.stringify(desktopRawGeometry));
    assert.ok(desktopRawGeometry.scrollHeight > desktopRawGeometry.clientHeight, JSON.stringify(desktopRawGeometry));
    assert.ok(Math.abs(desktopRawGeometry.top - desktopRawGeometry.titleBottom) <= 1, JSON.stringify(desktopRawGeometry));
    assert.ok(Math.abs(desktopRawGeometry.bottom - desktopRawGeometry.terminalBottom) <= 1, JSON.stringify(desktopRawGeometry));
    await cdp.evaluate("document.getElementById('conversation-view').click(); true");
    await cdp.evaluate("document.getElementById('quick-actions-open').click(); true");
    assert.equal(await cdp.evaluate("document.getElementById('quick-actions-dialog').open"), true);
    await cdp.evaluate("document.getElementById('quick-actions-dialog').close(); true");
    await cdp.send("Emulation.setDeviceMetricsOverride", {
      width: 390, height: 844, deviceScaleFactor: 1, mobile: false,
    });

    // Replacing one detail with another keeps Agents directly behind the
    // current screen instead of stacking session -> usage -> menu.
    await cdp.evaluate("document.getElementById('pulse-open').click(); true");
    await waitFor(
      () => cdp.evaluate("new URL(location.href).searchParams.get('view') === 'usage'"),
      "Usage did not replace the selected agent detail",
    );
    await cdp.evaluate("history.back(); true");
    await waitFor(
      () => cdp.evaluate("!new URL(location.href).searchParams.has('session') && !new URL(location.href).searchParams.has('view') && !document.getElementById('welcome').hidden"),
      "browser Back after multiple details did not restore the agent menu",
    );
    assert.equal(await cdp.evaluate("location.pathname"), "/");

    await cdp.evaluate("document.getElementById('launch-open').click(); true");
    await waitFor(
      () => cdp.evaluate("document.getElementById('launch-dialog').open"),
      "launch dialog did not open",
    );
    assert.equal(
      await cdp.evaluate("document.getElementById('launch-machine').value"),
      "tron",
      "without context the launcher skips the first online but unconfigured owner",
    );
    await cdp.evaluate(`(() => {
      const machine = document.getElementById('launch-machine');
      machine.value = 'tron';
      machine.dispatchEvent(new Event('change', { bubbles: true }));
      return true;
    })()`);
    await cdp.evaluate("document.getElementById('launch-browse').click(); true");
    await waitFor(
      () => cdp.evaluate("document.querySelector('.launch-browser-folder')?.textContent.includes('custom')"),
      "folder browser did not render configured-root contents",
    );
    await cdp.evaluate("document.querySelector('.launch-browser-folder').click(); true");
    await waitFor(
      () => cdp.evaluate("!document.getElementById('launch-browser-use').disabled"),
      "folder browser did not navigate into the selected folder",
    );
    await cdp.evaluate("document.getElementById('launch-browser-use').click(); true");
    assert.equal(await cdp.evaluate("document.getElementById('launch-directory').value"), "/workspace/custom");
    await waitFor(
      () => cdp.evaluate("document.querySelectorAll('#launch-session option').length === 2"),
      "saved conversation selector did not load after choosing the exact folder",
    );
    const savedConversation = await cdp.evaluate(`({
      visible: !document.getElementById('launch-sessions').hidden,
      value: document.getElementById('launch-session').options[1].value,
      label: document.getElementById('launch-session').options[1].textContent,
    })`);
    assert.equal(savedConversation.visible, true);
    assert.match(savedConversation.value, /^saved-[0-9a-f]{32}$/);
    assert.match(savedConversation.label, /Codex.*Continue the mobile launch flow/);
    await cdp.evaluate(`(() => {
      const conversation = document.getElementById('launch-session');
      conversation.selectedIndex = 1;
      window.__resumeConfirmation = null;
      window.confirm = (message) => {
        window.__resumeConfirmation = message;
        return false;
      };
      document.getElementById('launch-form').requestSubmit();
      return true;
    })()`);
    assert.equal(await cdp.evaluate("window.__resumeConfirmation"), [
      "Resume this saved conversation?",
      "",
      "Machine: Tron (tron)",
      "Profile: codex-max",
      "Folder: /workspace/custom",
      "Agent: codex",
      "Preview: Continue the mobile launch flow",
    ].join("\n"));
    assert.equal(launchRequests.length, 0, "cancelling confirmation must not send a launch request");
    assert.equal(await cdp.evaluate("document.getElementById('launch-dialog').open"), true);

    launchResponseDelayMs = 150;
    await cdp.evaluate(`(() => {
      document.getElementById('launch-session').value = '';
      window.__normalLaunchConfirmCalls = 0;
      window.confirm = () => { window.__normalLaunchConfirmCalls += 1; return false; };
      document.getElementById('launch-form').requestSubmit();
      return true;
    })()`);
    await waitFor(
      () => launchRequests.length === 1 && cdp.evaluate(`(() => {
        const form = document.getElementById('launch-form');
        return document.getElementById('launch-dialog').open
          && form.querySelector('button[type=submit]').disabled;
      })()`),
      "ordinary launch did not remain pending until its response",
    );
    await waitFor(
      () => cdp.evaluate(`(() => {
        const form = document.getElementById('launch-form');
        return !document.getElementById('launch-dialog').open
          && !form.querySelector('button[type=submit]').disabled
          && document.getElementById('toast').textContent.includes('Launched');
      })()`),
      "ordinary launch response did not close the dialog and restore submit state",
    );
    assert.equal(await cdp.evaluate("window.__normalLaunchConfirmCalls"), 0);
    assert.deepEqual(
      await cdp.evaluate("JSON.parse(localStorage.getItem('atmux.launch-directories'))"),
      { tron: ["/workspace/custom"] },
    );
    await cdp.evaluate("document.getElementById('launch-open').click(); true");
    await waitFor(
      () => cdp.evaluate("document.getElementById('launch-dialog').open"),
      "reopened launch dialog did not open",
    );
    assert.equal(await cdp.evaluate("document.getElementById('launch-machine').value"), "tron");
    await waitFor(
      () => cdp.evaluate("[...document.querySelectorAll('#launch-directory-options option')].some((option) => option.value === '/workspace/custom')"),
      "remembered folder did not return to the project picker",
    );
    await cdp.evaluate("document.querySelector('#launch-dialog .dialog-cancel').click(); true");

    launchMachinesUnavailable = true;
    await cdp.evaluate("document.getElementById('launch-open').click(); true");
    await waitFor(
      () => cdp.evaluate("document.getElementById('launch-dialog').open"),
      "unavailable launch dialog did not open",
    );
    const unavailableLaunch = await cdp.evaluate(`({
      machine: document.getElementById('launch-machine').value,
      machineDisabled: document.getElementById('launch-machine').disabled,
      projectDisabled: document.getElementById('launch-directory').disabled,
      browseDisabled: document.getElementById('launch-browse').disabled,
      submitDisabled: document.querySelector('#launch-form button[type=submit]').disabled,
      note: document.getElementById('launch-note').textContent,
    })`);
    assert.deepEqual(unavailableLaunch, {
      machine: "",
      machineDisabled: true,
      projectDisabled: true,
      browseDisabled: true,
      submitDisabled: true,
      note: "No online machine currently has both runnable agent profiles and configured project folders.",
    });
    await cdp.evaluate("document.querySelector('#launch-dialog .dialog-cancel').click(); true");
    launchMachinesUnavailable = false;

    await cdp.evaluate("localStorage.removeItem('atmux.pulse-account'); true");
    await cdp.send("Page.navigate", { url: `http://127.0.0.1:${port}/?view=usage` });
    await waitFor(
      () => cdp.evaluate("document.getElementById('pulse-status')?.textContent.includes('Ryan') && document.querySelectorAll('.pulse-quota-card').length === 2 && document.getElementById('pulse-content')?.textContent.includes('$4.00')"),
      "Pulse dashboard did not auto-load the configured account",
    );
    const dashboard = await cdp.evaluate(`({
      accountTag: document.getElementById('pulse-account').tagName,
      accountLabel: document.getElementById('pulse-account').selectedOptions[0].textContent,
      selectedAccount: new URL(location.href).searchParams.get('pulseAccount'),
      headings: [...document.querySelectorAll('.pulse-section h2')].map((node) => node.textContent),
      numericAccountInput: Boolean(document.querySelector('#pulse-account[inputmode="numeric"]')),
      content: document.getElementById('pulse-content').textContent,
      hasFiveHourGauge: Boolean(document.querySelector('progress[aria-label="Used: 62.5 percent"]')),
      reportSummary: document.querySelector('.pulse-report-detail summary')?.textContent,
    })`);
    assert.equal(dashboard.accountTag, "SELECT");
    assert.equal(dashboard.accountLabel, "Ryan");
    assert.equal(dashboard.selectedAccount, "4");
    assert.equal(dashboard.numericAccountInput, false);
    for (const heading of ["Account quotas", "Gemini buckets", "Token and cost report", "Context sessions", "Open alerts", "Subscriptions"]) {
      assert.ok(dashboard.headings.includes(heading), `missing ${heading}`);
    }
    assert.equal(dashboard.hasFiveHourGauge, true);
    for (const visibleValue of [
      "5-hour quota", "62.5%", "Weekly quota", "38.0%", "resets", "max",
      "account value", "reporter atmux-fixture", "slightly fast", "1,500,000", "$4.00",
      "claude-opus-5",
    ]) {
      assert.ok(dashboard.content.includes(visibleValue), `missing dashboard value ${visibleValue}`);
    }
    assert.ok(dashboard.reportSummary.includes("claude-max"));
    assert.ok(dashboard.reportSummary.includes("1,500,000 tokens · $4.00"));

    // Privacy modes and restrictive embedded browsers can expose Storage but
    // throw from every method. This script runs before app.js in a fresh
    // document, proving initialization itself (including setRailCollapsed)
    // fails open and still renders usable Conversation visibility controls.
    await cdp.send("Page.addScriptToEvaluateOnNewDocument", { source: `
      for (const method of ['getItem', 'setItem', 'removeItem', 'clear']) {
        Object.defineProperty(Storage.prototype, method, {
          configurable: true,
          value() { throw new DOMException('Storage disabled by fixture', 'SecurityError'); },
        });
      }
    ` });
    await cdp.send("Page.navigate", { url: `http://127.0.0.1:${port}/?session=tron~%25100` });
    await waitFor(
      () => cdp.evaluate(`document.readyState === 'complete'
        && !document.getElementById('agent-view').hidden
        && document.getElementById('conversation-filters-indicator').textContent === 'All'`),
      "throwing browser Storage aborted Conversation initialization",
      5_000,
    );
    const storageDeniedInitialization = await cdp.evaluate(`(() => {
      let storageThrows = false;
      try { localStorage.getItem('probe'); } catch { storageThrows = true; }
      const open = document.getElementById('conversation-filters-open');
      open.click();
      const human = document.getElementById('conversation-show-human');
      const internal = document.getElementById('conversation-show-internal');
      const defaults = [human.checked, internal.checked];
      human.click();
      const afterWriteFailure = {
        indicator: document.getElementById('conversation-filters-indicator').textContent,
        human: human.checked,
        internal: internal.checked,
      };
      document.getElementById('conversation-filters-reset').click();
      return {
        storageThrows,
        agentViewVisible: !document.getElementById('agent-view').hidden,
        dialogOpen: document.getElementById('conversation-filters-dialog').open,
        defaults,
        afterWriteFailure,
        reset: [human.checked, internal.checked],
        resetIndicator: document.getElementById('conversation-filters-indicator').textContent,
        overflowX: document.documentElement.scrollWidth - innerWidth,
      };
    })()`);
    assert.equal(storageDeniedInitialization.storageThrows, true, JSON.stringify(storageDeniedInitialization));
    assert.equal(storageDeniedInitialization.agentViewVisible, true, JSON.stringify(storageDeniedInitialization));
    assert.equal(storageDeniedInitialization.dialogOpen, true, JSON.stringify(storageDeniedInitialization));
    assert.deepEqual(storageDeniedInitialization.defaults, [true, true]);
    assert.deepEqual(storageDeniedInitialization.afterWriteFailure, {
      indicator: "1 off", human: false, internal: true,
    });
    assert.deepEqual(storageDeniedInitialization.reset, [true, true]);
    assert.equal(storageDeniedInitialization.resetIndicator, "All", JSON.stringify(storageDeniedInitialization));
    assert.ok(storageDeniedInitialization.overflowX <= 1, JSON.stringify(storageDeniedInitialization));
  } catch (error) {
    testError = error;
    throw error;
  } finally {
    try {
      await cleanupBrowserHarness({ cdp, chrome, server, profileDirectory });
    } catch (cleanupError) {
      if (!testError) throw cleanupError;
      // Preserve the functional assertion/command failure as the primary test
      // result while still making teardown trouble visible in CI diagnostics.
      console.error(cleanupError);
    }
    transcriptFixture = null;
    paneSnapshotContent = "";
    launchMachinesUnavailable = false;
    paneStreams.clear();
    overviewStreams.clear();
  }
});
