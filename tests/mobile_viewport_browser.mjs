import assert from "node:assert/strict";
import { spawn } from "node:child_process";
import { once } from "node:events";
import { mkdtemp, readFile, rm } from "node:fs/promises";
import { createServer } from "node:http";
import { createRequire } from "node:module";
import { tmpdir } from "node:os";
import { join } from "node:path";
import test from "node:test";

const { installMobileViewportRecovery } = createRequire(import.meta.url)("../web/app.js");
const delay = (ms) => new Promise((resolve) => setTimeout(resolve, ms));

async function waitFor(check, description) {
  const deadline = Date.now() + 15_000;
  while (Date.now() < deadline) {
    const result = await check();
    if (result) return result;
    await delay(30);
  }
  throw new Error(`Timed out: ${description}`);
}

async function connect(browserSocket) {
  const target = await fetch(`http://${new URL(browserSocket).host}/json/new?about:blank`, { method: "PUT" }).then((r) => r.json());
  const socket = new WebSocket(target.webSocketDebuggerUrl);
  await new Promise((resolve, reject) => {
    socket.addEventListener("open", resolve, { once: true });
    socket.addEventListener("error", reject, { once: true });
  });
  let nextId = 0;
  const pending = new Map();
  socket.addEventListener("message", ({ data }) => {
    const message = JSON.parse(data);
    const operation = pending.get(message.id);
    if (!operation) return;
    pending.delete(message.id);
    clearTimeout(operation.timer);
    if (message.error) operation.reject(new Error(message.error.message));
    else operation.resolve(message.result);
  });
  socket.addEventListener("close", () => {
    for (const operation of pending.values()) {
      clearTimeout(operation.timer);
      operation.reject(new Error("Browser closed"));
    }
    pending.clear();
  });
  const send = (method, params = {}) => new Promise((resolve, reject) => {
    const id = ++nextId;
    const timer = setTimeout(() => {
      pending.delete(id);
      reject(new Error(`Timed out: ${method}`));
    }, 15_000);
    pending.set(id, { resolve, reject, timer });
    socket.send(JSON.stringify({ id, method, params }));
  });
  const evaluate = async (expression) => {
    const result = await send("Runtime.evaluate", { expression, awaitPromise: true, returnByValue: true });
    if (result.exceptionDetails) throw new Error(JSON.stringify(result.exceptionDetails));
    return result.result.value;
  };
  return { socket, send, evaluate };
}

