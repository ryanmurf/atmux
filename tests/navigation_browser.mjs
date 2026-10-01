import assert from "node:assert/strict";
import { spawn } from "node:child_process";
import { once } from "node:events";
import { mkdtemp, readFile, rm } from "node:fs/promises";
import { createServer } from "node:http";
import { tmpdir } from "node:os";
import { join } from "node:path";
import test from "node:test";

async function waitFor(predicate, label) {
  const deadline = Date.now() + 15_000;
  while (Date.now() < deadline) {
    const value = await predicate();
    if (value) return value;
    await new Promise((resolve) => setTimeout(resolve, 25));
  }
  throw new Error(`Timed out: ${label}`);
}

async function connectCdp(browserSocket) {
  const endpoint = new URL(browserSocket);
  const target = await fetch(`http://${endpoint.host}/json/new?about:blank`, {
    method: "PUT", signal: AbortSignal.timeout(10_000),
  }).then((response) => response.json());
  const socket = new WebSocket(target.webSocketDebuggerUrl);
  await new Promise((resolve, reject) => {
    const timer = setTimeout(() => reject(new Error("CDP connect timeout")), 10_000);
    socket.addEventListener("open", () => { clearTimeout(timer); resolve(); }, { once: true });
    socket.addEventListener("error", () => { clearTimeout(timer); reject(new Error("CDP failed")); }, { once: true });
  });
  const pending = new Map();
  let sequence = 0;
  socket.addEventListener("message", ({ data }) => {
    const message = JSON.parse(data);
    const item = pending.get(message.id);
    if (!item) return;
    pending.delete(message.id);
    clearTimeout(item.timer);
    if (message.error) item.reject(new Error(message.error.message));
    else item.resolve(message.result);
  });
  socket.addEventListener("close", () => {
    for (const item of pending.values()) {
      clearTimeout(item.timer);
      item.reject(new Error("CDP closed"));
    }
    pending.clear();
  });
  const send = (method, params = {}) => new Promise((resolve, reject) => {
    const id = ++sequence;
    const timer = setTimeout(() => { pending.delete(id); reject(new Error(`CDP timeout: ${method}`)); }, 10_000);
    pending.set(id, { resolve, reject, timer });
    socket.send(JSON.stringify({ id, method, params }));
  });
  const evaluate = async (expression) => {
    const result = await send("Runtime.evaluate", { expression, returnByValue: true, awaitPromise: true });
    if (result.exceptionDetails) throw new Error(JSON.stringify(result.exceptionDetails));
    return result.result.value;
  };
  return { socket, send, evaluate };
}

