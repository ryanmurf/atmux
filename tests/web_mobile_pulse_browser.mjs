import assert from "node:assert/strict";
import { spawn } from "node:child_process";
import { EventEmitter } from "node:events";
import { mkdtemp, readFile, rm, writeFile } from "node:fs/promises";
import { createServer } from "node:http";
import { tmpdir } from "node:os";
import { extname, join, resolve } from "node:path";
import test from "node:test";

const WEB_ROOT = resolve(import.meta.dirname, "../web");
const CDP_COMMAND_TIMEOUT_MS = 15_000;
const CHROME_START_TIMEOUT_MS = 30_000;
const CLEANUP_TIMEOUT_MS = 5_000;
let transcriptFixture = null;
let agentSummaryFixture = null;
const sessionRenameRequests = [];
let transcriptResponseDelayMs = 0;
const transcriptRequests = [];
let paneSnapshotContent = "";
const paneStreams = new Set();
const overviewStreams = new Set();
let overviewUnavailable = false;
const launchRequests = [];
const launchSessionRequests = [];
const launchDirectoryMutationRequests = [];
const launchBrowserChildren = new Set();
let failLiveModels = false;
let restartCapabilityReady = false;
let restartCapabilityToken = "restart-v1-" + "a".repeat(64);
const restartRequests = [];
let launchOptionsDelayMs = 0;
let launchResponseDelayMs = 0;
let launchDirectoryMutationDelayMs = 0;
let launchMachinesUnavailable = false;
let largeLaunchDirectoryFixture = false;
let launchSessionResponseDelayMs = 0;
let overviewRevision = 1;
/// Every fixed update verb the dashboard forwarded, in order.
const fleetUpdateRequests = [];
/// The node state Tron reports; applying flips it the way a real node does.
let tronUpdateState = "idle";
let delayProjectFilePane = null;
let delayFileSavePane = null;
let delayGitSummaryPane = null;
let delayGitDiffPane = null;
let nextFileSaveConflict = false;
const fileSaveRequests = [];
const codeNavRequests = [];
const NAV_FIXTURE = [
  'import { formatTotal } from "./util";',
  "export function total(values: number[]): number {",
  "  const sum = values.reduce((left, right) => left + right, 0);",
  "  return formatTotal(sum);",
  "}",
  "export const report = () => total([1, 2]);",
].join("\n");
const UTIL_FIXTURE = [
  "export function formatTotal(value: number): number {",
  "  return value;",
  "}",
].join("\n");
const messageRequests = [];
const imageMessageRequests = [];
const specialKeyRequests = [];
const legacySpecialKeyRequests = [];
let nextSpecialKeyStatus = null;
let nextSpecialKeyResponseDelayMs = 0;
let nextSpecialKeyResponseGate = null;
let simulateOldCoordinatorInputRoute = false;
let nextMessageFailurePane = null;
let messageResponseDelayMs = 0;
const projectFileContents = new Map();
const projectFileVersions = new Map();
const LONG_KERNEL_VERSION = "k".repeat(160);
const LONG_OS_VERSION = "o".repeat(160);
// Mirrors a real coordinator health line: a single unwrappable run longer
// than any phone is wide.
const LONG_MACHINE_HEALTH = "machine clue is unreachable at https://192.168.0.140:7345: "
  + "error sending request for url (https://192.168.0.140:7345/api/v1/sessions): "
  + "client error (Connect): connection refused";

function mockNodeUpdate(overrides = {}) {
  return {
    enabled: true,
    version: "0.2.0",
    target: "x86_64-unknown-linux-gnu",
    mode: "self",
    latest: null,
    state: "idle",
    progress: null,
    last_checked_at: Date.now() - 30_000,
    last_error: null,
    previous: null,
    ...overrides,
  };
}

/// A fleet where exactly one machine has a verified release waiting, one is
/// already current, and one cannot be reached at all.
function mockFleetUpdates() {
  return [
    {
      id: "tron",
      label: "Tron",
      online: true,
      error: null,
      update: mockNodeUpdate({
        state: tronUpdateState,
        latest: {
          version: "0.3.0",
          tag: "v0.3.0",
          published_at: new Date(Date.now() - 2 * 3600 * 1000).toISOString(),
          verified: true,
          asset: "atmux-x86_64-unknown-linux-gnu",
        },
      }),
    },
    {
      id: "midnight",
      label: "Midnight",
      online: true,
      error: null,
      update: mockNodeUpdate({
        target: "aarch64-apple-darwin",
        previous: { version: "0.1.0", path: "/Users/ryan/.local/bin/atmux.prev" },
      }),
    },
    {
      id: "clue",
      label: "Clue",
      online: false,
      error: LONG_MACHINE_HEALTH,
      update: null,
    },
  ];
}

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
  const digit = [...`${machine}:${pane}`]
    .reduce((sum, character) => sum + character.charCodeAt(0), 0)
    .toString(16).at(-1);
  return {
    id: `${machine}~${pane}`, pane_id: pane, machine, name, status,
    instance_id: `pane-v1-${digit.repeat(64)}`,
    agent: "claude", profile: "max", path: "/workspace", command: "claude",
    ...extra,
  };
}