test("mobile viewport recovery and touch rail targets in Chromium", { timeout: 45_000 }, async () => {
  const css = await readFile(new URL("../web/app.css", import.meta.url), "utf8");
  const profile = await mkdtemp(join(tmpdir(), "atmux-viewport-browser-"));
  const html = `<!doctype html><html><head><meta name="viewport" content="width=device-width, initial-scale=1"><style>${css}</style></head><body>
    <header class="topbar">atmux</header><div class="health-alert" hidden></div>
    <main class="workspace"><aside class="rail"><input id="filter" aria-label="Filter">
      <button class="machine-header">Node</button><ul class="session-list">${Array.from({ length: 40 }, (_, index) => `
      <li class="session-row"><button class="session-button"><span>◆</span><span class="session-copy"><span class="session-name">Session ${index}</span><span class="session-sub">A saved conversation</span></span></button><button class="session-pin" aria-label="Pin session ${index}">☆</button><button class="session-edit" aria-label="Rename session ${index}">✎</button><button class="session-delete" aria-label="Delete session ${index}">×</button></li>`).join("")}</ul>
    </aside><section class="detail"></section></main><dialog id="dialog"><textarea id="message"></textarea></dialog></body></html>`;
  const server = createServer((request, response) => { response.setHeader("Content-Type", "text/html"); response.end(html); });
  server.listen(0, "127.0.0.1");
  await once(server, "listening");
  // Headless hosts can have no physical pointer. Give desktop mode the same
  // mouse capabilities used by Playwright; CDP mobile emulation overrides them.
  const chrome = spawn("google-chrome", ["--headless=new", "--no-sandbox", "--disable-gpu", "--disable-dev-shm-usage", "--blink-settings=primaryHoverType=2,availableHoverTypes=2,primaryPointerType=4,availablePointerTypes=4", "--remote-debugging-port=0", `--user-data-dir=${profile}`, "about:blank"], { stdio: ["ignore", "ignore", "pipe"] });
  let output = "";
  chrome.stderr.on("data", (chunk) => { output = `${output}${chunk}`.slice(-16000); });
  let cdp;
  try {
    const browserSocket = await waitFor(() => {
      if (chrome.exitCode !== null) throw new Error(output);
      return output.match(/DevTools listening on (ws:\/\/[^\s]+)/)?.[1];
    }, "Chrome DevTools startup");
    cdp = await connect(browserSocket);
    await cdp.send("Emulation.setDeviceMetricsOverride", { width: 390, height: 844, deviceScaleFactor: 1, mobile: true });
    await cdp.send("Emulation.setTouchEmulationEnabled", { enabled: true });
    await cdp.send("Page.navigate", { url: `http://127.0.0.1:${server.address().port}` });
    await waitFor(() => cdp.evaluate("document.readyState === 'complete' && document.querySelectorAll('.session-row').length === 40"), "rail fixture");
    await cdp.send("DOM.enable");
    await cdp.send("CSS.enable");
    const { root } = await cdp.send("DOM.getDocument");
    const { nodeId } = await cdp.send("DOM.querySelector", { nodeId: root.nodeId, selector: ".session-row" });
    const { nodeId: deleteId } = await cdp.send("DOM.querySelector", { nodeId: root.nodeId, selector: ".session-delete" });
    const geometry = `(() => {
      const button = document.querySelector('.session-button');
      const remove = document.querySelector('.session-delete');
      const rect = (node) => { const r = node.getBoundingClientRect(); return { left: r.left, top: r.top, width: r.width, height: r.height }; };
      return { coarse: matchMedia('(pointer: coarse)').matches, hover: matchMedia('(hover: hover)').matches,
        button: rect(button), pin: rect(document.querySelector('.session-pin')), edit: rect(document.querySelector('.session-edit')), remove: rect(remove), node: rect(document.querySelector('.machine-header')),
        background: getComputedStyle(remove).backgroundColor, border: getComputedStyle(remove).borderColor,
        bodyHeight: document.body.getBoundingClientRect().height, viewport: innerHeight,
        overflow: document.documentElement.scrollWidth - innerWidth };
    })()`;
    const before = await cdp.evaluate(geometry);
    assert.equal(before.coarse, true);
    assert.equal(before.hover, false);
    assert.ok(before.button.height >= 44 && before.pin.width >= 44 && before.edit.width >= 44 && before.edit.height >= 44 && before.remove.width >= 44 && before.remove.height >= 44 && before.node.height >= 44, JSON.stringify(before));
    assert.ok(before.button.left + before.button.width <= before.pin.left && before.pin.left + before.pin.width <= before.edit.left && before.edit.left + before.edit.width <= before.remove.left, "selection, pin, edit, and delete touch targets must not overlap");
    assert.ok(Math.abs((before.button.top + before.button.height / 2) - (before.remove.top + before.remove.height / 2)) <= 1, "delete must remain centered in the same row as its session");
    assert.ok(Math.abs(before.bodyHeight - before.viewport) < 2 && before.overflow <= 1, JSON.stringify(before));
    await cdp.send("CSS.forcePseudoState", { nodeId, forcedPseudoClasses: ["hover"] });
    await cdp.send("CSS.forcePseudoState", { nodeId: deleteId, forcedPseudoClasses: ["hover"] });
    assert.deepEqual(await cdp.evaluate(geometry), before, "a sticky touch hover must not change color or target geometry");

    // Chromium cannot reproduce WebKit's native keyboard bug. Mock only the
    // reported viewport/scroll boundary and keep actual DOM focus + panel
    // geometry to check recovery integration without claiming real iOS proof.
    await cdp.evaluate(`(() => {
      const viewport = Object.assign(new EventTarget(), { height: innerHeight, scale: 1, offsetTop: 0 });
      Object.defineProperty(window, 'visualViewport', { configurable: true, value: viewport });
      const originalScroll = window.scrollTo.bind(window);
      window.__resets = 0;
      window.scrollTo = (options) => { window.__resets += 1; viewport.offsetTop = 0; originalScroll(options); };
      (${installMobileViewportRecovery.toString()})({ window, document,
        isMobile: () => matchMedia('(max-width: 720px)').matches,
        syncViewport: () => document.documentElement.style.setProperty('--app-height', innerHeight + 'px') });
      document.querySelector('.rail').scrollTop = 210;
      document.getElementById('filter').focus({ preventScroll: true });
      viewport.height = innerHeight - 300; viewport.offsetTop = 300;
      viewport.dispatchEvent(new Event('resize'));
      return true;
    })()`);
    await delay(450);
    assert.equal(await cdp.evaluate("window.__resets"), 0);
    await cdp.evaluate("document.getElementById('filter').blur(); true");
    await delay(450);
    assert.equal(await cdp.evaluate("window.__resets"), 0, "keyboard animation still has a reduced visual viewport");
    await cdp.evaluate("visualViewport.height = innerHeight; visualViewport.offsetTop = 84; visualViewport.dispatchEvent(new Event('resize')); true");
    await waitFor(() => cdp.evaluate("window.__resets === 1"), "post-keyboard offset recovery");
    assert.equal(await cdp.evaluate("document.querySelector('.rail').scrollTop"), 210);
    const restored = await cdp.evaluate("({ body: document.body.getBoundingClientRect().height, viewport: innerHeight })");
    assert.ok(Math.abs(restored.body - restored.viewport) <= 1, JSON.stringify(restored));
    await cdp.evaluate("document.getElementById('dialog').showModal(); visualViewport.offsetTop = 84; visualViewport.dispatchEvent(new Event('resize')); true");
    await delay(450);
    assert.equal(await cdp.evaluate("window.__resets"), 1, "focused dialog editor must retain native reveal");
    await cdp.evaluate("document.getElementById('dialog').close(); document.activeElement.blur(); visualViewport.scale = 1.5; visualViewport.dispatchEvent(new Event('scroll')); true");
    await delay(450);
    assert.equal(await cdp.evaluate("window.__resets"), 1, "zoomed viewport must keep its pan");
    await cdp.evaluate("visualViewport.scale = 1; window.dispatchEvent(new Event('pageshow')); true");
    await waitFor(() => cdp.evaluate("window.__resets === 2"), "restored page recovery");
    // Use a fresh desktop target: Chromium retains emulated mobile pointer
    // capabilities on a renderer after touch emulation is disabled.
    cdp.socket.close();
    cdp = await connect(browserSocket);
    await cdp.send("Page.navigate", { url: `http://127.0.0.1:${server.address().port}` });
    await waitFor(() => cdp.evaluate("document.readyState === 'complete' && Boolean(document.querySelector('.session-row'))"), "desktop rail fixture");
    await cdp.send("DOM.enable");
    await cdp.send("CSS.enable");
    const desktopRoot = await cdp.send("DOM.getDocument");
    const desktopRow = await cdp.send("DOM.querySelector", { nodeId: desktopRoot.root.nodeId, selector: ".session-row" });
    await cdp.send("CSS.forcePseudoState", { nodeId: desktopRow.nodeId, forcedPseudoClasses: ["hover"] });
    const desktop = await cdp.evaluate(geometry);
    assert.equal(desktop.hover, true);
    assert.equal(desktop.background, "rgb(37, 23, 26)", "mouse hover retains delete affordance");
  } finally {
    cdp?.socket.close();
    if (chrome.exitCode === null && chrome.signalCode === null) {
      const exited = once(chrome, "exit");
      chrome.kill("SIGTERM");
      const killTimer = setTimeout(() => chrome.kill("SIGKILL"), 2500);
      try { await exited; } finally { clearTimeout(killTimer); }
    }
    server.closeAllConnections();
    await new Promise((resolve) => server.close(resolve));
    await rm(profile, { recursive: true, force: true, maxRetries: 10, retryDelay: 50 });
  }
});