test("mobile navigation persists preferences and session actions retain the selected output and process identity", { timeout: 60_000 }, async () => {
  const profileDirectory = await mkdtemp(join(tmpdir(), "atmux-navigation-browser-"));
  const streams = new Set();
  const paneStreams = new Set();
  const mutations = [];
  const restartRequests = [];
  const modelRequests = [];
  const paneContent = "first line\nsecond line ✓";
  const instance = (digit) => `pane-v1-${digit.repeat(64)}`;
  const machines = [
    { id: "local", label: "Workstation", kind: "local", online: true, sessions: 2 },
    { id: "max", label: "Max", kind: "remote", online: true, sessions: 1 },
  ];
  let sessions = [
    { id: "local~%1", machine: "local", pane_id: "%1", name: "alpha", path: "/work/api", agent: "codex", status: "working", instance_id: instance("a") },
    { id: "local~%2", machine: "local", pane_id: "%2", name: "zebra", path: "/work/web", agent: "claude", status: "waiting", instance_id: instance("b") },
    { id: "max~%1", machine: "max", pane_id: "%1", name: "beta", path: "/work/api", agent: "claude", status: "waiting", instance_id: instance("c") },
  ];
  let revision = 1;
  const snapshot = () => `event: sessions.snapshot\ndata: ${JSON.stringify({ revision, sessions, machines, health: null })}\n\n`;
  const files = new Map(await Promise.all(["index.html", "app.js", "app.css"].map(async (name) =>
    [name, await readFile(new URL(`../web/${name}`, import.meta.url))])));
  const server = createServer((request, response) => {
    const path = new URL(request.url, "http://fixture").pathname;
    if (request.method !== "GET") mutations.push({ path, method: request.method });
    if (path === "/api/v1/events") {
      response.writeHead(200, { "Content-Type": "text/event-stream", "Cache-Control": "no-cache" });
      streams.add(response);
      response.write(snapshot());
      request.on("close", () => streams.delete(response));
      return;
    }
    if (path === "/api/v1/fleet/updates") {
      response.writeHead(200, { "Content-Type": "application/json" }).end("[]");
      return;
    }
    if (/^\/api\/v1\/panes\/[^/]+\/events$/.test(path)) {
      response.writeHead(200, { "Content-Type": "text/event-stream", "Cache-Control": "no-cache" });
      paneStreams.add(response);
      response.write(`event: pane.snapshot\ndata: ${JSON.stringify({ revision: 1, content: paneContent })}\n\n`);
      request.on("close", () => paneStreams.delete(response));
      return;
    }
    if (/^\/api\/v1\/panes\/[^/]+\/models$/.test(path)) {
      const paneId = decodeURIComponent(path.split("/")[4]);
      modelRequests.push(paneId);
      const session = sessions.find((session) => session.id === paneId);
      response.writeHead(200, { "Content-Type": "application/json" }).end(JSON.stringify({
        pane_id: paneId, harness: session.agent, models: [], model_options: [], effort_options: [],
        resume_available: true, resume_note: null, restart_token: "restart-v1-" + "c".repeat(64),
      }));
      return;
    }
    if (/^\/api\/v1\/panes\/[^/]+\/restart-instance$/.test(path) && request.method === "POST") {
      let body = "";
      request.on("data", (chunk) => { body += chunk; });
      request.on("end", () => {
        restartRequests.push({ path, ...JSON.parse(body) });
        response.writeHead(409, { "Content-Type": "application/json" }).end('{"error":"The agent process changed; refresh before restarting"}');
      });
      return;
    }
    const name = path === "/" ? "index.html" : path.slice(1);
    if (files.has(name)) {
      const type = name.endsWith(".js") ? "text/javascript" : name.endsWith(".css") ? "text/css" : "text/html";
      response.writeHead(200, { "Content-Type": type }).end(files.get(name));
    } else response.writeHead(404, { "Content-Type": "application/json" }).end('{"error":"fixture route unavailable"}');
  });
  let chrome;
  let cdp;
  try {
    server.listen(0, "127.0.0.1");
    await once(server, "listening");
    const url = `http://127.0.0.1:${server.address().port}/`;
    chrome = spawn("google-chrome", [
      "--headless=new", "--no-sandbox", "--disable-gpu", "--disable-dev-shm-usage",
      "--remote-debugging-port=0", `--user-data-dir=${profileDirectory}`, "about:blank",
    ], { stdio: ["ignore", "ignore", "pipe"] });
    let output = "";
    let launchError = null;
    chrome.on("error", (error) => { launchError = error; });
    chrome.stderr.setEncoding("utf8");
    chrome.stderr.on("data", (chunk) => { output = `${output}${chunk}`.slice(-16_384); });
    const browserSocket = await waitFor(() => {
      if (launchError) throw launchError;
      if (chrome.exitCode !== null) throw new Error(`Chrome exited: ${output}`);
      return output.match(/DevTools listening on (ws:\/\/[^\s]+)/)?.[1];
    }, "Chrome startup");
    cdp = await connectCdp(browserSocket);
    await cdp.send("Page.enable");
    await cdp.send("Emulation.setDeviceMetricsOverride", { width: 390, height: 844, deviceScaleFactor: 1, mobile: true });
    await cdp.send("Emulation.setTouchEmulationEnabled", { enabled: true });
    await cdp.send("Page.navigate", { url });
    const ready = () => cdp.evaluate("document.querySelectorAll('.session-button').length === 3");
    await waitFor(ready, "agent list");

    const disclosure = "document.querySelector('.machine-toggle[data-machine-id=local]')";
    const localList = "document.getElementById('machine-sessions-local')";
    const pin = "document.querySelector('.session-pin[data-session-id=\"local~%2\"]')";
    const tap = async (expression) => {
      const point = await cdp.evaluate(`(() => { const r = (${expression}).getBoundingClientRect(); return { x: r.x + r.width / 2, y: r.y + r.height / 2 }; })()`);
      await cdp.send("Input.dispatchTouchEvent", { type: "touchStart", touchPoints: [point] });
      await cdp.send("Input.dispatchTouchEvent", { type: "touchEnd", touchPoints: [] });
    };
    assert.deepEqual(await cdp.evaluate(`(() => {
      const toggle = ${disclosure};
      const controls = [toggle, ${pin}, document.getElementById('filter-status'), document.getElementById('filter-harness')];
      return { expanded: toggle.getAttribute('aria-expanded'), target: toggle.getAttribute('aria-controls'),
        touchSize: controls.every((node) => node.getBoundingClientRect().width >= 44 && node.getBoundingClientRect().height >= 44),
        overflow: document.documentElement.scrollWidth - innerWidth };
    })()`), { expanded: "true", target: "machine-sessions-local", touchSize: true, overflow: 0 });

    await tap(disclosure);
    await waitFor(() => cdp.evaluate(`${localList}.hidden`), "touch collapse");
    assert.equal(await cdp.evaluate(`${disclosure}.getAttribute('aria-expanded')`), "false");
    assert.equal(await cdp.evaluate("document.getElementById('machine-view').hidden"), true, "collapse is independent from machine details");
    await cdp.evaluate("window.__navigationReloading = true");
    await cdp.send("Page.reload");
    await waitFor(() => cdp.evaluate(`!window.__navigationReloading && ${localList}?.hidden && document.querySelectorAll('.session-button').length === 3`), "collapse survives reload");

    await cdp.evaluate(`(() => {
      const input = document.getElementById('filter'); input.value = 'zebra'; input.dispatchEvent(new Event('input'));
    })()`);
    assert.deepEqual(await cdp.evaluate(`({ hidden: ${localList}.hidden,
      disabled: ${disclosure}.disabled,
      names: [...document.querySelectorAll('.session-name')].map((node) => node.textContent),
      collapsed: JSON.parse(localStorage.getItem('atmux.navigation.v1')).collapsed })`),
    { hidden: false, disabled: true, names: ["zebra"], collapsed: ["local"] });
    await cdp.evaluate("document.getElementById('filter-clear').click()");
    assert.equal(await cdp.evaluate(`${localList}.hidden`), true);

    await cdp.evaluate(`${disclosure}.focus()`);
    await cdp.send("Input.dispatchKeyEvent", { type: "keyDown", key: "Enter", code: "Enter", windowsVirtualKeyCode: 13, text: "\r" });
    await cdp.send("Input.dispatchKeyEvent", { type: "keyUp", key: "Enter", code: "Enter", windowsVirtualKeyCode: 13 });
    assert.equal(await cdp.evaluate(`${localList}.hidden`), false, "native keyboard disclosure opens the group");
    await cdp.evaluate(`window.__pinNode = ${pin}; window.__rowNode = ${pin}.closest('li'); ${pin}.focus(); ${pin}.click()`);
    assert.deepEqual(await cdp.evaluate(`({ names: [...${localList}.querySelectorAll('.session-name')].map((node) => node.textContent),
      focused: document.activeElement === window.__pinNode,
      sameRow: ${pin}.closest('li') === window.__rowNode,
      pressed: ${pin}.getAttribute('aria-pressed') })`),
    { names: ["zebra", "alpha"], focused: true, sameRow: true, pressed: "true" });
    await cdp.evaluate("window.__navigationReloading = true");
    await cdp.send("Page.reload");
    await waitFor(() => cdp.evaluate(`!window.__navigationReloading && ${pin}?.getAttribute('aria-pressed') === 'true'`), "favorite survives reload");
    assert.equal(await cdp.evaluate(`${localList}.querySelector('.session-name').textContent`), "zebra");

    await cdp.evaluate(`(() => {
      for (const [id, value] of [['filter-status', 'waiting'], ['filter-harness', 'claude']]) {
        const node = document.getElementById(id); node.value = value; node.dispatchEvent(new Event('change'));
      }
      const input = document.getElementById('filter'); input.value = 'api'; input.dispatchEvent(new Event('input'));
    })()`);
    assert.deepEqual(await cdp.evaluate(`({ names: [...document.querySelectorAll('.session-name')].map((node) => node.textContent),
      summary: document.getElementById('filter-summary').textContent })`), { names: ["beta"], summary: "1 of 3 agents" });
    await cdp.evaluate("document.getElementById('filter-clear').click()");
    assert.deepEqual(await cdp.evaluate("['filter', 'filter-status', 'filter-harness'].map((id) => document.getElementById(id).value)"), ["", "", ""]);

    sessions = sessions.map((session) => session.name === "zebra" ? { ...session, instance_id: instance("d") } : session);
    revision += 1;
    for (const response of streams) response.write(snapshot());
    await waitFor(() => cdp.evaluate(`${pin}.getAttribute('aria-pressed') === 'false'`), "recycled pane loses predecessor favorite");
    assert.equal(await cdp.evaluate(`${localList}.querySelector('.session-name').textContent`), "alpha");
    await tap("document.querySelector('.machine-header[data-machine-id=local]')");
    await waitFor(() => cdp.evaluate("!document.getElementById('machine-view').hidden"), "machine details still open");
    assert.equal(await cdp.evaluate("document.getElementById('machine-name').textContent"), "Workstation");
    assert.deepEqual(mutations, [], "navigation never sends mutations to an agent or owner");

    await cdp.evaluate("document.getElementById('machine-mobile-back').click()");
    await waitFor(() => cdp.evaluate("!document.body.classList.contains('has-selection')"), "return to agent menu");
    await tap("document.querySelector('.session-button[data-session-id=\"local~%2\"]')");
    await waitFor(() => cdp.evaluate("document.getElementById('stream-state').textContent === 'Live' && !document.getElementById('quick-resume').disabled"), "selected pane output and restart capability");
    const downloaded = await cdp.evaluate(`(async () => {
      const originalCreate = URL.createObjectURL;
      const originalClick = HTMLAnchorElement.prototype.click;
      let blob;
      let filename;
      URL.createObjectURL = (value) => { blob = value; return originalCreate(value); };
      HTMLAnchorElement.prototype.click = function () { filename = this.download; };
      try {
        document.getElementById('quick-actions-open').click();
        document.getElementById('quick-download-output').click();
        return { content: await blob.text(), type: blob.type, filename };
      } finally { URL.createObjectURL = originalCreate; HTMLAnchorElement.prototype.click = originalClick; }
    })()`);
    assert.equal(downloaded.content, `${paneContent}\n`);
    assert.equal(downloaded.type, "text/plain;charset=utf-8");
    assert.equal(downloaded.filename, "atmux-zebra-output.txt");

    const modelsBeforeActions = modelRequests.length;
    await cdp.evaluate("document.getElementById('quick-actions-open').click()");
    await waitFor(() => cdp.evaluate("!document.getElementById('quick-resume').disabled"), "Actions refreshed restart capability");
    assert.ok(modelRequests.length > modelsBeforeActions, "Actions issues a fresh capability read");
    assert.equal(modelRequests.at(-1), "local~%2");
    await cdp.evaluate("document.getElementById('quick-resume').click()");
    assert.equal(await cdp.evaluate("document.getElementById('resume-dialog').open"), true);
    sessions = sessions.map((session) => session.name === "zebra" ? { ...session, name: "replacement", instance_id: instance("e") } : session);
    revision += 1;
    for (const response of streams) response.write(snapshot());
    await waitFor(() => cdp.evaluate("document.getElementById('agent-name').textContent === 'replacement'"), "process replacement while confirmation open");
    await cdp.evaluate("document.getElementById('resume-confirm').click()");
    await waitFor(() => restartRequests.length === 1, "generation-bound restart request");
    assert.deepEqual(restartRequests, [{ path: "/api/v1/panes/local~%252/restart-instance", instance_id: instance("d"), restart_token: "restart-v1-" + "c".repeat(64) }]);
    await waitFor(() => cdp.evaluate("document.getElementById('toast').textContent.includes('process changed')"), "owner generation rejection");
    assert.equal(await cdp.evaluate("document.getElementById('resume-dialog').open"), false, "owner rejection closes the stale confirmation");
    await cdp.evaluate("document.getElementById('resume-confirm').click()");
    await new Promise((resolveDelay) => setTimeout(resolveDelay, 50));
    assert.equal(restartRequests.length, 1, "a rejected binding cannot be submitted again");
    await cdp.evaluate("document.getElementById('quick-actions-open').click()");
    await waitFor(() => cdp.evaluate("!document.getElementById('quick-resume').disabled"), "replacement capability can be fetched again");
    await cdp.evaluate("document.getElementById('quick-resume').click()");
    assert.equal(await cdp.evaluate("document.getElementById('resume-dialog').open"), true, "replacement requires a new confirmation");
    await cdp.evaluate("document.getElementById('resume-dialog').close()");
  } finally {
    cdp?.socket.close();
    if (chrome?.pid && chrome.exitCode === null && chrome.signalCode === null) {
      const exited = once(chrome, "exit");
      const force = setTimeout(() => chrome.kill("SIGKILL"), 3_000);
      chrome.kill("SIGTERM");
      try { await exited; } finally { clearTimeout(force); }
    }
    for (const response of streams) response.end();
    for (const response of paneStreams) response.end();
    if (server.listening) {
      const closed = once(server, "close");
      server.close();
      server.closeAllConnections();
      await closed;
    }
    await rm(profileDirectory, { recursive: true, force: true, maxRetries: 10, retryDelay: 50 });
  }
});