function mockOverviewMachines() {
  return [
    {
      id: "tron", label: "Tron", kind: "local", online: true, sessions: 1,
      metrics: {
        uptime_seconds: 183_840,
        kernel_version: LONG_KERNEL_VERSION,
        os_version: LONG_OS_VERSION,
      },
    },
    { id: "midnight", label: "Midnight", kind: "remote", online: true, sessions: 2 },
    { id: "clue", label: "Clue", kind: "remote", online: false, sessions: 0, health: LONG_MACHINE_HEALTH },
  ];
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

function emitOverviewSnapshot(sessions) {
  overviewRevision += 1;
  const payload = `event: sessions.snapshot\ndata: ${JSON.stringify({
    revision: overviewRevision,
    sessions,
    health: null,
    machines: mockOverviewMachines(),
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
  if (pathname === "/api/v1/events" && overviewUnavailable) {
    response.writeHead(503, { "content-type": "text/plain", "cache-control": "no-store" });
    response.end("unavailable");
    return true;
  }
  if (pathname === "/api/v1/events") {
    response.writeHead(200, { "content-type": "text/event-stream", "cache-control": "no-store" });
    response.write(`event: sessions.snapshot\ndata: ${JSON.stringify({
      revision: overviewRevision,
      sessions: [{
        id: "tron~%100", pane_id: "%100", machine: "tron", name: "codex-main",
        instance_id: `pane-v1-${"1".repeat(64)}`,
        status: "waiting", agent: "codex", profile: "codex-max", path: "/workspace", command: "codex",
      },
      mockSession("midnight", "%5", "alpha-planner", "working"),
      mockSession("midnight", "%7", "beta-planner", "waiting"),
      ],
      machines: mockOverviewMachines(),
      health: null,
    })}\n\n`);
    overviewStreams.add(response);
    request.once("close", () => { overviewStreams.delete(response); });
    return true;
  }
  if (pathname === "/api/v1/fleet/updates") {
    json(response, mockFleetUpdates());
    return true;
  }
  const updateVerb = /^\/api\/v1\/machines\/([^/]+)\/update\/(check|apply|rollback)$/.exec(pathname);
  if (updateVerb && request.method === "POST") {
    const machine = decodeURIComponent(updateVerb[1]);
    const action = updateVerb[2];
    let body = "";
    request.setEncoding("utf8");
    request.on("data", (chunk) => { body += chunk; });
    request.on("end", () => {
      fleetUpdateRequests.push({ machine, action, body });
      if (action === "apply" && machine === "tron") tronUpdateState = "restarting";
      const entry = mockFleetUpdates().find((item) => item.id === machine);
      if (!entry?.update) {
        errorJson(response, 409, "machine cannot update itself");
        return;
      }
      response.writeHead(202, { "content-type": "application/json", "cache-control": "no-store" });
      response.end(JSON.stringify(entry.update));
    });
    return true;
  }
  if (/^\/api\/v1\/panes\/[^/]+\/events$/.test(pathname)) {
    response.writeHead(200, { "content-type": "text/event-stream", "cache-control": "no-store" });
    response.write(`event: pane.snapshot\ndata: ${JSON.stringify({ revision: 1, content: paneSnapshotContent })}\n\n`);
    paneStreams.add(response);
    request.once("close", () => { paneStreams.delete(response); });
    return true;
  }
  if (/^\/api\/v1\/panes\/[^/]+\/summary$/.test(pathname)) {
    json(response, agentSummaryFixture || { enabled: false, title: "", digest: "" }); return true;
  }
  if (/^\/api\/v1\/sessions\/[^/]+$/.test(pathname) && request.method === "PATCH") {
    let body = ""; request.setEncoding("utf8"); request.on("data", (chunk) => { body += chunk; });
    request.on("end", () => {
      sessionRenameRequests.push({ pane: decodeURIComponent(pathname.split("/")[4]), body: JSON.parse(body) });
      response.writeHead(204); response.end();
    }); return true;
  }
  if (/^\/api\/v1\/panes\/[^/]+\/transcript$/.test(pathname)) {
    const observed = { pane: decodeURIComponent(pathname.split("/")[4]), hash: url.searchParams.get("known_hash"), closed: false };
    transcriptRequests.push(observed);
    // Capture at request time so retired, slow responses can be tested.
    const payload = structuredClone(transcriptFixture || { available: false, source: "codex", changed: false, messages: [] });
    let timer;
    response.once("close", () => { observed.closed = true; clearTimeout(timer); });
    if (transcriptResponseDelayMs) timer = setTimeout(() => json(response, payload), transcriptResponseDelayMs);
    else json(response, payload);
    return true;
  }
  if (/^\/api\/v1\/panes\/[^/]+\/code\/(?:definitions|references|resolve)$/.test(pathname)) {
    const operation = pathname.split("/").pop();
    const observed = {
      operation,
      pane: decodeURIComponent(pathname.split("/")[4]),
      symbol: url.searchParams.get("symbol"),
      path: url.searchParams.get("path"),
      spec: url.searchParams.get("spec"),
    };
    codeNavRequests.push(observed);
    const reply = (results) => json(response, {
      pane_id: observed.pane, operation, query: observed.symbol || observed.spec, truncated: false, results,
    });
    if (operation === "resolve" && observed.spec === "./util") {
      reply([{ path: "src/util.ts", line: 1, column: 17, kind: "function", preview: "export function formatTotal(value: number): number {" }]);
    } else if (operation === "definitions" && observed.symbol === "reduce") {
      reply([
        { path: "src/util.ts", line: 2, column: 3, kind: "method", preview: "return value;" },
        { path: "src/app.js", line: 3, column: 7, kind: "variable", preview: "const line2 = <script>" },
      ]);
    } else reply([]);
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
        entries: [
          { kind: "file", name: "app.js", path: "src/app.js", size: 8192 },
          { kind: "file", name: "nav.ts", path: "src/nav.ts", size: Buffer.byteLength(NAV_FIXTURE) },
          { kind: "file", name: "util.ts", path: "src/util.ts", size: Buffer.byteLength(UTIL_FIXTURE) },
        ],
      });
    } else if (path === "src/nav.ts" || path === "src/util.ts") {
      const content = path === "src/nav.ts" ? NAV_FIXTURE : UTIL_FIXTURE;
      json(response, {
        kind: "file", path, language: "typescript", size: Buffer.byteLength(content), truncated: false,
        content, content_hash: "9".repeat(64), line_count: content.split("\n").length,
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
    const paneId = decodeURIComponent(pathname.split("/")[4]);
    let body = "";
    request.setEncoding("utf8");
    request.on("data", (chunk) => { body += chunk; });
    request.on("end", () => {
      messageRequests.push({ paneId, body });
      const reply = () => {
        if (nextMessageFailurePane === paneId) {
          nextMessageFailurePane = null;
          errorJson(response, 503, "message fixture rejected the send");
        } else json(response, {});
      };
      if (messageResponseDelayMs > 0) setTimeout(reply, messageResponseDelayMs);
      else reply();
    });
    return true;
  }
  if (/^\/api\/v1\/panes\/[^/]+\/image-messages$/.test(pathname) && request.method === "POST") {
    const paneId = decodeURIComponent(pathname.split("/")[4]);
    let body = "";
    request.setEncoding("utf8");
    request.on("data", (chunk) => { body += chunk; });
    request.on("end", () => {
      imageMessageRequests.push({ paneId, body: JSON.parse(body) });
      json(response, {});
    });
    return true;
  }
  if (/^\/api\/v1\/panes\/[^/]+\/input-keys$/.test(pathname) && request.method === "POST") {
    if (simulateOldCoordinatorInputRoute) {
      simulateOldCoordinatorInputRoute = false;
      request.resume();
      errorJson(response, 404, "legacy coordinator has no input-key route");
      return true;
    }
    const paneId = decodeURIComponent(pathname.split("/")[4]);
    let body = "";
    request.setEncoding("utf8");
    request.on("data", (chunk) => { body += chunk; });
    request.on("end", () => {
      specialKeyRequests.push({ paneId, body: JSON.parse(body) });
      const status = nextSpecialKeyStatus;
      const delayMs = nextSpecialKeyResponseDelayMs;
      const responseGate = nextSpecialKeyResponseGate;
      nextSpecialKeyStatus = null;
      nextSpecialKeyResponseDelayMs = 0;
      nextSpecialKeyResponseGate = null;
      const reply = () => {
        if (status) errorJson(response, status, "fixture input-key rejection");
        else json(response, {});
      };
      if (responseGate) void responseGate.then(reply);
      else if (delayMs > 0) setTimeout(reply, delayMs);
      else reply();
    });
    return true;
  }
  if (/^\/api\/v1\/panes\/[^/]+\/special-keys$/.test(pathname) && request.method === "POST") {
    const paneId = decodeURIComponent(pathname.split("/")[4]);
    let body = "";
    request.setEncoding("utf8");
    request.on("data", (chunk) => { body += chunk; });
    request.on("end", () => {
      legacySpecialKeyRequests.push({ paneId, body: JSON.parse(body) });
      json(response, {});
    });
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
      note: null, resume_available: restartCapabilityReady, resume_note: null,
      restart_token: restartCapabilityReady ? restartCapabilityToken : null,
    });
    return true;
  }
  if (/^\/api\/v1\/panes\/[^/]+\/restart-instance$/.test(pathname) && request.method === "POST") {
    let body = "";
    request.setEncoding("utf8");
    request.on("data", (chunk) => { body += chunk; });
    request.on("end", () => {
      restartRequests.push({ pathname, body: JSON.parse(body) });
      restartCapabilityToken = "restart-v1-" + "b".repeat(64);
      errorJson(response, 409, "agent process changed after restart confirmation");
    });
    return true;
  }
  if (pathname === "/api/v1/launch-options") {
    const tronDirectories = largeLaunchDirectoryFixture
      ? Array.from({ length: 2_000 }, (_, index) => `/workspace/mobile-search-${index}`)
      : ["/workspace", "/workspace/discovered"];
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
          directories: tronDirectories,
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
    const current = path === "/workspace" || path === "/workspace/custom" ? path : null;
    const directories = current === "/workspace/custom"
      ? [...launchBrowserChildren].map((name) => ({ path: `/workspace/custom/${name}`, name }))
      : [{ path: "/workspace/custom", name: "custom" }];
    json(response, {
      machine: "tron", current,
      parent: current === "/workspace/custom" ? "/workspace" : null,
      directories, truncated: false,
    });
    return true;
  }
  if (["/api/v1/launch-directories/folders", "/api/v1/launch-directories/clone"].includes(pathname)
      && request.method === "POST") {
    let body = "";
    request.setEncoding("utf8");
    request.on("data", (chunk) => { body += chunk; });
    request.on("end", () => {
      let parsed = null;
      try { parsed = JSON.parse(body); } catch { /* asserted below */ }
      launchDirectoryMutationRequests.push({ pathname, body: parsed });
      const reply = () => {
        if (parsed?.name === "existing") {
          errorJson(response, 409, "destination already exists");
          return;
        }
        if (/^https:\/\/[^/]*@/.test(String(parsed?.repository || ""))) {
          errorJson(response, 400, "credential-bearing HTTPS repository URLs are not allowed");
          return;
        }
        const derived = String(parsed?.repository || "").split(/[?#]/, 1)[0]
          .replace(/\/+$/, "").split(/[/:]/).pop()?.replace(/\.git$/, "");
        const name = parsed?.name || parsed?.destination || derived;
        launchBrowserChildren.add(name);
        json(response, {
          directory: { path: `/workspace/custom/${name}`, name },
          listing: {
            machine: "tron", current: "/workspace/custom", parent: "/workspace",
            directories: [...launchBrowserChildren].map((child) => ({
              path: `/workspace/custom/${child}`, name: child,
            })),
            truncated: false,
          },
        });
      };
      if (launchDirectoryMutationDelayMs > 0) {
        const delayMs = launchDirectoryMutationDelayMs;
        launchDirectoryMutationDelayMs = 0;
        setTimeout(reply, delayMs);
      } else reply();
    });
    return true;
  }
  if (pathname === "/api/v1/launch-sessions") {
    const directory = url.searchParams.get("directory");
    const profileId = url.searchParams.get("profile_id");
    launchSessionRequests.push({ directory, profileId, machine: url.searchParams.get("machine") });
    const reply = {
      machine: "tron", directory, profile_id: profileId, truncated: false,
      sessions: directory === "/workspace/custom" && profileId === "profile-codex-max"
        ? [{
          id: "saved-0123456789abcdef0123456789abcdef",
          harness: "codex", updated_ms: 1_786_993_200_000,
          preview: "Continue the mobile launch flow",
        }]
        : [],
    };
    if (launchSessionResponseDelayMs > 0) {
      const delayMs = launchSessionResponseDelayMs;
      launchSessionResponseDelayMs = 0;
      setTimeout(() => json(response, reply), delayMs);
    } else json(response, reply);
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
  launchDirectoryMutationRequests.length = 0;
  launchBrowserChildren.clear();
  fileSaveRequests.length = 0;
  messageRequests.length = 0;
  imageMessageRequests.length = 0;
  specialKeyRequests.length = 0;
  legacySpecialKeyRequests.length = 0;
  nextSpecialKeyStatus = null;
  nextSpecialKeyResponseDelayMs = 0;
  nextSpecialKeyResponseGate = null;
  simulateOldCoordinatorInputRoute = false;
  projectFileContents.clear();
  projectFileVersions.clear();
  delayFileSavePane = null;
  nextFileSaveConflict = false;
  nextMessageFailurePane = null;
  messageResponseDelayMs = 0;
  failLiveModels = false;
  launchOptionsDelayMs = 0;
  launchResponseDelayMs = 0;
  launchDirectoryMutationDelayMs = 0;
  overviewRevision = 1;
  fleetUpdateRequests.length = 0;
  tronUpdateState = "idle";
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
    cdp = await openCdp(browserSocket, "about:blank");
    await cdp.send("Page.enable");
    await cdp.send("Page.addScriptToEvaluateOnNewDocument", { source: String.raw`
      window.__speechInstances = [];
      class FakeSpeechRecognition {
        constructor() { window.__speechInstances.push(this); }
        start() {}
        stop() { queueMicrotask(() => this.onend?.()); }
        abort() {}
      }
      window.SpeechRecognition = FakeSpeechRecognition;
      Element.prototype.setPointerCapture = function setPointerCapture() {};
      const nativeArrayBuffer = File.prototype.arrayBuffer;
      File.prototype.arrayBuffer = function delayedArrayBuffer() {
        if (!window.__delayNextFileRead) return nativeArrayBuffer.call(this);
        window.__delayNextFileRead = false;
        return new Promise((resolve) => {
          window.__releaseDelayedFileRead = async () => resolve(await nativeArrayBuffer.call(this));
        });
      };
    ` });
    await cdp.send("Page.navigate", {
      url: `http://127.0.0.1:${port}/?session=tron~%25100`,
    });
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

    const hostileDraft = `<img src=x onerror=alert(1)>\n<script>window.draftLeaked=true</script>`;
    const switchedDrafts = await cdp.evaluate(`(() => {
      const input = document.getElementById('message');
      const shellTopBefore = document.querySelector('.terminal-shell').getBoundingClientRect().top;
      input.focus({ preventScroll: true });
      input.value = ${JSON.stringify(hostileDraft)};
      input.setSelectionRange(5, 17);
      input.dispatchEvent(new InputEvent('input', { bubbles: true, data: 'x' }));
      input.blur();
      document.querySelector('[data-session-id="midnight~%5"]').click();
      const blankOnB = input.value;
      const focusOnB = document.activeElement.id;
      input.value = 'beta agent private draft';
      input.setSelectionRange(4, 10);
      input.dispatchEvent(new InputEvent('input', { bubbles: true, data: 't' }));
      document.querySelector('[data-session-id="tron~%100"]').click();
      return {
        blankOnB,
        focusOnB,
        restoredA: input.value,
        selectionStart: input.selectionStart,
        selectionEnd: input.selectionEnd,
        injectedNode: Boolean(document.querySelector('[src="x"]')),
        injectedScriptRan: window.draftLeaked === true,
        shellTopBefore,
        shellTop: document.querySelector('.terminal-shell').getBoundingClientRect().top,
      };
    })()`);
    assert.equal(switchedDrafts.blankOnB, "", JSON.stringify(switchedDrafts));
    assert.notEqual(switchedDrafts.focusOnB, "message", JSON.stringify(switchedDrafts));
    assert.equal(switchedDrafts.restoredA, hostileDraft, JSON.stringify(switchedDrafts));
    assert.equal(switchedDrafts.selectionStart, 5, JSON.stringify(switchedDrafts));
    assert.equal(switchedDrafts.selectionEnd, 17, JSON.stringify(switchedDrafts));
    assert.equal(switchedDrafts.injectedNode, false, JSON.stringify(switchedDrafts));
    assert.equal(switchedDrafts.injectedScriptRan, false, JSON.stringify(switchedDrafts));
    assert.equal(switchedDrafts.shellTop, switchedDrafts.shellTopBefore, JSON.stringify(switchedDrafts));

    await cdp.evaluate("document.getElementById('launch-open').click(); true");
    await waitFor(
      () => cdp.evaluate("document.getElementById('launch-dialog').open"),
      "new-agent dialog did not open over the pane draft",
    );
    assert.equal(await cdp.evaluate("document.getElementById('message').value"), hostileDraft);
    await cdp.evaluate("document.querySelector('#launch-dialog .dialog-cancel').click(); true");
    await waitFor(
      () => cdp.evaluate("!document.getElementById('launch-dialog').open"),
      "new-agent dialog did not close",
    );
    assert.equal(await cdp.evaluate("document.getElementById('message').value"), hostileDraft);

    const pagehideDraft = `${hostileDraft}\nflush immediately on pagehide`;
    assert.equal(await cdp.evaluate(`(() => {
      const input = document.getElementById('message');
      input.value = ${JSON.stringify(pagehideDraft)};
      input.setSelectionRange(7, 19);
      input.dispatchEvent(new InputEvent('input', { bubbles: true, data: 'e' }));
      window.dispatchEvent(new PageTransitionEvent('pagehide', { persisted: true }));
      const stored = JSON.parse(localStorage.getItem('atmux.composer-drafts.v1'));
      stored.drafts.push({
        key: 'pane:midnight:pane-v1-${"d".repeat(64)}',
        text: 'pane deleted while this browser was closed',
        selectionStart: 0,
        selectionEnd: 0,
        version: 1,
        updatedAt: 1,
      });
      localStorage.setItem('atmux.composer-drafts.v1', JSON.stringify(stored));
      window.__atmuxBeforeDraftReload = true;
      return stored.drafts.some((draft) => draft.text === input.value);
    })()`), true, "pagehide did not flush the draft before its debounce elapsed");
    await cdp.send("Page.reload", { ignoreCache: true });
    await waitFor(
      () => cdp.evaluate(`window.__atmuxBeforeDraftReload !== true
        && document.readyState === 'complete'
        && !document.getElementById('agent-view').hidden
        && document.getElementById('message').value === ${JSON.stringify(pagehideDraft)}`),
      "the selected agent draft did not survive a mobile refresh",
    );
    const refreshedDraft = await cdp.evaluate(`(() => {
      const input = document.getElementById('message');
      return {
        value: input.value,
        selectionStart: input.selectionStart,
        selectionEnd: input.selectionEnd,
        focused: document.activeElement === input,
        shellTop: document.querySelector('.terminal-shell').getBoundingClientRect().top,
      };
    })()`);
    assert.equal(refreshedDraft.value, pagehideDraft, JSON.stringify(refreshedDraft));
    assert.equal(refreshedDraft.selectionStart, 7, JSON.stringify(refreshedDraft));
    assert.equal(refreshedDraft.selectionEnd, 19, JSON.stringify(refreshedDraft));
    assert.equal(refreshedDraft.focused, false, JSON.stringify(refreshedDraft));
    assert.equal(refreshedDraft.shellTop, switchedDrafts.shellTopBefore, JSON.stringify(refreshedDraft));
    assert.equal(await cdp.evaluate(`JSON.parse(
      localStorage.getItem('atmux.composer-drafts.v1')
    ).drafts.some((draft) => draft.text === 'pane deleted while this browser was closed')`), false,
    "an online owner's cold-start orphan survived its authoritative snapshot");

    const messagesBeforeMismatchedAttachment = messageRequests.length;
    const imagesBeforeMismatchedAttachment = imageMessageRequests.length;
    const mismatchedAttachment = await cdp.evaluate(`(async () => {
      const picker = document.getElementById('image-input');
      const transfer = new DataTransfer();
      transfer.items.add(new File(
        [new Uint8Array([137, 80, 78, 71, 13, 10, 26, 10])],
        'draft.png',
        { type: 'image/png' },
      ));
      picker.files = transfer.files;
      picker.dispatchEvent(new Event('change', { bubbles: true }));
      document.querySelector('[data-session-id="midnight~%5"]').click();
      const input = document.getElementById('message');
      const mismatch = {
        draftB: input.value,
        sendDisabled: document.getElementById('send').disabled,
        guidance: document.getElementById('attachment-target').textContent,
      };
      input.dispatchEvent(new KeyboardEvent('keydown', { key: 'Enter', bubbles: true }));
      await new Promise((resolve) => setTimeout(resolve, 50));
      document.querySelector('[data-session-id="tron~%100"]').click();
      mismatch.returnedA = input.value;
      mismatch.sendEnabledOnA = !document.getElementById('send').disabled;
      document.querySelector('[data-session-id="midnight~%5"]').click();
      document.getElementById('attachment-clear').click();
      mismatch.draftBAfterClear = input.value;
      mismatch.sendEnabledAfterClear = !document.getElementById('send').disabled;
      mismatch.attachmentsAfterClear = document.querySelectorAll('.attachment-preview').length;
      document.querySelector('[data-session-id="tron~%100"]').click();
      mismatch.returnedAAfterClear = input.value;
      return mismatch;
    })()`);
    assert.equal(mismatchedAttachment.draftB, "beta agent private draft", JSON.stringify(mismatchedAttachment));
    assert.equal(mismatchedAttachment.sendDisabled, true, JSON.stringify(mismatchedAttachment));
    assert.match(mismatchedAttachment.guidance, /Return to that agent or clear them before sending/);
    assert.equal(mismatchedAttachment.returnedA, pagehideDraft, JSON.stringify(mismatchedAttachment));
    assert.equal(mismatchedAttachment.sendEnabledOnA, true, JSON.stringify(mismatchedAttachment));
    assert.equal(mismatchedAttachment.draftBAfterClear, "beta agent private draft", JSON.stringify(mismatchedAttachment));
    assert.equal(mismatchedAttachment.sendEnabledAfterClear, true, JSON.stringify(mismatchedAttachment));
    assert.equal(mismatchedAttachment.attachmentsAfterClear, 0, JSON.stringify(mismatchedAttachment));
    assert.equal(mismatchedAttachment.returnedAAfterClear, pagehideDraft, JSON.stringify(mismatchedAttachment));
    assert.equal(messageRequests.length, messagesBeforeMismatchedAttachment);
    assert.equal(imageMessageRequests.length, imagesBeforeMismatchedAttachment);

    messageResponseDelayMs = 250;
    const messagesBeforeDraftSubmit = messageRequests.length;
    await cdp.evaluate(`(() => {
      document.getElementById('send').click();
      document.querySelector('[data-session-id="midnight~%5"]').click();
    })()`);
    await waitFor(
      () => messageRequests.length === messagesBeforeDraftSubmit + 1,
      "the first agent draft was not submitted",
    );
    await new Promise((resolve) => setTimeout(resolve, 350));
    messageResponseDelayMs = 0;
    assert.equal(await cdp.evaluate("document.getElementById('message').value"), "beta agent private draft");
    assert.deepEqual(messageRequests.at(-1), {
      paneId: "tron~%100",
      body: JSON.stringify({
        text: pagehideDraft,
        submit: true,
        instance_id: `pane-v1-${"1".repeat(64)}`,
      }),
    });
    assert.equal(await cdp.evaluate(`(() => {
      document.querySelector('[data-session-id="tron~%100"]').click();
      return document.getElementById('message').value;
    })()`), "");
    assert.deepEqual(await cdp.evaluate(`(() => {
      const input = document.getElementById('message');
      input.value = 'new draft after a successful send';
      input.dispatchEvent(new InputEvent('input', { bubbles: true, data: 'd' }));
      document.querySelector('[data-session-id="midnight~%5"]').click();
      const b = input.value;
      document.querySelector('[data-session-id="tron~%100"]').click();
      const restoredA = input.value;
      input.value = '';
      input.dispatchEvent(new InputEvent('input', { bubbles: true, data: null }));
      document.querySelector('[data-session-id="midnight~%5"]').click();
      return { b, restoredA };
    })()`), {
      b: "beta agent private draft",
      restoredA: "new draft after a successful send",
    });

    nextMessageFailurePane = "midnight~%5";
    messageResponseDelayMs = 2_000;
    const messagesBeforeFailedDraftSubmit = messageRequests.length;
    await cdp.evaluate(`(() => {
      document.querySelector('[data-session-id="midnight~%5"]').click();
      document.getElementById('send').click();
    })()`);
    await waitFor(
      () => messageRequests.length === messagesBeforeFailedDraftSubmit + 1,
      "the protected failed-send fixture was not accepted",
    );
    const pressureSessions = Array.from({ length: 65 }, (_, index) => mockSession(
      "midnight",
      `%${200 + index}`,
      `draft-pressure-${index}`,
      "waiting",
      { instance_id: `pane-v1-${(index + 1_000).toString(16).padStart(64, "0")}` },
    ));
    emitOverviewPatch(pressureSessions);
    await waitFor(
      () => cdp.evaluate("Boolean(document.querySelector('[data-session-id=\"midnight~%264\"]'))"),
      "draft-pressure panes were not rendered",
    );
    assert.equal(await cdp.evaluate(`(() => {
      const input = document.getElementById('message');
      for (let index = 0; index < 65; index += 1) {
        document.querySelector('[data-session-id="midnight~%' + (200 + index) + '"]').click();
        input.value = 'pressure draft ' + index;
        input.dispatchEvent(new InputEvent('input', { bubbles: true, data: 'x' }));
      }
      document.querySelector('[data-session-id="midnight~%5"]').click();
      return input.value;
    })()`), "beta agent private draft", "capacity pressure evicted an in-flight failed-send draft");
    await waitFor(
      () => cdp.evaluate("document.getElementById('toast').textContent.includes('rejected the send')"),
      "the failed-send fixture did not reach the composer",
      5_000,
    );
    messageResponseDelayMs = 0;
    assert.equal(await cdp.evaluate("document.getElementById('message').value"), "beta agent private draft");
    assert.deepEqual(await cdp.evaluate(`(() => {
      const input = document.getElementById('message');
      document.querySelector('[data-session-id="tron~%100"]').click();
      const a = input.value;
      document.querySelector('[data-session-id="midnight~%5"]').click();
      return { a, b: input.value };
    })()`), { a: "", b: "beta agent private draft" });

    messageResponseDelayMs = 250;
    const messagesBeforeReincarnation = messageRequests.length;
    await cdp.evaluate("document.getElementById('send').click(); true");
    await waitFor(
      () => messageRequests.length === messagesBeforeReincarnation + 1,
      "the old incarnation send was not accepted by the fixture",
    );
    emitOverviewPatch([
      mockSession("midnight", "%5", "alpha-planner", "working", {
        instance_id: `pane-v1-${"e".repeat(64)}`,
      }),
    ]);
    await waitFor(
      () => cdp.evaluate("document.getElementById('message').value === ''"),
      "a recreated pane id inherited the deleted agent's draft",
    );
    await new Promise((resolve) => setTimeout(resolve, 350));
    messageResponseDelayMs = 0;
    assert.equal(await cdp.evaluate(`(() => {
      const input = document.getElementById('message');
      input.dispatchEvent(new KeyboardEvent('keydown', { key: 'ArrowUp', bubbles: true }));
      return input.value;
    })()`), "", "late success history leaked into a new pane incarnation");
    assert.equal(await cdp.evaluate(`JSON.parse(
      localStorage.getItem('atmux.composer-drafts.v1')
    ).drafts.some((draft) => draft.text === 'beta agent private draft')`), false,
    "reincarnated pane's old draft remained in browser storage");

    await cdp.evaluate(`(() => {
      const input = document.getElementById('message');
      input.value = 'draft for a pane omitted by the next snapshot';
      input.dispatchEvent(new InputEvent('input', { bubbles: true, data: 't' }));
    })()`);
    emitOverviewSnapshot([
      {
        id: "tron~%100", pane_id: "%100", machine: "tron", name: "codex-main",
        instance_id: `pane-v1-${"1".repeat(64)}`,
        status: "waiting", agent: "codex", profile: "codex-max", path: "/workspace", command: "codex",
      },
      mockSession("midnight", "%7", "beta-planner", "waiting"),
    ]);
    await waitFor(
      () => cdp.evaluate("!document.body.classList.contains('has-selection')"),
      "authoritative snapshot did not remove the selected pane",
    );
    assert.equal(await cdp.evaluate(`JSON.parse(
      localStorage.getItem('atmux.composer-drafts.v1')
    ).drafts.length`), 0, "snapshot-removed pane draft remained in browser storage");
    emitOverviewPatch([
      mockSession("midnight", "%5", "alpha-planner", "working", {
        instance_id: `pane-v1-${"f".repeat(64)}`,
      }),
    ]);
    await waitFor(
      () => cdp.evaluate("Boolean(document.querySelector('[data-session-id=\"midnight~%5\"]'))"),
      "the fixture pane was not restored after snapshot cleanup",
    );

    const messagesBeforeRecognitionRace = messageRequests.length;
    await cdp.evaluate(`(() => {
      document.querySelector('[data-session-id="midnight~%5"]').click();
      const input = document.getElementById('message');
      input.value = 'old incarnation speech prefix';
      input.dispatchEvent(new InputEvent('input', { bubbles: true, data: 'x' }));
      document.getElementById('talk').dispatchEvent(new PointerEvent(
        'pointerdown', { bubbles: true, pointerId: 70, pointerType: 'touch' },
      ));
      window.__activeReincarnationRecognition = window.__speechInstances.at(-1);
    })()`);
    emitOverviewPatch([
      mockSession("midnight", "%5", "alpha-planner", "working", {
        instance_id: `pane-v1-${"7".repeat(64)}`,
      }),
    ]);
    await waitFor(
      () => cdp.evaluate("document.getElementById('message').value === ''"),
      "replacement composer did not detach from active recognition",
    );
    const activeRecognitionRace = await cdp.evaluate(`(async () => {
      const recognition = window.__activeReincarnationRecognition;
      recognition.onresult?.({
        resultIndex: 0,
        results: [Object.assign([{ transcript: 'must not reach replacement UI' }], { isFinal: true })],
      });
      document.getElementById('talk').dispatchEvent(new PointerEvent(
        'pointerup', { bubbles: true, pointerId: 70, pointerType: 'touch' },
      ));
      await new Promise((resolve) => setTimeout(resolve, 50));
      return {
        text: document.getElementById('message').value,
        notice: document.getElementById('toast').textContent,
      };
    })()`);
    assert.equal(activeRecognitionRace.text, "", JSON.stringify(activeRecognitionRace));
    assert.match(activeRecognitionRace.notice, /restarted while listening/);
    assert.equal(messageRequests.length, messagesBeforeRecognitionRace,
      "active recognition posted to a replacement pane incarnation");

    messageResponseDelayMs = 500;
    const messagesBeforeQueuedRace = messageRequests.length;
    await cdp.evaluate(`(() => {
      const input = document.getElementById('message');
      input.value = 'busy race send';
      input.dispatchEvent(new InputEvent('input', { bubbles: true, data: 'x' }));
      document.getElementById('send').click();
    })()`);
    await waitFor(
      () => messageRequests.length === messagesBeforeQueuedRace + 1,
      "busy send did not reach the queue race fixture",
    );
    await cdp.evaluate(`(() => {
      const talk = document.getElementById('talk');
      talk.dispatchEvent(new PointerEvent(
        'pointerdown', { bubbles: true, pointerId: 71, pointerType: 'touch' },
      ));
      const recognition = window.__speechInstances.at(-1);
      recognition.onresult({
        resultIndex: 0,
        results: [Object.assign([{ transcript: 'queued stale speech' }], { isFinal: true })],
      });
      talk.dispatchEvent(new PointerEvent(
        'pointerup', { bubbles: true, pointerId: 71, pointerType: 'touch' },
      ));
    })()`);
    emitOverviewPatch([
      mockSession("midnight", "%5", "alpha-planner", "working", {
        instance_id: `pane-v1-${"8".repeat(64)}`,
      }),
    ]);
    await new Promise((resolve) => setTimeout(resolve, 700));
    messageResponseDelayMs = 0;
    assert.equal(messageRequests.filter(({ body }) => body.includes("queued stale speech")).length, 0,
      "queued recognition posted after its pane incarnation was replaced");
    assert.equal(await cdp.evaluate("document.getElementById('message').value"), "",
      "queued recognition mutated the replacement composer");

    const imagesBeforeConversionRace = imageMessageRequests.length;
    await cdp.evaluate(`(() => {
      const input = document.getElementById('message');
      input.value = 'delayed image from old incarnation';
      input.dispatchEvent(new InputEvent('input', { bubbles: true, data: 'x' }));
      const transfer = new DataTransfer();
      transfer.items.add(new File(
        [new Uint8Array([137, 80, 78, 71, 13, 10, 26, 10])],
        'delayed.png',
        { type: 'image/png' },
      ));
      window.__delayNextFileRead = true;
      const picker = document.getElementById('image-input');
      picker.files = transfer.files;
      picker.dispatchEvent(new Event('change', { bubbles: true }));
      document.getElementById('send').click();
    })()`);
    await waitFor(
      () => cdp.evaluate("typeof window.__releaseDelayedFileRead === 'function'"),
      "image conversion did not reach its delayed boundary",
    );
    emitOverviewPatch([
      mockSession("midnight", "%5", "alpha-planner", "working", {
        instance_id: `pane-v1-${"9".repeat(64)}`,
      }),
    ]);
    await cdp.evaluate("window.__releaseDelayedFileRead(); true");
    await waitFor(
      () => cdp.evaluate("!document.getElementById('attachment-clear').disabled"),
      "stale image conversion did not finish",
    );
    const imageConversionRace = await cdp.evaluate(`({
      text: document.getElementById('message').value,
      attachments: document.querySelectorAll('.attachment-preview').length,
      target: document.getElementById('attachment-target').textContent,
      notice: document.getElementById('toast').textContent,
    })`);
    assert.equal(imageMessageRequests.length, imagesBeforeConversionRace,
      "delayed image conversion posted to a replacement pane incarnation");
    assert.equal(imageConversionRace.text, "", JSON.stringify(imageConversionRace));
    assert.equal(imageConversionRace.attachments, 1, JSON.stringify(imageConversionRace));
    assert.match(imageConversionRace.target, /Return to that agent or clear them/);
    assert.match(imageConversionRace.notice, /Images were kept/);
    await cdp.evaluate("document.getElementById('attachment-clear').click(); true");

    // The first launch option is online but cannot launch. The federated
    // `tron~pane` owner remains the contextual target and Home/local cannot be
    // selected accidentally.
    largeLaunchDirectoryFixture = true;
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
    launchSessionResponseDelayMs = 1_000;
    const mobileSearch = await cdp.evaluate(`(async () => {
      const input = document.getElementById('launch-directory');
      const suggestions = document.getElementById('launch-directory-suggestions');
      const nativeFetch = window.fetch;
      let savedSessionAborted = false;
      window.fetch = (resource, options = {}) => {
        if (String(resource).startsWith('/api/v1/launch-sessions?')) {
          options.signal?.addEventListener('abort', () => { savedSessionAborted = true; });
        }
        return nativeFetch.call(window, resource, options);
      };
      input.focus({ preventScroll: true });
      let mutations = 0;
      const observer = new MutationObserver((records) => { mutations += records.length; });
      observer.observe(suggestions, { childList: true });
      const started = performance.now();
      for (const value of [
        'm', 'mo', 'mob', 'mobi', 'mobile', 'mobile-', 'mobile-s',
        'mobile-se', 'mobile-sea', 'mobile-sear', 'mobile-searc',
        'mobile-search', 'mobile-search-', 'mobile-search-1',
        'mobile-search-19', 'mobile-search-199', 'mobile-search-1999',
      ]) {
        input.value = value;
        input.dispatchEvent(new InputEvent('input', { bubbles: true, data: value.at(-1) }));
      }
      await new Promise((resolve) => setTimeout(resolve, 400));
      observer.disconnect();
      const result = {
        elapsed: performance.now() - started,
        nativeList: input.getAttribute('list'),
        comboboxRole: input.getAttribute('role'),
        controls: input.getAttribute('aria-controls'),
        expanded: input.getAttribute('aria-expanded'),
        suggestionCount: suggestions.children.length,
        suggestionDisplay: getComputedStyle(suggestions).display,
        suggestionTapHeight: suggestions.querySelector('button')?.getBoundingClientRect().height || 0,
        overflowX: document.documentElement.scrollWidth - innerWidth,
        mutations,
        machine: document.getElementById('launch-machine').value,
        match: suggestions.querySelector('button')?.dataset.directory || null,
      };
      input.dispatchEvent(new KeyboardEvent('keydown', { key: 'ArrowDown', bubbles: true }));
      result.activeDescendant = input.getAttribute('aria-activedescendant');
      result.activeSelected = document.getElementById(result.activeDescendant)
        ?.getAttribute('aria-selected');
      input.dispatchEvent(new KeyboardEvent('keydown', { key: 'Enter', bubbles: true }));
      result.selected = input.value;
      result.selectedDirectory = input.dataset.selectedDirectory;
      await new Promise((resolve) => setTimeout(resolve, 100));
      input.value = 'no-project-matches-this';
      input.dispatchEvent(new InputEvent('input', { bubbles: true, data: 'x' }));
      await new Promise((resolve) => setTimeout(resolve, 25));
      result.savedSessionAborted = savedSessionAborted;
      result.savedSessionsHidden = document.getElementById('launch-sessions').hidden;
      input.value = 'mobile-search-1';
      input.dispatchEvent(new InputEvent('input', { bubbles: true, data: '1' }));
      await new Promise((resolve) => setTimeout(resolve, 200));
      input.dispatchEvent(new KeyboardEvent('keydown', { key: 'ArrowDown', bubbles: true }));
      result.firstActive = input.getAttribute('aria-activedescendant');
      input.dispatchEvent(new KeyboardEvent('keydown', { key: 'ArrowDown', bubbles: true }));
      result.secondActive = input.getAttribute('aria-activedescendant');
      input.dispatchEvent(new KeyboardEvent('keydown', { key: 'ArrowUp', bubbles: true }));
      result.activeBeforeEscape = input.getAttribute('aria-activedescendant');
      input.dispatchEvent(new KeyboardEvent('keydown', { key: 'Escape', bubbles: true }));
      result.expandedAfterEscape = input.getAttribute('aria-expanded');
      result.activeAfterEscape = input.getAttribute('aria-activedescendant');
      window.fetch = nativeFetch;
      return result;
    })()`);
    launchSessionResponseDelayMs = 0;
    assert.equal(mobileSearch.nativeList, null, JSON.stringify(mobileSearch));
    assert.equal(mobileSearch.comboboxRole, "combobox", JSON.stringify(mobileSearch));
    assert.equal(mobileSearch.controls, "launch-directory-suggestions", JSON.stringify(mobileSearch));
    assert.equal(mobileSearch.expanded, "true", JSON.stringify(mobileSearch));
    assert.ok(mobileSearch.suggestionCount <= 40, JSON.stringify(mobileSearch));
    assert.equal(mobileSearch.suggestionDisplay, "grid", JSON.stringify(mobileSearch));
    assert.ok(mobileSearch.suggestionTapHeight >= 44, JSON.stringify(mobileSearch));
    assert.ok(mobileSearch.overflowX <= 1, JSON.stringify(mobileSearch));
    assert.ok(mobileSearch.mutations <= 2, JSON.stringify(mobileSearch));
    assert.ok(mobileSearch.elapsed < 1_500, JSON.stringify(mobileSearch));
    assert.equal(mobileSearch.machine, "tron", JSON.stringify(mobileSearch));
    assert.equal(mobileSearch.match, "/workspace/mobile-search-1999", JSON.stringify(mobileSearch));
    assert.equal(mobileSearch.activeDescendant, "launch-directory-suggestion-0", JSON.stringify(mobileSearch));
    assert.equal(mobileSearch.activeSelected, "true", JSON.stringify(mobileSearch));
    assert.equal(mobileSearch.selected, "/workspace/mobile-search-1999", JSON.stringify(mobileSearch));
    assert.equal(mobileSearch.selectedDirectory, "/workspace/mobile-search-1999", JSON.stringify(mobileSearch));
    assert.equal(mobileSearch.savedSessionAborted, true, JSON.stringify(mobileSearch));
    assert.equal(mobileSearch.savedSessionsHidden, true, JSON.stringify(mobileSearch));
    assert.ok(mobileSearch.firstActive, JSON.stringify(mobileSearch));
    assert.notEqual(mobileSearch.secondActive, mobileSearch.firstActive, JSON.stringify(mobileSearch));
    assert.equal(mobileSearch.activeBeforeEscape, mobileSearch.firstActive, JSON.stringify(mobileSearch));
    assert.equal(mobileSearch.expandedAfterEscape, "false", JSON.stringify(mobileSearch));
    assert.equal(mobileSearch.activeAfterEscape, null, JSON.stringify(mobileSearch));
    const mouseTarget = await cdp.evaluate(`(async () => {
      const input = document.getElementById('launch-directory');
      input.focus({ preventScroll: true });
      input.value = 'mobile-search-1666';
      input.dispatchEvent(new InputEvent('input', { bubbles: true, data: '6' }));
      await new Promise((resolve) => setTimeout(resolve, 250));
      const button = document.querySelector('#launch-directory-suggestions [role=option]');
      const box = button.getBoundingClientRect();
      return { x: box.left + box.width / 2, y: box.top + box.height / 2 };
    })()`);
    await cdp.send("Input.dispatchMouseEvent", {
      type: "mousePressed", x: mouseTarget.x, y: mouseTarget.y, button: "left", clickCount: 1,
    });
    await new Promise((resolve) => setTimeout(resolve, 50));
    assert.deepEqual(await cdp.evaluate(`(() => {
      const input = document.getElementById('launch-directory');
      return {
        active: document.activeElement?.id || null,
        expanded: input.getAttribute('aria-expanded'),
        hidden: document.getElementById('launch-directory-suggestions').hidden,
        value: input.value,
      };
    })()`), {
      active: "launch-directory",
      expanded: "true",
      hidden: false,
      value: "mobile-search-1666",
    });
    await cdp.send("Input.dispatchMouseEvent", {
      type: "mouseReleased", x: mouseTarget.x, y: mouseTarget.y, button: "left", clickCount: 1,
    });
    await new Promise((resolve) => setTimeout(resolve, 50));
    assert.deepEqual(await cdp.evaluate(`(() => {
      const input = document.getElementById('launch-directory');
      return { value: input.value, selectedDirectory: input.dataset.selectedDirectory };
    })()`), {
      value: "/workspace/mobile-search-1666",
      selectedDirectory: "/workspace/mobile-search-1666",
    });
    await cdp.send("Emulation.setTouchEmulationEnabled", { enabled: true, maxTouchPoints: 5 });
    await cdp.send("Emulation.setDeviceMetricsOverride", {
      width: 390, height: 844, deviceScaleFactor: 1, mobile: true,
    });
    const touchDrag = await cdp.evaluate(`(async () => {
      const input = document.getElementById('launch-directory');
      input.focus({ preventScroll: true });
      input.value = 'mobile-search';
      input.dispatchEvent(new InputEvent('input', { bubbles: true, data: 'h' }));
      await new Promise((resolve) => setTimeout(resolve, 250));
      const suggestions = document.getElementById('launch-directory-suggestions');
      suggestions.scrollTop = 0;
      const box = suggestions.getBoundingClientRect();
      window.__folderTouchEvents = [];
      for (const name of ['pointerdown', 'pointermove', 'pointerup', 'pointercancel', 'touchstart', 'touchmove', 'touchend', 'touchcancel']) {
        suggestions.addEventListener(name, (event) => {
          window.__folderTouchEvents.push([name, event.defaultPrevented]);
        });
      }
      const x = Math.round(box.left + box.width / 2);
      const visibleY = [];
      for (let y = Math.max(0, Math.ceil(box.top) + 4); y < Math.min(innerHeight, Math.floor(box.bottom) - 4); y += 8) {
        if (suggestions.contains(document.elementFromPoint(x, y))) visibleY.push(y);
      }
      return {
        x,
        startY: visibleY.at(-1) || 0,
        endY: visibleY[0] || 0,
        before: suggestions.scrollTop,
        value: input.value,
        selectedDirectory: input.dataset.selectedDirectory,
        scrollHeight: suggestions.scrollHeight,
        clientHeight: suggestions.clientHeight,
        visiblePoints: visibleY.length,
      };
    })()`);
    assert.ok(touchDrag.scrollHeight > touchDrag.clientHeight, JSON.stringify(touchDrag));
    assert.ok(touchDrag.visiblePoints > 8, JSON.stringify(touchDrag));
    await cdp.send("Input.dispatchTouchEvent", {
      type: "touchStart",
      touchPoints: [{ x: touchDrag.x, y: touchDrag.startY, radiusX: 4, radiusY: 4, force: 1 }],
    });
    await new Promise((resolve) => setTimeout(resolve, 30));
    for (const y of [touchDrag.startY - 40, touchDrag.startY - 80, touchDrag.endY]) {
      await cdp.send("Input.dispatchTouchEvent", {
        type: "touchMove",
        touchPoints: [{ x: touchDrag.x, y, radiusX: 4, radiusY: 4, force: 1 }],
      });
      await new Promise((resolve) => setTimeout(resolve, 30));
    }
    await cdp.send("Input.dispatchTouchEvent", { type: "touchEnd", touchPoints: [] });
    await new Promise((resolve) => setTimeout(resolve, 100));
    const touchDragResult = await cdp.evaluate(`(() => {
      const input = document.getElementById('launch-directory');
      const suggestions = document.getElementById('launch-directory-suggestions');
      return {
        scrollTop: suggestions.scrollTop,
        value: input.value,
        selectedDirectory: input.dataset.selectedDirectory,
        expanded: input.getAttribute('aria-expanded'),
        events: window.__folderTouchEvents,
      };
    })()`);
    assert.ok(
      touchDragResult.scrollTop > touchDrag.before,
      JSON.stringify({ touchDrag, touchDragResult }),
    );
    assert.equal(touchDragResult.value, touchDrag.value, JSON.stringify(touchDragResult));
    assert.equal(touchDragResult.selectedDirectory, touchDrag.selectedDirectory, JSON.stringify(touchDragResult));
    assert.equal(touchDragResult.expanded, "true", JSON.stringify(touchDragResult));
    assert.deepEqual(
      touchDragResult.events.find(([name]) => name === "pointerdown"),
      ["pointerdown", false],
      JSON.stringify(touchDragResult),
    );
    assert.ok(
      touchDragResult.events.some(([name]) => name === "pointermove"),
      JSON.stringify(touchDragResult),
    );
    const touchTarget = await cdp.evaluate(`(async () => {
      const input = document.getElementById('launch-directory');
      input.value = 'mobile-search-1777';
      input.dispatchEvent(new InputEvent('input', { bubbles: true, data: '7' }));
      await new Promise((resolve) => setTimeout(resolve, 250));
      const button = document.querySelector('#launch-directory-suggestions [role=option]');
      const box = button.getBoundingClientRect();
      return { x: box.left + box.width / 2, y: box.top + box.height / 2 };
    })()`);
    await cdp.send("Input.dispatchTouchEvent", {
      type: "touchStart",
      touchPoints: [{ x: touchTarget.x, y: touchTarget.y, radiusX: 4, radiusY: 4, force: 1 }],
    });
    await new Promise((resolve) => setTimeout(resolve, 50));
    await cdp.send("Input.dispatchTouchEvent", { type: "touchEnd", touchPoints: [] });
    await new Promise((resolve) => setTimeout(resolve, 100));
    assert.deepEqual(await cdp.evaluate(`(() => {
      const input = document.getElementById('launch-directory');
      return { value: input.value, selectedDirectory: input.dataset.selectedDirectory };
    })()`), {
      value: "/workspace/mobile-search-1777",
      selectedDirectory: "/workspace/mobile-search-1777",
    });
    await cdp.send("Emulation.setDeviceMetricsOverride", {
      width: 1024, height: 768, deviceScaleFactor: 1, mobile: true,
    });
    const wideTouchSearch = await cdp.evaluate(`(async () => {
      const input = document.getElementById('launch-directory');
      input.focus({ preventScroll: true });
      input.value = 'mobile-search-1888';
      input.dispatchEvent(new InputEvent('input', { bubbles: true, data: '8' }));
      await new Promise((resolve) => setTimeout(resolve, 250));
      const suggestions = document.getElementById('launch-directory-suggestions');
      return {
        wideLayout: !matchMedia('(max-width: 720px)').matches,
        touchPoints: navigator.maxTouchPoints,
        nativeList: input.getAttribute('list'),
        role: input.getAttribute('role'),
        expanded: input.getAttribute('aria-expanded'),
        count: suggestions.children.length,
        match: suggestions.querySelector('[role=option]')?.dataset.directory || null,
      };
    })()`);
    assert.deepEqual(wideTouchSearch, {
      wideLayout: true,
      touchPoints: 5,
      nativeList: null,
      role: "combobox",
      expanded: "true",
      count: 1,
      match: "/workspace/mobile-search-1888",
    });
    await cdp.send("Emulation.setDeviceMetricsOverride", {
      width: 768, height: 1024, deviceScaleFactor: 1, mobile: true,
    });
    assert.deepEqual(await cdp.evaluate(`({
      wideLayout: !matchMedia('(max-width: 720px)').matches,
      nativeList: document.getElementById('launch-directory').getAttribute('list'),
      role: document.getElementById('launch-directory').getAttribute('role'),
      match: document.querySelector('#launch-directory-suggestions [role=option]')?.dataset.directory || null,
    })`), {
      wideLayout: true,
      nativeList: null,
      role: "combobox",
      match: "/workspace/mobile-search-1888",
    });
    await cdp.send("Emulation.setTouchEmulationEnabled", { enabled: false });
    await cdp.send("Emulation.setDeviceMetricsOverride", {
      width: 390, height: 844, deviceScaleFactor: 1, mobile: false,
    });
    await cdp.evaluate("document.querySelector('#launch-dialog .dialog-cancel').click(); true");
    largeLaunchDirectoryFixture = false;
    launchSessionRequests.length = 0;

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
    assert.deepEqual(machineLabels, ["Tron", "Midnight", "Clue"]);
    // An unwrappable rail line (an offline machine's long health message) must
    // not widen the body's grid column past the phone. It used to: the implicit
    // `auto` column grew to the rail's min-content, and `overflow: hidden` then
    // clipped the topbar and rail at the right edge with no way to scroll.
    const landingFit = await cdp.evaluate(`(() => {
      const width = (selector) => Math.round(document.querySelector(selector).getBoundingClientRect().width);
      const rail = document.querySelector('.rail');
      const status = [...document.querySelectorAll('.machine-header')]
        .find((node) => node.querySelector('.machine-label')?.textContent === 'Clue')
        .querySelector('.machine-status');
      return {
        innerWidth,
        topbar: width('.topbar'),
        workspace: width('.workspace'),
        rail: width('.rail'),
        railOverflow: rail.scrollWidth - rail.clientWidth,
        statusRight: Math.round(status.getBoundingClientRect().right),
        statusTruncated: status.scrollWidth > status.clientWidth,
        statusText: status.textContent,
      };
    })()`);
    assert.equal(landingFit.topbar, landingFit.innerWidth, JSON.stringify(landingFit));
    assert.equal(landingFit.workspace, landingFit.innerWidth, JSON.stringify(landingFit));
    assert.equal(landingFit.rail, landingFit.innerWidth, JSON.stringify(landingFit));
    assert.ok(landingFit.railOverflow <= 0, JSON.stringify(landingFit));
    assert.ok(landingFit.statusRight <= landingFit.innerWidth, JSON.stringify(landingFit));
    assert.equal(landingFit.statusTruncated, true, JSON.stringify(landingFit));
    assert.equal(landingFit.statusText, `Offline · ${LONG_MACHINE_HEALTH}`);

    // A verified release is announced on the landing page with one compact
    // pill and one action, and both still fit a 390px phone.
    await waitFor(
      () => cdp.evaluate("!document.getElementById('update-all-open').hidden"),
      "the Update all action never appeared for a verified release",
    );
    const updateLanding = await cdp.evaluate(`(() => {
      const rail = document.querySelector('.rail');
      const pillFor = (label) => [...document.querySelectorAll('.machine-header')]
        .find((node) => node.querySelector('.machine-label')?.textContent === label)
        ?.querySelector('.machine-update-pill');
      const updateAll = document.getElementById('update-all-open');
      return {
        innerWidth,
        tronPill: pillFor('Tron').hidden ? '' : pillFor('Tron').textContent,
        midnightPill: pillFor('Midnight').hidden ? '' : pillFor('Midnight').textContent,
        cluePill: pillFor('Clue').hidden ? '' : pillFor('Clue').textContent,
        updateAllText: updateAll.textContent,
        updateAllRight: Math.round(updateAll.getBoundingClientRect().right),
        railOverflow: rail.scrollWidth - rail.clientWidth,
        documentOverflow: document.documentElement.scrollWidth - innerWidth,
      };
    })()`);
    assert.equal(updateLanding.tronPill, "↑ v0.3.0", JSON.stringify(updateLanding));
    assert.equal(updateLanding.midnightPill, "", JSON.stringify(updateLanding));
    assert.equal(updateLanding.cluePill, "", JSON.stringify(updateLanding));
    assert.equal(updateLanding.updateAllText, "↑ Update all (1)", JSON.stringify(updateLanding));
    assert.ok(updateLanding.updateAllRight <= updateLanding.innerWidth, JSON.stringify(updateLanding));
    assert.ok(updateLanding.railOverflow <= 0, JSON.stringify(updateLanding));
    assert.ok(updateLanding.documentOverflow <= 1, JSON.stringify(updateLanding));

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

    const softwareCard = await cdp.evaluate(`(() => {
      const card = document.getElementById('machine-software');
      const button = (name) => card.querySelector('[data-update-action="' + name + '"]');
      return {
        heading: card.querySelector('h2').textContent,
        version: card.querySelector('.software-version').textContent,
        latest: card.querySelector('.software-latest').textContent,
        actions: [...card.querySelectorAll('button')].map((node) => ({
          label: node.textContent, action: node.dataset.updateAction, disabled: node.disabled,
        })),
        machineOnButtons: [...card.querySelectorAll('button')].map((node) => node.dataset.machineId),
        updateEnabled: !button('apply').disabled,
        rollbackEnabled: !button('rollback').disabled,
        cardOverflow: card.scrollWidth - card.clientWidth,
        documentOverflow: document.documentElement.scrollWidth - innerWidth,
      };
    })()`);
    assert.equal(softwareCard.heading, "Software");
    assert.equal(softwareCard.version, "atmux v0.2.0 · x86_64-unknown-linux-gnu");
    assert.match(softwareCard.latest, /^v0\.3\.0 available · verified · published /);
    assert.deepEqual(softwareCard.actions.map((item) => item.action), ["check", "apply", "rollback"]);
    assert.deepEqual(softwareCard.actions.map((item) => item.label), ["Check now", "Update", "Roll back"]);
    assert.deepEqual(softwareCard.machineOnButtons, ["tron", "tron", "tron"]);
    assert.equal(softwareCard.updateEnabled, true, JSON.stringify(softwareCard));
    // Tron kept no previous executable, so a rollback is never offered.
    assert.equal(softwareCard.rollbackEnabled, false, JSON.stringify(softwareCard));
    assert.ok(softwareCard.cardOverflow <= 1, JSON.stringify(softwareCard));
    assert.ok(softwareCard.documentOverflow <= 1, JSON.stringify(softwareCard));

    // Update is confirmed before anything is sent, and the confirmation says
    // exactly what happens to the agents running on that machine.
    await cdp.evaluate("document.querySelector('#machine-software [data-update-action=\"apply\"]').click(); true");
    await waitFor(
      () => cdp.evaluate("document.getElementById('update-dialog').open"),
      "the update confirmation did not open",
    );
    assert.deepEqual(await cdp.evaluate(`({
      title: document.getElementById('update-dialog-title').textContent,
      target: document.getElementById('update-dialog-target').textContent,
      note: document.getElementById('update-dialog-note').textContent,
      confirm: document.getElementById('update-confirm').textContent,
    })`), {
      title: "Install the new atmux?",
      target: "Install the newest verified atmux on Tron.",
      note: "atmux restarts on Tron; agent sessions keep running in tmux.",
      confirm: "Update",
    });
    assert.equal(fleetUpdateRequests.length, 0, "no verb may be sent before the confirmation");
    await cdp.evaluate("document.getElementById('update-confirm').click(); true");
    await waitFor(
      () => Promise.resolve(fleetUpdateRequests.length > 0),
      "the confirmed update never reached the coordinator",
    );
    assert.deepEqual(
      fleetUpdateRequests.map(({ machine, action, body }) => ({ machine, action, body })),
      [{ machine: "tron", action: "apply", body: "{}" }],
    );
    await waitFor(
      () => cdp.evaluate("(document.querySelector('#machine-software .software-state')?.textContent || '').includes('Restarting')"),
      "the Software card never reported the restart",
    );
    const restarting = await cdp.evaluate(`(() => {
      const card = document.getElementById('machine-software');
      return {
        state: card.querySelector('.software-state').textContent,
        updateDisabled: card.querySelector('[data-update-action="apply"]').disabled,
        checkDisabled: card.querySelector('[data-update-action="check"]').disabled,
        dialogOpen: document.getElementById('update-dialog').open,
      };
    })()`);
    assert.equal(restarting.state, "Restarting into the new version…");
    assert.equal(restarting.updateDisabled, true, JSON.stringify(restarting));
    assert.equal(restarting.checkDisabled, true, JSON.stringify(restarting));
    assert.equal(restarting.dialogOpen, false, JSON.stringify(restarting));

    await cdp.evaluate("document.getElementById('machine-mobile-back').click(); true");
    await waitFor(
      () => cdp.evaluate("document.getElementById('update-all-open').hidden"),
      "Update all stayed offered while the only candidate was restarting",
    );

    // Rolling back restarts the node too, so it is confirmed the same way and
    // sends nothing until the operator agrees.
    await cdp.evaluate(`(() => {
      [...document.querySelectorAll('.machine-header')]
        .find((node) => node.querySelector('.machine-label')?.textContent === 'Midnight')
        .click();
      return true;
    })()`);
    await waitFor(
      () => cdp.evaluate("!document.getElementById('machine-view').hidden && document.getElementById('machine-name').textContent === 'Midnight'"),
      "Midnight's machine view did not open",
    );
    await waitFor(
      () => cdp.evaluate("document.querySelector('#machine-software [data-update-action=\"rollback\"]') !== null && !document.querySelector('#machine-software [data-update-action=\"rollback\"]').disabled"),
      "Roll back was never offered for a machine with a previous executable",
    );
    const requestsBeforeRollback = fleetUpdateRequests.length;
    await cdp.evaluate("document.querySelector('#machine-software [data-update-action=\"rollback\"]').click(); true");
    await waitFor(
      () => cdp.evaluate("document.getElementById('update-dialog').open"),
      "the rollback confirmation did not open",
    );
    assert.deepEqual(await cdp.evaluate(`({
      title: document.getElementById('update-dialog-title').textContent,
      target: document.getElementById('update-dialog-target').textContent,
      note: document.getElementById('update-dialog-note').textContent,
      confirm: document.getElementById('update-confirm').textContent,
    })`), {
      title: "Roll back atmux?",
      target: "Restore the previously installed atmux on Midnight.",
      note: "atmux restarts on Midnight; agent sessions keep running in tmux.",
      confirm: "Roll back",
    });
    assert.equal(
      fleetUpdateRequests.length,
      requestsBeforeRollback,
      "a rollback must not reach the coordinator before it is confirmed",
    );
    await cdp.evaluate("document.querySelector('#update-dialog .dialog-cancel').click(); true");
    await waitFor(
      () => cdp.evaluate("!document.getElementById('update-dialog').open"),
      "the rollback confirmation did not close",
    );
    assert.equal(
      fleetUpdateRequests.length,
      requestsBeforeRollback,
      "cancelling a rollback must send nothing",
    );
    await cdp.evaluate("document.getElementById('machine-mobile-back').click(); true");
    await waitFor(
      () => cdp.evaluate("document.getElementById('machine-view').hidden"),
      "the machine view did not close after the rollback confirmation",
    );
    // The landing page still fits the phone with an update in flight.
    const afterUpdateLanding = await cdp.evaluate(`(() => {
      const rail = document.querySelector('.rail');
      const width = (selector) => Math.round(document.querySelector(selector).getBoundingClientRect().width);
      return {
        innerWidth,
        topbar: width('.topbar'),
        workspace: width('.workspace'),
        rail: width('.rail'),
        railOverflow: rail.scrollWidth - rail.clientWidth,
        documentOverflow: document.documentElement.scrollWidth - innerWidth,
      };
    })()`);
    assert.equal(afterUpdateLanding.topbar, afterUpdateLanding.innerWidth, JSON.stringify(afterUpdateLanding));
    assert.equal(afterUpdateLanding.workspace, afterUpdateLanding.innerWidth, JSON.stringify(afterUpdateLanding));
    assert.equal(afterUpdateLanding.rail, afterUpdateLanding.innerWidth, JSON.stringify(afterUpdateLanding));
    assert.ok(afterUpdateLanding.railOverflow <= 0, JSON.stringify(afterUpdateLanding));
    assert.ok(afterUpdateLanding.documentOverflow <= 1, JSON.stringify(afterUpdateLanding));

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
    assert.deepEqual(quickActions.actions, ["Duplicate agent", "Copy agent link", "Restart session", "Compact", "Download raw output", "Ctrl+B ×2", "Interrupt", "Kill agent"]);
    assert.equal(quickActions.compactInComposer, false, JSON.stringify(quickActions));

    const keyLayout = await cdp.evaluate(`(() => ({
      labels: [...document.querySelectorAll('[data-pane-key]')].map((button) => button.getAttribute('aria-label')),
      targets: [...document.querySelectorAll('[data-pane-key]')].map((button) => {
        const box = button.getBoundingClientRect();
        return { width: box.width, height: box.height };
      }),
      dialogOverflow: document.getElementById('quick-actions-dialog').scrollWidth
        - document.getElementById('quick-actions-dialog').clientWidth,
    }))()`);
    assert.deepEqual(keyLayout.labels, [
      "Send Up arrow", "Send Left arrow", "Send Down arrow", "Send Right arrow", "Send blank Enter",
    ]);
    assert.ok(keyLayout.targets.every(({ width, height }) => width >= 44 && height >= 44), JSON.stringify(keyLayout));
    assert.ok(keyLayout.dialogOverflow <= 1, JSON.stringify(keyLayout));

    // Interactive keys use their own generation-bound route and a bounded
    // ordered queue. Deliberate rapid taps remain distinct without touching
    // the per-agent composer draft.
    const messagesBeforePaneKeys = messageRequests.length;
    nextSpecialKeyResponseDelayMs = 250;
    await cdp.evaluate(`(() => {
      const input = document.getElementById('message');
      input.value = 'draft stays with this agent';
      input.setSelectionRange(6, 11);
      input.dispatchEvent(new Event('input', { bubbles: true }));
      const down = document.querySelector('[data-pane-key="down"]');
      down.click(); down.click(); down.click();
      document.querySelector('[data-pane-key="enter"]').click();
      return true;
    })()`);
    await waitFor(() => specialKeyRequests.length === 1, "the first delayed Down was not dispatched");
    await new Promise((resolve) => setTimeout(resolve, 60));
    assert.equal(specialKeyRequests.length, 1, "queued keys were dispatched concurrently");
    const queuedStatus = await cdp.evaluate(`({
      status: document.getElementById('quick-pane-key-status').textContent,
      keyDisabled: document.querySelector('[data-pane-key="down"]').disabled,
      duplicateDisabled: document.getElementById('quick-duplicate').disabled,
      busy: document.querySelector('.quick-pane-keypad').getAttribute('aria-busy'),
    })`);
    assert.match(queuedStatus.status, /4 keys sending or queued/);
    assert.equal(queuedStatus.keyDisabled, false, JSON.stringify(queuedStatus));
    assert.equal(queuedStatus.duplicateDisabled, false, JSON.stringify(queuedStatus));
    assert.equal(queuedStatus.busy, "true", JSON.stringify(queuedStatus));
    await waitFor(() => specialKeyRequests.length === 4, "Down×3 then Enter did not drain in order");
    const draftAfterArrow = await cdp.evaluate(`(() => {
      const input = document.getElementById('message');
      return { value: input.value, start: input.selectionStart, end: input.selectionEnd };
    })()`);
    assert.deepEqual(draftAfterArrow, { value: "draft stays with this agent", start: 6, end: 11 });
    assert.equal(messageRequests.length, messagesBeforePaneKeys, "a pane key must not become a chat message");

    assert.equal(await cdp.evaluate("document.getElementById('message').value"), "draft stays with this agent");
    await cdp.evaluate(`(() => {
      const input = document.getElementById('message');
      input.value = '';
      input.dispatchEvent(new Event('input', { bubbles: true }));
      document.querySelector('[data-pane-key="enter"]').click();
      return true;
    })()`);
    await waitFor(() => specialKeyRequests.length === 5, "blank Enter was disabled by an empty composer");
    await waitFor(
      () => cdp.evaluate("!document.querySelector('[data-pane-key=\"up\"]').disabled"),
      "key controls stayed busy after blank Enter",
    );

    // Native button semantics provide keyboard activation without installing
    // a page-level arrow-key handler that could steal textarea cursor keys.
    await cdp.evaluate("document.querySelector('[data-pane-key=\"up\"]').focus(); true");
    await cdp.send("Input.dispatchKeyEvent", {
      type: "rawKeyDown", key: " ", code: "Space", windowsVirtualKeyCode: 32, nativeVirtualKeyCode: 32,
    });
    await cdp.send("Input.dispatchKeyEvent", {
      type: "keyUp", key: " ", code: "Space", windowsVirtualKeyCode: 32, nativeVirtualKeyCode: 32,
    });
    await waitFor(() => specialKeyRequests.length === 6, "keyboard activation did not send Up");
    for (const action of ["left", "right"]) {
      await waitFor(
        () => cdp.evaluate(`!document.querySelector('[data-pane-key="${action}"]').disabled`),
        `${action} arrow stayed disabled after the prior request`,
      );
      await cdp.evaluate(`document.querySelector('[data-pane-key="${action}"]').click(); true`);
      await waitFor(
        () => specialKeyRequests.length === (action === "left" ? 7 : 8),
        `${action} arrow was not delivered`,
      );
    }
    const expectedInstance = `pane-v1-${"1".repeat(64)}`;
    assert.deepEqual(specialKeyRequests, ["down", "down", "down", "enter", "enter", "up", "left", "right"].map((action) => ({
      paneId: "tron~%100",
      body: { action, machine: "tron", instance_id: expectedInstance },
    })));
    assert.equal(messageRequests.length, messagesBeforePaneKeys, "blank Enter must bypass the empty chat composer");
    await waitFor(
      () => cdp.evaluate("document.querySelector('.quick-pane-keypad').getAttribute('aria-busy') === 'false'"),
      "the prior key queue did not become idle before the cap check",
    );

    // The cap includes the in-flight request. Only this target's key buttons
    // disable at the cap; the rest of Quick actions remains usable.
    let releaseCappedKey;
    nextSpecialKeyResponseGate = new Promise((resolveGate) => { releaseCappedKey = resolveGate; });
    await cdp.evaluate(`(() => {
      const down = document.querySelector('[data-pane-key="down"]');
      for (let index = 0; index < 20; index += 1) down.click();
      return true;
    })()`);
    await waitFor(
      () => cdp.evaluate("document.getElementById('quick-pane-key-status').textContent.includes('full (16)')"),
      "the pane-key queue did not expose its cap",
    );
    const capState = await cdp.evaluate(`({
      keyDisabled: document.querySelector('[data-pane-key="down"]').disabled,
      duplicateDisabled: document.getElementById('quick-duplicate').disabled,
    })`);
    assert.equal(capState.keyDisabled, true, JSON.stringify(capState));
    assert.equal(capState.duplicateDisabled, false, JSON.stringify(capState));
    await new Promise((resolve) => setTimeout(resolve, 60));
    assert.equal(specialKeyRequests.length, 9, "a delayed key allowed concurrent queue dispatch");
    releaseCappedKey();
    await waitFor(() => specialKeyRequests.length === 24, "the bounded queue did not drain exactly 16 taps");
    await waitFor(
      () => cdp.evaluate("!document.querySelector('[data-pane-key=\"down\"]').disabled"),
      "key controls stayed capped after the queue drained",
    );

    // A conflict invalidates only the queued events for that exact pane
    // generation. Keys captured after switching agents retain their original
    // pane and machine while the first target is still in flight.
    nextSpecialKeyStatus = 409;
    nextSpecialKeyResponseDelayMs = 250;
    await cdp.evaluate(`(() => {
      const down = document.querySelector('[data-pane-key="down"]');
      down.click(); down.click(); down.click();
      document.getElementById('quick-actions-dialog').close();
      document.querySelector('[data-session-id="midnight~%5"]').click();
      document.getElementById('quick-actions-open').click();
      document.querySelector('[data-pane-key="left"]').click();
      document.querySelector('[data-pane-key="enter"]').click();
      return true;
    })()`);
    await waitFor(() => specialKeyRequests.length === 27, "the cross-agent queue did not finish safely");
    const midnightInstance = `pane-v1-${"9".repeat(64)}`;
    assert.deepEqual(specialKeyRequests.slice(24), [
      { paneId: "tron~%100", body: { action: "down", machine: "tron", instance_id: expectedInstance } },
      { paneId: "midnight~%5", body: { action: "left", machine: "midnight", instance_id: midnightInstance } },
      { paneId: "midnight~%5", body: { action: "enter", machine: "midnight", instance_id: midnightInstance } },
    ]);
    await waitFor(
      () => cdp.evaluate("document.getElementById('quick-pane-key-status').textContent.includes('Sent blank Enter')"),
      "the switched agent did not receive a completed queue status",
    );
    assert.match(
      await cdp.evaluate("document.getElementById('quick-pane-key-status').textContent"),
      /Sent blank Enter/,
    );
    await cdp.evaluate(`(() => {
      document.getElementById('quick-actions-dialog').close();
      document.querySelector('[data-session-id="tron~%100"]').click();
      document.getElementById('quick-actions-open').click();
      return true;
    })()`);
    const conflictStatus = await cdp.evaluate("document.getElementById('quick-pane-key-status').textContent");
    assert.match(conflictStatus, /agent changed/);
    assert.match(conflictStatus, /2 queued keys were discarded/);

    // A new browser must not fall back to an old coordinator's unbound
    // /special-keys route. Its 404 is actionable mixed-version guidance.
    simulateOldCoordinatorInputRoute = true;
    await cdp.evaluate("document.querySelector('[data-pane-key=\"right\"]').click(); true");
    await waitFor(
      () => cdp.evaluate("document.getElementById('quick-pane-key-status').textContent.includes('out of sync')"),
      "an old coordinator did not produce refresh guidance",
    );
    assert.equal(specialKeyRequests.length, 27, "an old coordinator accepted a generation-bound key");
    assert.equal(legacySpecialKeyRequests.length, 0, "the new browser fell back to legacy special-keys");

    nextSpecialKeyStatus = 422;
    await cdp.evaluate("document.querySelector('[data-pane-key=\"down\"]').click(); true");
    await waitFor(() => specialKeyRequests.length === 28, "the mixed-version fixture did not receive the key");
    await waitFor(
      () => cdp.evaluate("document.getElementById('quick-pane-key-status').textContent.includes('out of sync')"),
      "a mixed-version key rejection did not provide refresh guidance",
    );
    assert.match(
      await cdp.evaluate("document.getElementById('quick-pane-key-status').textContent"),
      /Refresh after the server updates/,
    );
    assert.equal(legacySpecialKeyRequests.length, 0, "generation-bound keys used the legacy route");

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

    // Source navigation: plain taps select a name and show the symbol panel,
    // same-file definitions jump without asking the owner, imported names
    // resolve through the owner, Back returns, and ambiguous definitions list.
    await cdp.evaluate("document.querySelector('#file-viewer .project-viewer-back').click(); true");
    await cdp.evaluate(`(() => {
      [...document.querySelectorAll('#files-list .project-entry')].find((entry) => entry.textContent.includes('nav.ts')).click();
      return true;
    })()`);
    await waitFor(
      () => cdp.evaluate("document.querySelectorAll('#file-viewer .code-line').length === 6 && Boolean(document.querySelector('#file-viewer .code-symbol[data-symbol=\"total\"]'))"),
      "navigable TypeScript source did not render",
    );
    const navigationRequestsBefore = codeNavRequests.length;
    const selected = await cdp.evaluate(`(() => {
      const usage = document.querySelector('#file-viewer .code-line[data-line="6"] .code-symbol[data-symbol="total"]');
      usage.click();
      const panel = document.querySelector('#file-viewer .code-symbol-panel');
      return {
        name: panel?.querySelector('.code-symbol-name')?.textContent,
        count: panel?.querySelector('.code-symbol-count')?.textContent,
        matches: document.querySelectorAll('#file-viewer .code-symbol.symbol-match').length,
        origin: usage.classList.contains('symbol-origin'),
        keyword: document.querySelector('#file-viewer .code-line[data-line="2"] .syntax-keyword')?.textContent,
        imported: document.querySelector('#file-viewer .code-line[data-line="1"] .code-import')?.dataset.importSpec,
      };
    })()`);
    assert.deepEqual(selected, {
      name: "total", count: "2 in this file", matches: 2, origin: true, keyword: "export", imported: "./util",
    });
    await cdp.evaluate("document.querySelector('#file-viewer .code-go-definition').click(); true");
    await waitFor(
      () => cdp.evaluate("document.querySelector('#file-viewer .code-line[data-line=\"2\"]').classList.contains('code-line-flash')"),
      "same-file definition was not revealed",
    );
    assert.equal(codeNavRequests.length, navigationRequestsBefore, "a same-file definition must not query the owner");
    await cdp.evaluate(`(() => {
      const call = document.querySelector('#file-viewer .code-line[data-line="4"] .code-symbol[data-symbol="formatTotal"]');
      call.dispatchEvent(new MouseEvent('click', { bubbles: true, cancelable: true, ctrlKey: true }));
      return true;
    })()`);
    await waitFor(
      () => cdp.evaluate("document.querySelector('#file-viewer .code-viewer-path')?.textContent === 'src/util.ts' && document.querySelectorAll('#file-viewer .code-line').length === 3"),
      "Ctrl-click on an imported name did not open its module",
    );
    await waitFor(
      () => cdp.evaluate("Boolean(document.querySelector('#file-viewer .code-line[data-line=\"1\"] .code-symbol.symbol-origin[data-symbol=\"formatTotal\"]'))"),
      "the imported declaration was not selected after the jump",
    );
    assert.deepEqual(codeNavRequests.at(-1), {
      operation: "resolve", pane: "tron~%100", symbol: "formatTotal", path: "src/nav.ts", spec: "./util",
    });
    assert.equal(await cdp.evaluate("document.querySelector('#file-viewer .code-history-back').disabled"), false);
    await cdp.evaluate("document.querySelector('#file-viewer .code-history-back').click(); true");
    await waitFor(
      () => cdp.evaluate("document.querySelector('#file-viewer .code-viewer-path')?.textContent === 'src/nav.ts' && document.querySelectorAll('#file-viewer .code-line').length === 6"),
      "Back did not return to the previous file",
    );
    assert.equal(await cdp.evaluate("document.querySelector('#file-viewer .code-history-forward').disabled"), false);
    await cdp.evaluate(`(() => {
      document.querySelector('#file-viewer .code-line[data-line="3"] .code-symbol[data-symbol="reduce"]').click();
      document.querySelector('#file-viewer .code-go-definition').click();
      return true;
    })()`);
    await waitFor(
      () => cdp.evaluate("document.querySelectorAll('#file-viewer .code-symbol-result').length === 2"),
      "ambiguous definitions were not listed",
    );
    const listed = await cdp.evaluate(`(() => ({
      status: document.querySelector('#file-viewer .code-symbol-status')?.textContent,
      locations: [...document.querySelectorAll('#file-viewer .code-symbol-location')].map((node) => node.textContent),
      markup: Boolean(document.querySelector('#file-viewer .code-symbol-panel script')),
    }))()`);
    assert.equal(listed.status, "2 candidate definitions");
    assert.deepEqual(listed.locations, ["src/util.ts:2", "src/app.js:3"]);
    assert.equal(listed.markup, false);
    assert.deepEqual(codeNavRequests.at(-1), {
      operation: "definitions", pane: "tron~%100", symbol: "reduce", path: "src/nav.ts", spec: null,
    });
    await cdp.evaluate("document.querySelectorAll('#file-viewer .code-symbol-result')[1].click(); true");
    await waitFor(
      () => cdp.evaluate("document.querySelector('#file-viewer .code-viewer-path')?.textContent === 'src/app.js' && document.querySelectorAll('#file-viewer .code-line').length === 320"),
      "choosing a listed definition did not open it",
    );
    await waitFor(
      () => cdp.evaluate("document.querySelector('#file-viewer .code-line[data-line=\"3\"]').classList.contains('code-line-flash')"),
      "the chosen definition line was not revealed",
    );

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
      // The preceding Files fixture uses a legacy summary without an
      // instance ID; this status-only change must keep that same identity.
      instance_id: null,
    })]);
    transcriptFixture = transcript(20, 80, "third-transcript");
    await waitFor(
      () => cdp.evaluate("document.getElementById('agent-meta').textContent.includes('working')"),
      "overview churn did not render while Raw was visible",
    );
    assert.equal(
      await cdp.evaluate("document.querySelector('[data-transcript-id=\"message-99\"]') !== null"),
      false, "hidden Conversation should defer refresh until the reader returns",
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
    await waitFor(
      () => cdp.evaluate("document.querySelector('[data-transcript-id=\"message-99\"]') !== null"),
      "Conversation did not catch up after leaving Raw",
      5_000,
    );
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

    // Conversation folds mixed tools into bounded runs. Errors stay visible
    // in the outer summary, and expansion restores every original card.
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
      () => cdp.evaluate("document.querySelectorAll('#conversation .tool-call-group').length === 2"),
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
        errorSummary: conversation.querySelector('[data-transcript-id="tool-wait-error"] .tool-label')?.textContent,
        cancelledSummary: conversation.querySelector('[data-transcript-id="tool-wait-cancelled"] .tool-label')?.textContent,
        malformedStatusSummaries: ['numeric', 'boolean', 'null'].map((suffix) =>
          conversation.querySelector('[data-transcript-id="tool-wait-' + suffix + '"] .tool-label')?.textContent),
        meaningfulSeparate: Boolean(conversation.querySelector('[data-transcript-id="tool-wait-meaningful"]')),
        execErrorSummary: conversation.querySelector('[data-transcript-id="tool-exec-error"] .tool-label')?.textContent,
        execBoundarySummaries: ['timeout', 'ok-after-timeout', 'json-error', 'json-ok', 'process-error']
          .map((suffix) => conversation.querySelector('[data-transcript-id="tool-exec-' + suffix + '"] .tool-label')?.textContent),
        mixedToolsGrouped: [
          'tool-exec-split-1', 'tool-patch-split', 'tool-exec-split-2',
          'tool-apply-1', 'tool-apply-2', 'tool-web-1', 'tool-web-2', 'tool-plan-1', 'tool-plan-2',
        ]
          .every((id) => {
            const node = conversation.querySelector('[data-transcript-id="' + id + '"]');
            return Boolean(node?.closest('.tool-call-group'));
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
    assert.ok(compactTools.groupSummaries.includes("Tools ×29 · 9 errors · tokens —"), JSON.stringify(compactTools));
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
    assert.equal(compactTools.mixedToolsGrouped, true, JSON.stringify(compactTools));
    assert.equal(compactTools.fileReaderPreferences, '{"wrap":true,"size":"small"}');
    assert.equal(compactTools.markupInjected, false, JSON.stringify(compactTools));
    assert.equal(compactTools.escapedInputVisible, true, JSON.stringify(compactTools));
    assert.ok(Math.abs(compactTools.after - compactTools.before) <= 1, JSON.stringify(compactTools));

    const expandedExec = await cdp.evaluate(`(() => {
      const conversation = document.getElementById('conversation');
      const group = conversation.querySelector('[data-transcript-id="tool-group:tool-exec-1"]');
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
    assert.equal(expandedExec.label, "Tools ×29 · 9 errors · tokens —; 29 calls and results");
    assert.deepEqual(expandedExec.order, transcriptFixture.messages.filter((message) => message.kind === "tool").slice(3).map((message) => message.id));
    assert.equal(expandedExec.inputVisible, true, JSON.stringify(expandedExec));
    assert.equal(expandedExec.resultVisible, true, JSON.stringify(expandedExec));
    assert.equal(expandedExec.markupInjected, false, JSON.stringify(expandedExec));

    // A stale expansion callback must not mutate a freshly reconnected
    // transcript even when it is still the same pane. Hold the queued callback,
    // force the pane stream's resync path, then invoke the old callback against
    // the new Conversation generation.
    const staleExpansionSetup = await cdp.evaluate(`(() => {
      const conversation = document.getElementById('conversation');
      const group = conversation.querySelector('.tool-call-group:not(.tool-run-group)');
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
      () => cdp.evaluate("document.getElementById('stream-state').textContent === 'Live' && document.querySelectorAll('#conversation .tool-call-group:not([data-old-generation])').length === 2"),
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
    assert.equal(staleExpansionResult.groupCount, 2, JSON.stringify(staleExpansionResult));
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
    assert.equal(humanHidden.groups, 2, JSON.stringify(humanHidden));
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
    // Page.reload acknowledges the command before the replacement document is
    // necessarily installed. The transcript predicate below also matches the
    // old document, so waiting on content alone can let a Back click race (and
    // be discarded by) the pending navigation on a slow CI worker.
    await cdp.evaluate("window.__atmuxFilterReloadGeneration = 'before-reload'; true");
    await cdp.send("Page.reload");
    await waitFor(
      () => cdp.evaluate(`window.__atmuxFilterReloadGeneration !== 'before-reload'
        && document.readyState === 'complete'
        && document.getElementById('conversation').textContent.includes('VISIBLE AGENT')
        && !document.getElementById('conversation').textContent.includes('HIDDEN HUMAN')
        && document.getElementById('conversation-filters-indicator').textContent === '2 off'
        && document.querySelector('.session-button[data-session-id="midnight~%5"]') !== null`),
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
      return !document.querySelector('#conversation .tool-call-group:not(.tool-run-group)');
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
    assert.equal(mergedGroupAnchorAfter.summary, "exec ×3 · tokens —", JSON.stringify(mergedGroupAnchorAfter));
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
        singletonOutsideGroup: !singleton.closest('.tool-call-group:not(.tool-run-group)'),
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
    assert.equal(await cdp.evaluate("document.getElementById('launch-browser-up').disabled"), false);
    await cdp.evaluate("document.getElementById('launch-browser-up').click(); true");
    await waitFor(
      () => cdp.evaluate("document.getElementById('launch-browser-path').textContent === '/workspace'"),
      "folder browser did not navigate to its allowed parent",
    );
    assert.equal(
      await cdp.evaluate("document.getElementById('launch-browser-up').disabled"),
      true,
      "Up must be disabled only at the actual allowed root",
    );
    await cdp.evaluate("document.querySelector('.launch-browser-folder').click(); true");
    await waitFor(
      () => cdp.evaluate("document.getElementById('launch-browser-path').textContent === '/workspace/custom'"),
      "folder browser did not return to the selected child",
    );

    const browserActionGeometry = await cdp.evaluate(`(() => ({
      documentWidth: document.documentElement.scrollWidth,
      viewportWidth: window.innerWidth,
      buttons: [...document.querySelectorAll('.launch-browser-actions button')].map((button) => ({
        height: button.getBoundingClientRect().height,
        right: button.getBoundingClientRect().right,
      })),
    }))()`);
    assert.ok(browserActionGeometry.documentWidth <= browserActionGeometry.viewportWidth, JSON.stringify(browserActionGeometry));
    assert.ok(browserActionGeometry.buttons.every((button) => button.height >= 44 && button.right <= browserActionGeometry.viewportWidth + 1), JSON.stringify(browserActionGeometry));

    await cdp.evaluate("document.getElementById('launch-browser-new').click(); true");
    await cdp.evaluate(`(() => {
      document.getElementById('launch-browser-new-name').value = 'new project';
      document.getElementById('launch-browser-operation-confirm').click();
      return true;
    })()`);
    await waitFor(
      () => cdp.evaluate("[...document.querySelectorAll('.launch-browser-folder')].some((button) => button.dataset.path === '/workspace/custom/new project')"),
      "new folder action did not refresh the current listing",
    );
    assert.deepEqual(launchDirectoryMutationRequests.at(-1), {
      pathname: "/api/v1/launch-directories/folders",
      body: { machine: "tron", directory: "/workspace/custom", name: "new project" },
    });

    await cdp.evaluate("document.getElementById('launch-browser-clone').click(); true");
    const clonePanel = await cdp.evaluate(`(() => {
      const repository = document.getElementById('launch-browser-repository');
      repository.value = 'https://example.test/team/cloned project.git';
      repository.dispatchEvent(new Event('input', { bubbles: true }));
      const destination = document.getElementById('launch-browser-destination');
      return {
        destination: destination.value,
        fontSize: getComputedStyle(repository).fontSize,
        inputHeight: repository.getBoundingClientRect().height,
        expanded: document.getElementById('launch-browser-clone').getAttribute('aria-expanded'),
      };
    })()`);
    assert.equal(clonePanel.destination, "cloned project", JSON.stringify(clonePanel));
    assert.equal(clonePanel.fontSize, "16px", JSON.stringify(clonePanel));
    assert.ok(clonePanel.inputHeight >= 44, JSON.stringify(clonePanel));
    assert.equal(clonePanel.expanded, "true", JSON.stringify(clonePanel));
    await cdp.evaluate("document.getElementById('launch-browser-operation-confirm').click(); true");
    await waitFor(
      () => cdp.evaluate("[...document.querySelectorAll('.launch-browser-folder')].some((button) => button.dataset.path === '/workspace/custom/cloned project')"),
      "clone action did not refresh the current listing",
    );
    assert.deepEqual(launchDirectoryMutationRequests.at(-1), {
      pathname: "/api/v1/launch-directories/clone",
      body: {
        machine: "tron", directory: "/workspace/custom",
        repository: "https://example.test/team/cloned project.git",
        destination: "cloned project",
      },
    });
    assert.equal(await cdp.evaluate("document.getElementById('launch-browser-operation').hidden"), true);

    await cdp.evaluate(`(() => {
      document.getElementById('launch-browser-clone').click();
      const repository = document.getElementById('launch-browser-repository');
      repository.value = 'https://oauth2:super-secret@example.test/team/private.git';
      repository.dispatchEvent(new Event('input', { bubbles: true }));
      document.getElementById('launch-browser-operation-confirm').click();
      return true;
    })()`);
    await waitFor(
      () => cdp.evaluate("document.getElementById('launch-browser-operation-note').textContent.includes('credential-bearing')"),
      "credential-bearing repository rejection was not shown inline",
    );
    const credentialError = await cdp.evaluate("document.getElementById('launch-browser-operation-note').textContent");
    assert.doesNotMatch(credentialError, /super-secret|oauth2|private\.git/);
    await cdp.evaluate("document.getElementById('launch-browser-operation-cancel').click(); true");

    launchDirectoryMutationDelayMs = 600;
    await cdp.evaluate(`(() => {
      document.getElementById('launch-browser-clone').click();
      const repository = document.getElementById('launch-browser-repository');
      repository.value = 'https://example.test/team/stale-clone.git';
      repository.dispatchEvent(new Event('input', { bubbles: true }));
      document.getElementById('launch-browser-operation-confirm').click();
      return true;
    })()`);
    await waitFor(
      () => cdp.evaluate("document.getElementById('launch-browser-operation-confirm').disabled"),
      "delayed clone did not enter its disabled mutation state",
    );
    await cdp.evaluate("document.getElementById('launch-browser-close').click(); true");
    await cdp.evaluate("document.getElementById('launch-browse').click(); true");
    await waitFor(
      () => cdp.evaluate("!document.getElementById('launch-browser').hidden && document.querySelector('.launch-browser-folder')?.dataset.path === '/workspace/custom'"),
      "folder browser did not reopen while an old clone response was pending",
    );
    await cdp.evaluate("document.querySelector('.launch-browser-folder').click(); true");
    await waitFor(
      () => cdp.evaluate("document.getElementById('launch-browser-path').textContent === '/workspace/custom'"),
      "folder browser did not navigate after reopening during an old clone request",
    );
    await cdp.evaluate("document.getElementById('launch-browser-clone').click(); true");
    const reopenedControls = await cdp.evaluate(`(() => ({
      repository: document.getElementById('launch-browser-repository').disabled,
      destination: document.getElementById('launch-browser-destination').disabled,
      cancel: document.getElementById('launch-browser-operation-cancel').disabled,
      confirm: document.getElementById('launch-browser-operation-confirm').disabled,
    }))()`);
    assert.deepEqual(reopenedControls, {
      repository: false, destination: false, cancel: false, confirm: false,
    });
    await new Promise((resolveWait) => setTimeout(resolveWait, 700));
    assert.equal(
      await cdp.evaluate("[...document.querySelectorAll('.launch-browser-folder')].some((button) => button.dataset.path.endsWith('/stale-clone'))"),
      false,
      "a stale clone response mutated the reopened browser",
    );
    assert.equal(await cdp.evaluate("document.getElementById('launch-browser-operation').hidden"), false);
    await cdp.evaluate("document.getElementById('launch-browser-operation-cancel').click(); true");

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
      () => cdp.evaluate("[...document.querySelectorAll('#launch-directory-suggestions [role=option]')].some((option) => option.dataset.directory === '/workspace/custom')"),
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

test("Actions refreshes restart readiness and a failed confirmation cannot reuse its binding", { timeout: 60_000 }, async () => {
  const profileDirectory = await mkdtemp(join(tmpdir(), "atmux-restart-browser-"));
  let server;
  let chrome;
  let cdp;
  let testError = null;
  restartCapabilityReady = false;
  restartCapabilityToken = "restart-v1-" + "a".repeat(64);
  restartRequests.length = 0;
  try {
    const started = await startServer();
    server = started.server;
    const browser = await launchChrome(profileDirectory);
    chrome = browser.chrome;
    cdp = await openCdp(browser.browserSocket, "about:blank");
    await cdp.send("Page.enable");
    await cdp.send("Page.navigate", { url: `http://127.0.0.1:${started.port}/?session=tron~%25100` });
    await waitFor(
      () => cdp.evaluate("document.getElementById('agent-name')?.textContent === 'codex-main' && document.getElementById('quick-resume-note').textContent === 'Session restart is unavailable'"),
      "initial unavailable restart capability did not load",
    );
    restartCapabilityReady = true;
    await cdp.evaluate("document.getElementById('quick-actions-open').click(); true");
    await waitFor(
      () => cdp.evaluate("!document.getElementById('quick-resume').disabled"),
      "Actions did not refresh a now-ready agent",
    );
    await cdp.evaluate("document.getElementById('quick-resume').click(); document.getElementById('resume-confirm').click(); true");
    await waitFor(
      () => cdp.evaluate("!document.getElementById('resume-dialog').open && document.getElementById('toast').textContent.includes('process changed') && !document.getElementById('resume-confirm').disabled"),
      "failed restart confirmation did not close with feedback",
    );
    assert.equal(restartRequests.length, 1);
    assert.equal(restartRequests[0].body.restart_token, "restart-v1-" + "a".repeat(64));
    await cdp.evaluate("document.getElementById('resume-confirm').click(); true");
    await new Promise((resolveDelay) => setTimeout(resolveDelay, 50));
    assert.equal(restartRequests.length, 1, "the failed confirmation retained a reusable binding");
    await cdp.evaluate("document.getElementById('quick-actions-open').click(); true");
    await waitFor(() => cdp.evaluate("!document.getElementById('quick-resume').disabled"), "fresh confirmation capability did not load");
    await cdp.evaluate("document.getElementById('quick-resume').click(); document.getElementById('resume-confirm').click(); true");
    await waitFor(() => restartRequests.length === 2, "freshly confirmed restart request did not arrive");
    assert.equal(restartRequests[1].body.restart_token, "restart-v1-" + "b".repeat(64));
  } catch (error) {
    testError = error;
    throw error;
  } finally {
    try { await cleanupBrowserHarness({ cdp, chrome, server, profileDirectory }); }
    catch (cleanupError) {
      if (!testError) throw cleanupError;
      console.error(cleanupError);
    }
    restartCapabilityReady = false;
    paneStreams.clear();
    overviewStreams.clear();
  }
});

test("raw downloads reject replaced pane output and retired callbacks while retaining known offline output", { timeout: 60_000 }, async () => {
  const profileDirectory = await mkdtemp(join(tmpdir(), "atmux-output-browser-"));
  let server;
  let chrome;
  let cdp;
  let testError = null;
  try {
    const started = await startServer();
    server = started.server;
    const browser = await launchChrome(profileDirectory);
    chrome = browser.chrome;
    cdp = await openCdp(browser.browserSocket, "about:blank");
    await cdp.send("Page.enable");
    await cdp.send("Page.addScriptToEvaluateOnNewDocument", { source: `
      window.__reviewPaneStreams = [];
      window.__heldSnapshots = [];
      const NativeEventSource = EventSource;
      window.EventSource = class extends NativeEventSource {
        constructor(url) {
          super(url);
          this.paneStream = url.includes('/panes/');
          this.callbacks = new Map();
          if (this.paneStream) window.__reviewPaneStreams.push(this);
        }
        addEventListener(type, callback, options) {
          this.callbacks.set(type, callback);
          super.addEventListener(type, (event) => {
            if (this.paneStream && type === 'pane.snapshot' && window.__holdPaneSnapshots) {
              window.__heldSnapshots.push(() => callback(event));
            } else callback(event);
          }, options);
        }
      };
      const create = URL.createObjectURL.bind(URL);
      URL.createObjectURL = (blob) => { window.__exportBlob = blob; return create(blob); };
      HTMLAnchorElement.prototype.click = function() { window.__exportFilename = this.download; };
    ` });
    paneSnapshotContent = "OLD PROCESS OUTPUT";
    await cdp.send("Page.navigate", { url: `http://127.0.0.1:${started.port}/?session=tron~%25100` });
    await waitFor(
      () => cdp.evaluate("document.getElementById('pane')?.textContent === 'OLD PROCESS OUTPUT'"),
      "original pane output missing",
    );
    await cdp.evaluate("window.__holdPaneSnapshots = true; true");
    paneSnapshotContent = "NEW PROCESS OUTPUT";
    emitOverviewPatch([mockSession("tron", "%100", "replacement-agent", "waiting", {
      instance_id: "pane-v1-" + "f".repeat(64), agent: "codex",
    })]);
    await waitFor(
      () => cdp.evaluate("document.getElementById('agent-name').textContent === 'replacement-agent' && window.__heldSnapshots.length > 0"),
      "replacement instance did not request a fresh output snapshot",
    );
    const replaced = await cdp.evaluate(`(() => {
      const retired = window.__reviewPaneStreams[0];
      retired.callbacks.get('pane.snapshot')({ data: JSON.stringify({ revision: 77, content: 'LATE OLD OUTPUT' }) });
      retired.callbacks.get('pane.removed')({ data: '{}' });
      document.getElementById('quick-actions-open').click();
      document.getElementById('quick-download-output').click();
      return {
        pane: document.getElementById('pane').textContent,
        selected: new URL(location.href).searchParams.get('session'),
        exported: Boolean(window.__exportBlob),
        feedback: document.getElementById('toast').textContent,
      };
    })()`);
    assert.deepEqual(replaced, {
      pane: "", selected: "tron~%100", exported: false, feedback: "No raw output is available yet",
    });
    await cdp.evaluate("window.__holdPaneSnapshots = false; window.__heldSnapshots.splice(0).forEach((release) => release()); true");
    await waitFor(
      () => cdp.evaluate("document.getElementById('pane').textContent === 'NEW PROCESS OUTPUT'"),
      "replacement snapshot did not render",
    );
    const downloaded = await cdp.evaluate(`(async () => {
      const current = window.__reviewPaneStreams.at(-1);
      current.callbacks.get('pane.error')({ data: JSON.stringify({ error: 'Owner offline', kind: 'offline' }) });
      document.getElementById('quick-download-output').click();
      return { filename: window.__exportFilename, output: await window.__exportBlob.text() };
    })()`);
    assert.deepEqual(downloaded, { filename: "atmux-replacement-agent-output.txt", output: "NEW PROCESS OUTPUT\n" });
  } catch (error) {
    testError = error;
    throw error;
  } finally {
    try { await cleanupBrowserHarness({ cdp, chrome, server, profileDirectory }); }
    catch (cleanupError) {
      if (!testError) throw cleanupError;
      console.error(cleanupError);
    }
    paneSnapshotContent = "";
    paneStreams.clear();
    overviewStreams.clear();
  }
});

test("conversation groups mixed errors and shows per-entry and filter-independent totals on mobile", { timeout: 60_000 }, async () => {
  const profileDirectory = await mkdtemp(join(tmpdir(), "atmux-conversation-metrics-"));
  overviewRevision = 1;
  transcriptFixture = {
    available: true, source: "codex", changed: true, truncated: true, content_hash: "metrics-first",
    messages: [
      { id: "human", role: "user", markdown: "Check these tools" },
      ...[
        ["exec", "result"], ["send_message", "sent"], ["exec", "Error: failed"],
        ["exec", "<script>unsafe</script>"], ["exec", "Process exited with code 1"], ["followup_task", "A useful reply"],
      ].map(([tool_name, tool_output], index) => ({
        id: `mixed-${index}`, role: "tool", kind: "tool", tool_name, tool_output,
        input_tokens: 1000, output_tokens: 100,
      })),
      { id: "assistant", role: "assistant", markdown: "Here is the result", input_tokens: 500, output_tokens: 50 },
      { id: "approval", role: "tool", kind: "tool", tool_name: "exec", tool_output: "Approval required before continuing" },
    ],
  };
  let server;
  let chrome;
  let cdp;
  let testError = null;
  try {
    const started = await startServer();
    server = started.server;
    const browser = await launchChrome(profileDirectory);
    chrome = browser.chrome;
    cdp = await openCdp(browser.browserSocket, "about:blank");
    await cdp.send("Page.enable");
    await cdp.send("Emulation.setDeviceMetricsOverride", { width: 390, height: 844, deviceScaleFactor: 1, mobile: true });
    await cdp.send("Page.navigate", { url: `http://127.0.0.1:${started.port}/?session=tron~%25100` });
    await waitFor(() => cdp.evaluate("document.querySelector('#conversation .tool-run-group') !== null"), "mixed screenshot tools did not group");
    const readMetrics = () => cdp.evaluate("document.getElementById('conversation-metrics').textContent");
    const originalTotals = await readMetrics();
    assert.equal(originalTotals, "Loaded totals (partial) · 2 messages · 7 tools · 6.5k in · 650 out · 7.2k total tokens · 2 errors");
    const initial = await cdp.evaluate(`(() => {
      const conversation = document.getElementById('conversation');
      const group = conversation.querySelector('.tool-run-group');
      return {
        groups: conversation.querySelectorAll('.tool-call-group').length,
        open: group.open,
        summary: group.querySelector('summary').textContent,
        metrics: [...conversation.querySelectorAll('.entry-metrics')].map((node) => node.textContent),
        approvalSeparate: !conversation.querySelector('[data-transcript-id="approval"]').closest('.tool-call-group'),
        errorVisible: group.classList.contains('has-errors'),
        top: document.getElementById('conversation-metrics').getBoundingClientRect().bottom <= conversation.getBoundingClientRect().top,
        overflow: document.documentElement.scrollWidth - innerWidth,
        injected: Boolean(conversation.querySelector('script, img')),
      };
    })()`);
    assert.equal(initial.groups, 1);
    assert.equal(initial.open, false);
    assert.equal(initial.summary, "Tools ×6 · 2 errors · 6k in · 600 out");
    assert.deepEqual(initial.metrics, ["tokens —", ...Array(6).fill("1k in · 100 out"), "500 in · 50 out", "tokens —"]);
    assert.equal(initial.approvalSeparate, true);
    assert.equal(initial.errorVisible, true);
    assert.equal(initial.top, true);
    assert.ok(initial.overflow <= 1);
    assert.equal(initial.injected, false);
    if (process.env.ATMUX_CONVERSATION_SCREENSHOT) {
      const capture = await cdp.send("Page.captureScreenshot", { format: "png" });
      await writeFile(process.env.ATMUX_CONVERSATION_SCREENSHOT, Buffer.from(capture.data, "base64"));
    }
    await cdp.evaluate("document.querySelector('#conversation .tool-run-group > summary').click()");
    assert.equal(await cdp.evaluate("document.querySelector('#conversation .tool-run-group').open"), true);
    assert.equal(await readMetrics(), originalTotals, "expansion double-counted tokens");
    transcriptFixture = { ...transcriptFixture, content_hash: "metrics-updated", messages: [...transcriptFixture.messages,
      { id: "assistant-new", role: "assistant", markdown: "New reply", input_tokens: 100, output_tokens: 10 },
    ] };
    await waitFor(() => cdp.evaluate("document.getElementById('conversation').textContent.includes('New reply')"), "metrics did not refresh");
    assert.equal(await cdp.evaluate("document.querySelector('#conversation .tool-run-group').open"), true, "refresh lost expansion");
    const updatedTotals = await readMetrics();
    assert.match(updatedTotals, /6.6k in · 660 out · 7.3k total tokens/);
    await cdp.evaluate(`(() => {
      document.getElementById('conversation-filters-open').click();
      document.getElementById('conversation-show-internal').click();
      document.querySelector('#conversation-filters-dialog .primary').click();
    })()`);
    assert.equal(await readMetrics(), updatedTotals, "Show filters changed totals");
    assert.equal(await cdp.evaluate("document.querySelectorAll('#conversation .tool-card').length"), 0);
    await cdp.evaluate("document.getElementById('raw-view').click()");
    assert.equal(await cdp.evaluate("document.getElementById('conversation-metrics').hidden"), true);
    await cdp.evaluate("document.getElementById('conversation-view').click()");
    assert.equal(await cdp.evaluate("document.getElementById('conversation-metrics').hidden"), false);
    // Ryan's second screenshot: seven failures and one ordinary exec result
    // must be one row, with the following Agent prose outside that disclosure.
    transcriptFixture = {
      available: true, source: "codex", changed: true, truncated: false, content_hash: "metrics-exec-eight",
      messages: [
        ...Array.from({ length: 8 }, (_, index) => ({
          id: `eight-${index}`, role: "tool", kind: "tool", tool_name: "exec",
          tool_output: index === 3 ? "result" : "Error: failed",
        })),
        { id: "eight-agent", role: "assistant", markdown: "Agent prose stays outside" },
      ],
    };
    await cdp.evaluate(`(() => {
      document.getElementById('conversation-filters-open').click();
      document.getElementById('conversation-filters-reset').click();
      document.querySelector('#conversation-filters-dialog .primary').click();
    })()`);
    await waitFor(() => cdp.evaluate("document.querySelector('#conversation .tool-call-group > summary')?.textContent === 'exec ×8 · 7 errors · tokens —'"), "second screenshot did not become one exec row");
    assert.equal(await cdp.evaluate("document.querySelectorAll('#conversation .tool-call-group').length"), 1);
    assert.equal(await cdp.evaluate("document.querySelector('#conversation .tool-call-group').open"), false);
    assert.equal(await cdp.evaluate("Boolean(document.querySelector('#conversation > [data-transcript-id=\"eight-agent\"]'))"), true);
    transcriptFixture = { available: false, source: "codex", changed: false, messages: [] };
    await cdp.evaluate("document.querySelector('.session-button[data-session-id=\"midnight~%5\"]').click()");
    assert.equal(await readMetrics(), "", "previous session totals survived selection change");
  } catch (error) {
    testError = error;
    throw error;
  } finally {
    try { await cleanupBrowserHarness({ cdp, chrome, server, profileDirectory }); }
    catch (cleanupError) { if (!testError) throw cleanupError; }
    transcriptFixture = null;
    paneStreams.clear();
    overviewStreams.clear();
  }
});

test("Conversation accepts slow reads, survives continuous output and pauses outside its view", { timeout: 60_000 }, async () => {
  const profileDirectory = await mkdtemp(join(tmpdir(), "atmux-conversation-refresh-"));
  overviewRevision = 1;
  transcriptRequests.length = 0;
  transcriptResponseDelayMs = 3200;
  const fixture = (text, hash) => ({
    available: true, source: "claude", changed: true, truncated: false, content_hash: hash,
    messages: [{ id: hash, role: "assistant", markdown: text, input_tokens: 100, output_tokens: 10 }],
  });
  transcriptFixture = fixture("Slow conversation loaded", "slow-first");
  let server, chrome, cdp, patches;
  let testError = null;
  try {
    const started = await startServer();
    server = started.server;
    const browser = await launchChrome(profileDirectory);
    chrome = browser.chrome;
    cdp = await openCdp(browser.browserSocket, "about:blank");
    await cdp.send("Page.enable");
    await cdp.send("Emulation.setDeviceMetricsOverride", { width: 390, height: 844, deviceScaleFactor: 1, mobile: true });
    await cdp.send("Page.navigate", { url: `http://127.0.0.1:${started.port}/?session=midnight~%255` });
    await waitFor(() => transcriptRequests.length > 0, "Conversation did not request its first read");
    let revision = 1;
    patches = setInterval(() => {
      emitPanePatch({ base_revision: revision, revision: ++revision, start_line: 0, delete_lines: 0, lines: [] });
    }, 100);
    await waitFor(() => cdp.evaluate("document.getElementById('conversation').textContent.includes('Slow conversation loaded')"), "slow reads were continually superseded", 10_000);
    assert.equal(transcriptRequests.length, 1, "slow reads must not overlap");

    transcriptResponseDelayMs = 0;
    transcriptFixture = fixture("Continuous output still refreshes", "streaming");
    await waitFor(() => cdp.evaluate("document.getElementById('conversation').textContent.includes('Continuous output still refreshes')"), "pane patches starved Conversation refresh");
    assert.equal(transcriptRequests[1].hash, "slow-first", "known hash was not reused");
    clearInterval(patches);
    patches = null;
    await cdp.evaluate("document.getElementById('raw-view').click()");
    const rawRequests = transcriptRequests.length;
    await new Promise((resolve) => setTimeout(resolve, 2800));
    assert.equal(transcriptRequests.length, rawRequests, "hidden Conversation kept polling");

    // Start a slow old response, then switch to another owner. It must be
    // aborted, and must not populate the replacement or block its first read.
    transcriptResponseDelayMs = 3200;
    transcriptFixture = fixture("Retired owner must never appear", "retired");
    await cdp.evaluate("document.getElementById('conversation-view').click()");
    await waitFor(() => transcriptRequests.length > rawRequests, "Conversation did not resume immediately");
    const oldRequest = transcriptRequests.at(-1);
    transcriptFixture = fixture("New selected owner", "replacement");
    transcriptResponseDelayMs = 0;
    await cdp.evaluate("document.querySelector('.session-button[data-session-id=\"tron~%100\"]').click()");
    await waitFor(() => cdp.evaluate("document.getElementById('conversation').textContent.includes('New selected owner')"), "retired request blocked new owner");
    await waitFor(() => oldRequest.closed, "retired read was not aborted");
    assert.equal(transcriptRequests.at(-1).hash, null, "old owner's hash leaked into new selection");

    // Reusing the same pane ID with a different process generation must retire
    // its pending response just as changing to another pane does.
    const beforeReplacement = transcriptRequests.length;
    transcriptResponseDelayMs = 3200;
    transcriptFixture = fixture("Retired generation must never appear", "retired-generation");
    await cdp.evaluate("document.getElementById('raw-view').click(); document.getElementById('conversation-view').click()");
    await waitFor(() => transcriptRequests.length > beforeReplacement, "generation test did not start a pending read");
    const replacedRequest = transcriptRequests.at(-1);
    transcriptResponseDelayMs = 0;
    transcriptFixture = fixture("Replacement generation", "new-generation");
    emitOverviewPatch([mockSession("tron", "%100", "codex-main", "waiting", {
      agent: "codex", instance_id: "pane-v1-" + "f".repeat(64),
    })]);
    await waitFor(() => cdp.evaluate("document.getElementById('conversation').textContent.includes('Replacement generation')"), "same-pane replacement retained its predecessor's request");
    await waitFor(() => replacedRequest.closed, "replaced generation's read was not aborted");
    assert.equal(transcriptRequests.at(-1).hash, null, "old generation's hash leaked into replacement");

    await cdp.evaluate("window.dispatchEvent(new PageTransitionEvent('pagehide', { persisted: true }))");
    const hiddenRequests = transcriptRequests.length;
    await new Promise((resolve) => setTimeout(resolve, 3300));
    assert.equal(transcriptRequests.length, hiddenRequests, "background page kept polling");
    assert.equal(await cdp.evaluate("document.getElementById('conversation').textContent.includes('Retired owner')"), false);
    assert.equal(await cdp.evaluate("document.getElementById('conversation').textContent.includes('Retired generation')"), false);
    transcriptFixture = fixture("Returned from background", "visible-again");
    await cdp.evaluate("window.dispatchEvent(new PageTransitionEvent('pageshow', { persisted: true }))");
    await waitFor(() => cdp.evaluate("document.getElementById('conversation').textContent.includes('Returned from background')"), "foreground page did not resume Conversation");
  } catch (error) {
    testError = error;
    throw error;
  } finally {
    clearInterval(patches);
    try { await cleanupBrowserHarness({ cdp, chrome, server, profileDirectory }); }
    catch (cleanupError) { if (!testError) throw cleanupError; }
    transcriptFixture = null;
    transcriptResponseDelayMs = 0;
    transcriptRequests.length = 0;
    paneStreams.clear();
    overviewStreams.clear();
  }
});

test("composer input history survives successful sends and stays scoped to the session", { timeout: 60_000 }, async () => {
  const profileDirectory = await mkdtemp(join(tmpdir(), "atmux-history-browser-"));
  messageRequests.length = 0;
  messageResponseDelayMs = 0;
  nextMessageFailurePane = null;
  overviewRevision = 1;
  let server;
  let chrome;
  let cdp;
  let testError = null;
  try {
    const started = await startServer();
    server = started.server;
    const browser = await launchChrome(profileDirectory);
    chrome = browser.chrome;
    cdp = await openCdp(browser.browserSocket, "about:blank");
    await cdp.send("Page.enable");
    await cdp.send("Page.navigate", { url: `http://127.0.0.1:${started.port}/?session=tron~%25100` });
    await waitFor(
      () => cdp.evaluate("document.readyState === 'complete' && document.getElementById('agent-name').textContent === 'codex-main' && !document.getElementById('message').disabled"),
      "history test composer did not become available",
    );
    const value = () => cdp.evaluate("document.getElementById('message').value");
    const draft = (text, position = text.length) => cdp.evaluate(`(() => {
      const input = document.getElementById('message');
      input.focus();
      input.value = ${JSON.stringify(text)};
      input.setSelectionRange(${position}, ${position});
      input.dispatchEvent(new InputEvent('input', { bubbles: true }));
    })()`);
    const arrow = async (key) => {
      const keyCode = key === "ArrowUp" ? 38 : 40;
      await cdp.send("Input.dispatchKeyEvent", { type: "rawKeyDown", key, code: key, windowsVirtualKeyCode: keyCode });
      await cdp.send("Input.dispatchKeyEvent", { type: "keyUp", key, code: key, windowsVirtualKeyCode: keyCode });
    };
    const send = async (text) => {
      await draft(text);
      const before = messageRequests.length;
      await cdp.evaluate("document.getElementById('send').click()");
      await waitFor(() => messageRequests.length === before + 1, "history fixture did not receive the message");
      await waitFor(
        () => cdp.evaluate("!document.getElementById('send').disabled && document.getElementById('message').value === ''"),
        "successful send did not finish clearing the draft",
      );
    };
    const select = async (id) => {
      await cdp.evaluate(`document.querySelector('.session-button[data-session-id="${id}"]').click()`);
      await waitFor(() => cdp.evaluate("!document.getElementById('message').disabled"), "selected composer is disabled");
    };
    const first = "first message";
    const second = "second message\nwith multiple lines";
    await send(first);
    await send(second);
    await draft("");
    await arrow("ArrowUp");
    assert.equal(await value(), second, "successful draft cleanup must retain sent-message history");
    await draft("unfinished draft");
    await arrow("ArrowUp");
    assert.equal(await value(), second, "successful draft cleanup must retain the newest sent message");
    await arrow("ArrowUp");
    assert.equal(await value(), first, "repeated Up must browse past a recalled multiline message");
    await arrow("ArrowUp");
    assert.equal(await value(), first, "Up stops at the oldest message");
    await arrow("ArrowDown");
    assert.equal(await value(), second);
    await arrow("ArrowDown");
    assert.equal(await value(), "unfinished draft", "Down restores the original unsent draft");

    await draft("first line\nmiddle line\nlast line", 14);
    await arrow("ArrowUp");
    assert.equal(await value(), "first line\nmiddle line\nlast line", "editing a middle line must not recall history");
    await draft("first line\nlast line", 3);
    await arrow("ArrowUp");
    assert.equal(await value(), second, "Up on the first line recalls history without requiring Home");

    await select("midnight~%5");
    assert.equal(await value(), "", "another session must not inherit the selected history entry");
    await send("only for Midnight");
    await arrow("ArrowUp");
    assert.equal(await value(), "only for Midnight");
    await select("tron~%100");
    await draft("");
    await arrow("ArrowUp");
    assert.equal(await value(), second, "switching sessions must retain each session's own history");

    nextMessageFailurePane = "tron~%100";
    await draft("failed message stays a draft");
    const beforeFailure = messageRequests.length;
    await cdp.evaluate("document.getElementById('send').click()");
    await waitFor(() => messageRequests.length === beforeFailure + 1, "failed message was not attempted");
    await waitFor(() => cdp.evaluate("!document.getElementById('send').disabled"), "failed send did not settle");
    assert.equal(await value(), "failed message stays a draft");
    await arrow("ArrowUp");
    assert.equal(await value(), second, "failed sends must neither clear history nor become sent entries");
    await arrow("ArrowDown");
    assert.equal(await value(), "failed message stays a draft");

    emitOverviewPatch([mockSession("tron", "%100", "replacement-history-agent", "waiting", {
      instance_id: "pane-v1-" + "f".repeat(64), agent: "codex",
    })]);
    await waitFor(
      () => cdp.evaluate("document.getElementById('agent-name').textContent === 'replacement-history-agent' && !document.getElementById('message').disabled"),
      "replacement pane did not become available",
    );
    await draft("");
    await arrow("ArrowUp");
    assert.equal(await value(), "", "a reused pane ID must not inherit another process's history");
    assert.equal(messageRequests.length, 4, "browsing history must never submit a message");
  } catch (error) {
    testError = error;
    throw error;
  } finally {
    try { await cleanupBrowserHarness({ cdp, chrome, server, profileDirectory }); }
    catch (cleanupError) {
      if (!testError) throw cleanupError;
      console.error(cleanupError);
    }
    nextMessageFailurePane = null;
    paneStreams.clear();
    overviewStreams.clear();
  }
});

test("dashboard reconnect preserves the pane and draft, and link/search actions report their outcomes", { timeout: 60_000 }, async () => {
  const profileDirectory = await mkdtemp(join(tmpdir(), "atmux-interaction-browser-"));
  let server;
  let chrome;
  let cdp;
  let testError = null;
  try {
    const started = await startServer();
    server = started.server;
    const browser = await launchChrome(profileDirectory);
    chrome = browser.chrome;
    cdp = await openCdp(browser.browserSocket, "about:blank");
    await cdp.send("Page.enable");
    await cdp.send("Page.navigate", { url: `http://127.0.0.1:${started.port}/?session=tron~%25100` });
    await waitFor(
      () => cdp.evaluate("document.getElementById('overview-status')?.textContent === 'Live' && !document.getElementById('agent-view').hidden"),
      "interaction test overview did not become live",
    );
    await cdp.evaluate(`(() => {
      const input = document.getElementById('message');
      input.value = 'Unsent draft survives overview retry';
      input.setSelectionRange(3, 9);
      input.dispatchEvent(new InputEvent('input', { bubbles: true }));
    })()`);
    const originalPaneStreams = new Set(paneStreams);
    // A brief drop that the browser reconnects from never raises the banner.
    for (const response of overviewStreams) response.end();
    await waitFor(
      () => cdp.evaluate("document.getElementById('overview-status').textContent === 'Reconnecting…'"),
      "a dropped overview did not report reconnecting",
      5_000,
    );
    assert.equal(await cdp.evaluate("document.getElementById('overview-notice').hidden"), true);
    await waitFor(
      () => cdp.evaluate("document.getElementById('overview-status').textContent === 'Live'"),
      "a briefly dropped overview did not reconnect by itself",
    );
    assert.equal(await cdp.evaluate("document.getElementById('health-alert').hidden"), true);
    // A drop that outlasts the grace period exposes the banner and Retry.
    overviewUnavailable = true;
    for (const response of overviewStreams) response.end();
    await waitFor(
      () => cdp.evaluate("!document.getElementById('overview-notice').hidden && !document.getElementById('overview-retry').disabled"),
      "disconnected overview did not expose retry",
      15_000,
    );
    overviewUnavailable = false;
    const offline = await cdp.evaluate(`(() => ({
      note: document.getElementById('overview-note').textContent,
      bannerHeight: document.getElementById('health-alert').getBoundingClientRect().height,
      overflowX: document.documentElement.scrollWidth - innerWidth,
      selected: new URL(location.href).searchParams.get('session'),
      draft: document.getElementById('message').value,
    }))()`);
    assert.match(offline.note, /disconnected/);
    assert.ok(offline.bannerHeight > 0, JSON.stringify(offline));
    assert.ok(offline.overflowX <= 1, JSON.stringify(offline));
    assert.equal(offline.selected, "tron~%100");
    assert.equal(offline.draft, "Unsent draft survives overview retry");
    await cdp.evaluate("document.getElementById('overview-retry').click(); true");
    await waitFor(
      () => cdp.evaluate("document.getElementById('overview-status').textContent === 'Live' && document.getElementById('health-alert').hidden"),
      "manual retry did not restore live overview",
    );
    assert.equal(await cdp.evaluate("document.getElementById('message').value"), offline.draft);
    assert.equal(await cdp.evaluate("document.getElementById('message').selectionStart"), 3);
    assert.equal(await cdp.evaluate("new URL(location.href).searchParams.get('session')"), offline.selected);
    for (const stream of originalPaneStreams) assert.ok(paneStreams.has(stream), "overview retry replaced a pane stream");

    await cdp.evaluate(`(() => {
      Object.defineProperty(navigator, 'clipboard', { configurable: true, value: {
        async writeText() { throw new Error('permission denied'); },
      } });
      document.getElementById('quick-actions-open').click();
      document.getElementById('quick-copy-link').click();
    })()`);
    await waitFor(
      () => cdp.evaluate("document.getElementById('quick-copy-link-status').textContent.includes('Could not copy')"),
      "clipboard denial was not reported",
    );
    assert.equal(await cdp.evaluate("document.getElementById('quick-actions-dialog').open"), true);
    await cdp.evaluate(`(() => {
      navigator.clipboard.writeText = async (value) => { window.__copiedAgentLink = value; };
      document.getElementById('quick-copy-link').click();
    })()`);
    await waitFor(
      () => cdp.evaluate("document.getElementById('quick-copy-link-status').textContent.includes('Link copied')"),
      "successful clipboard write was not reported",
    );
    assert.equal(new URL(await cdp.evaluate("window.__copiedAgentLink")).searchParams.get("session"), "tron~%100");
    await cdp.evaluate("document.getElementById('quick-actions-dialog').close(); true");

    await cdp.send("Emulation.setDeviceMetricsOverride", { width: 1100, height: 800, deviceScaleFactor: 1, mobile: false });
    const shortcuts = await cdp.evaluate(`(() => {
      const filter = document.getElementById('filter');
      const key = (target, value) => {
        const event = new KeyboardEvent('keydown', { key: value, bubbles: true, cancelable: true });
        target.dispatchEvent(event);
        return event.defaultPrevented;
      };
      document.getElementById('rail-toggle').click();
      const focused = key(document.body, '/') && document.activeElement === filter;
      const expanded = !document.body.classList.contains('rail-collapsed');
      filter.value = 'tron';
      filter.dispatchEvent(new InputEvent('input', { bubbles: true }));
      key(filter, 'Escape');
      const cleared = filter.value === '' && document.activeElement === filter;
      key(filter, 'Escape');
      const blurred = document.activeElement !== filter;
      const message = document.getElementById('message');
      message.focus();
      const typingUntouched = !key(message, '/') && document.activeElement === message;
      document.getElementById('quick-actions-open').click();
      const dialogUntouched = !key(document.body, '/') && document.activeElement !== filter;
      document.getElementById('quick-actions-dialog').close();
      return { focused, expanded, cleared, blurred, typingUntouched, dialogUntouched };
    })()`);
    assert.deepEqual(shortcuts, {
      focused: true, expanded: true, cleared: true, blurred: true, typingUntouched: true, dialogUntouched: true,
    });
  } catch (error) {
    testError = error;
    throw error;
  } finally {
    try { await cleanupBrowserHarness({ cdp, chrome, server, profileDirectory }); }
    catch (cleanupError) {
      if (!testError) throw cleanupError;
      console.error(cleanupError);
    }
    paneStreams.clear();
    overviewStreams.clear();
  }
});


test("inline rename, keyboard controls and safe automatic summaries work in the browser", { timeout: 60_000 }, async () => {
  const profileDirectory = await mkdtemp(join(tmpdir(), "atmux-a2-browser-"));
  let server; let chrome; let cdp; let testError = null;
  sessionRenameRequests.length = 0;
  agentSummaryFixture = { enabled: true, title: "Durable session summaries", description: "Keep context",
    digest: "Goal: retain context. <script>window.summaryInjected = true</script> Tests remain.", stale: false };
  try {
    const started = await startServer(); server = started.server;
    const browser = await launchChrome(profileDirectory); chrome = browser.chrome;
    cdp = await openCdp(browser.browserSocket, "about:blank"); await cdp.send("Page.enable");
    await cdp.send("Page.navigate", { url: `http://127.0.0.1:${started.port}/?session=tron~%25100` });
    await waitFor(() => cdp.evaluate("document.getElementById('agent-name').textContent === 'codex-main'"), "selected session did not load");
    await cdp.evaluate("document.getElementById('message').blur(); document.dispatchEvent(new KeyboardEvent('keydown', { key: 'F2', bubbles: true })); true");
    await waitFor(() => cdp.evaluate("Boolean(document.querySelector('.inline-rename-name'))"), "F2 did not open inline rename");
    await cdp.evaluate("document.querySelector('.inline-rename-name').dispatchEvent(new KeyboardEvent('keydown', { key: 'Escape', bubbles: true })); true");
    assert.equal(await cdp.evaluate("Boolean(document.querySelector('.inline-rename'))"), false);
    assert.equal(sessionRenameRequests.length, 0);
    await cdp.evaluate("document.getElementById('agent-name').dispatchEvent(new MouseEvent('dblclick', { bubbles: true })); true");
    await cdp.evaluate("[...document.querySelectorAll('.inline-rename button')].find(b => b.textContent === 'Suggest').click(); true");
    await waitFor(() => cdp.evaluate("document.querySelector('.inline-rename-name').value === 'durable-session-summaries'"), "Suggest did not fill title");
    await cdp.evaluate("document.querySelector('.inline-rename-name').dispatchEvent(new KeyboardEvent('keydown', { key: 'Enter', bubbles: true })); true");
    await waitFor(() => sessionRenameRequests.length === 1, "Enter did not send rename");
    assert.deepEqual(sessionRenameRequests[0], { pane: "tron~%100", body: { instance_id: "pane-v1-" + "1".repeat(64), name: "durable-session-summaries" } });
    await waitFor(() => cdp.evaluate("!document.querySelector('.inline-rename')"), "saved inline editor did not close");
    await cdp.evaluate("document.getElementById('conversation-view').click(); true");
    await waitFor(() => cdp.evaluate("Boolean(document.querySelector('.conversation-summary'))"), "summary block did not load");
    assert.equal(await cdp.evaluate("document.querySelector('.conversation-summary').open"), false);
    await cdp.evaluate("document.querySelector('.conversation-summary summary').click(); true");
    assert.equal(await cdp.evaluate("document.querySelector('.conversation-summary').open"), true);
    assert.equal(await cdp.evaluate("Boolean(window.summaryInjected)"), false);
    assert.equal(await cdp.evaluate("document.querySelector('.conversation-summary script') === null"), true);
    emitOverviewPatch([{ ...mockSession("tron", "%100", "durable-session-summaries", "waiting", { agent: "codex" }),
      instance_id: "pane-v1-" + "1".repeat(64), description: "Keep context", description_source: "auto" }]);
    await waitFor(() => cdp.evaluate("document.querySelector('.session-description[data-source=auto]')?.textContent === 'Keep context'"), "automatic note marker did not render");
    await cdp.evaluate("document.querySelector('.session-button[data-session-id=\"tron~%100\"] .session-name').dispatchEvent(new MouseEvent('dblclick', { bubbles: true })); true");
    assert.equal(await cdp.evaluate("Boolean(document.querySelector('.session-row .inline-rename'))"), true);
    await cdp.evaluate("document.querySelector('.inline-rename-name').dispatchEvent(new KeyboardEvent('keydown', { key: 'Escape', bubbles: true })); true");
  } catch (error) { testError = error; throw error; }
  finally {
    agentSummaryFixture = null;
    try { await cleanupBrowserHarness({ cdp, chrome, server, profileDirectory }); }
    catch (error) { if (!testError) throw error; console.error(error); }
    paneStreams.clear(); overviewStreams.clear();
  }
});