test("session history supports filters, pagination, hostile text, resume hook and mobile Back", { timeout: 60_000 }, async () => {
  const profileDirectory = await mkdtemp(join(tmpdir(), "atmux-history-browser-"));
  const streams = new Set();
  const queries = [];
  const files = new Map(await Promise.all(["index.html", "app.js", "app.css"].map(async (name) =>
    [name, await readFile(new URL(`../web/${name}`, import.meta.url))])));
  const archived = { session_key: "0199a5b7-5560-7abc-8def-0123456789ab", machine: "peer", name: "<img src=x onerror=window.historyXss=true>", description: "Archived task", project: { remote: "https://github.com/org/repo" }, state: "archived", last_active_ms: 1000 };
  let hold = false;
  const pending = [];
  const server = createServer((request, response) => {
    const url = new URL(request.url, "http://fixture");
    if (url.pathname === "/api/v1/events") {
      response.writeHead(200, { "Content-Type": "text/event-stream" }); streams.add(response);
      response.write(`event: sessions.snapshot\ndata: ${JSON.stringify({ revision: 1, sessions: [], machines: [], health: null })}\n\n`);
      request.on("close", () => streams.delete(response)); return;
    }
    if (url.pathname === "/api/v1/session-history") {
      queries.push(url.searchParams);
      const page = url.searchParams.has("cursor") ? { sessions: [{ ...archived, session_key: "0199a5b7-5560-7abc-8def-0123456789ac", name: "Second task", state: "closed" }], next_cursor: null }
        : { sessions: [archived], next_cursor: archived.session_key };
      const send = () => response.writeHead(200, { "Content-Type": "application/json" }).end(JSON.stringify(page));
      if (hold) pending.push(send); else send(); return;
    }
    const name = url.pathname === "/" ? "index.html" : url.pathname.slice(1);
    if (files.has(name)) { response.writeHead(200, { "Content-Type": name.endsWith("js") ? "text/javascript" : name.endsWith("css") ? "text/css" : "text/html" }).end(files.get(name)); return; }
    response.writeHead(200, { "Content-Type": "application/json" }).end("[]");
  });
  let chrome;
  let cdp;
  try {
    server.listen(0, "127.0.0.1"); await once(server, "listening");
    chrome = spawn("google-chrome", ["--headless=new", "--no-sandbox", "--disable-gpu", "--disable-dev-shm-usage", "--remote-debugging-port=0", `--user-data-dir=${profileDirectory}`, "about:blank"], { stdio: ["ignore", "ignore", "pipe"] });
    let output = "";
    chrome.stderr.setEncoding("utf8"); chrome.stderr.on("data", (chunk) => { output = `${output}${chunk}`.slice(-16_384); });
    const browserSocket = await waitFor(() => output.match(/DevTools listening on (ws:\/\/[^\s]+)/)?.[1], "Chrome startup");
    cdp = await connectCdp(browserSocket);
    await cdp.send("Page.enable");
    await cdp.send("Emulation.setDeviceMetricsOverride", { width: 390, height: 844, deviceScaleFactor: 1, mobile: true });
    await cdp.send("Page.navigate", { url: `http://127.0.0.1:${server.address().port}/` });
    await waitFor(() => cdp.evaluate("document.getElementById('overview-status')?.textContent === 'Live'"), "history button");
    await cdp.evaluate("document.getElementById('history-open').click()");
    await waitFor(() => cdp.evaluate("document.querySelectorAll('.session-history-row').length === 1"), "history first page");
    assert.equal(await cdp.evaluate("new URL(location.href).searchParams.get('view')"), "sessions");
    assert.equal(await cdp.evaluate("document.querySelector('.session-history-row strong').textContent"), archived.name);
    assert.equal(await cdp.evaluate("Boolean(window.historyXss) || Boolean(document.querySelector('.session-history-row img'))"), false);
    assert.equal(await cdp.evaluate("document.querySelector('[data-session-resume]').disabled"), true);
    await cdp.evaluate("window.atmuxSessionResume = (request) => window.resumeRequest = request; document.getElementById('history-refresh').click()");
    await waitFor(() => cdp.evaluate("!document.querySelector('[data-session-resume]').disabled"), "resume hook");
    await cdp.evaluate("document.querySelector('[data-session-resume]').click()");
    assert.deepEqual(await cdp.evaluate("window.resumeRequest"), { session_key: archived.session_key, machine: "peer" });
    await cdp.evaluate("document.getElementById('history-more').click()");
    await waitFor(() => cdp.evaluate("document.querySelectorAll('.session-history-row').length === 2"), "history pagination");
    assert.equal(queries.at(-1).get("cursor"), archived.session_key);
    await cdp.evaluate("document.getElementById('history-text').value = 'task & context'; document.getElementById('history-state').value = 'archived'; document.getElementById('history-machine').value = 'peer'; document.getElementById('history-project').value = 'repo'; document.getElementById('history-filters').requestSubmit()");
    await waitFor(() => queries.at(-1).get("text") === "task & context", "history search");
    assert.equal(queries.at(-1).get("state"), "archived"); assert.equal(queries.at(-1).get("machine"), "peer");
    assert.equal(queries.at(-1).get("project"), "repo"); assert.equal(queries.at(-1).has("cursor"), false);
    hold = true;
    await cdp.evaluate("document.getElementById('history-refresh').click()");
    await waitFor(() => pending.length === 1, "pending history refresh");
    await cdp.evaluate("document.getElementById('history-back').click()");
    pending.shift()();
    await waitFor(() => cdp.evaluate("document.getElementById('history-view').hidden && !document.body.classList.contains('has-selection')"), "history Back");
    assert.equal(await cdp.evaluate("new URL(location.href).searchParams.get('view')"), null);
    assert.equal(await cdp.evaluate("document.documentElement.scrollWidth <= innerWidth"), true);
  } finally {
    cdp?.socket.close();
    if (chrome?.pid && chrome.exitCode === null && chrome.signalCode === null) {
      const exited = once(chrome, "exit"); const force = setTimeout(() => chrome.kill("SIGKILL"), 3_000);
      chrome.kill("SIGTERM"); try { await exited; } finally { clearTimeout(force); }
    }
    for (const response of streams) response.end();
    if (server.listening) { const closed = once(server, "close"); server.close(); server.closeAllConnections(); await closed; }
    await rm(profileDirectory, { recursive: true, force: true, maxRetries: 10, retryDelay: 50 });
  }
});
