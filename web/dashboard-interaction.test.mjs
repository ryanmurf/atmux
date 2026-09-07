import assert from "node:assert/strict";
import test from "node:test";
import { createRequire } from "node:module";

const require = createRequire(import.meta.url);
const {
  overviewConnectionPresentation,
  createOverviewStream,
  selectedAgentUrl,
  copySelectedAgentLink,
  agentSearchShortcut,
  paneOutputBinding,
  paneOutputMatchesSession,
} = require("./app.js");

function overviewHarness({ accept = () => true } = {}) {
  const events = new Map();
  const timers = new Map();
  const states = [];
  const received = [];
  const faults = [];
  let sequence = 0;
  const source = {
    closed: false,
    addEventListener(name, callback) { events.set(name, callback); },
    close() { this.closed = true; },
  };
  const stream = createOverviewStream({
    createSource: () => source,
    onConnection: (state) => states.push(state),
    onOverview: (event) => { received.push(event); return accept(event); },
    onProtocolError: (event) => faults.push(event),
    setTimer(callback, delay) { timers.set(++sequence, { callback, delay }); return sequence; },
    clearTimer(id) { timers.delete(id); },
  });
  return {
    source, stream, states, timers, received, faults,
    emit(name, data = {}) { events.get(name)({ data }); },
    timeout() {
      const pending = [...timers.values()];
      timers.clear();
      for (const timer of pending) timer.callback();
    },
  };
}

test("overview is Live only after a snapshot, and an unchanged fleet stays Live", () => {
  const h = overviewHarness();
  assert.deepEqual(h.states, ["connecting"]);
  assert.equal([...h.timers.values()][0].delay, 15_000);
  h.source.onopen();
  assert.equal(h.states.at(-1), "connecting");
  h.emit("sessions.snapshot");
  assert.equal(h.states.at(-1), "live");
  assert.equal(h.timers.size, 0, "there is no time-since-patch timeout on an idle fleet");
  h.timeout();
  assert.equal(h.states.at(-1), "live");
});

test("missing initial snapshot becomes visibly stale and a later snapshot recovers", () => {
  const h = overviewHarness();
  h.source.onopen();
  h.timeout();
  assert.equal(h.states.at(-1), "stale");
  assert.equal(overviewConnectionPresentation(h.states.at(-1)).retry, true);
  h.emit("sessions.snapshot");
  assert.equal(h.states.at(-1), "live");
  assert.equal(overviewConnectionPresentation("live").retry, false);
});

test("native EventSource reconnects need their own authoritative snapshot", () => {
  const h = overviewHarness();
  h.emit("sessions.snapshot");
  h.source.onerror();
  assert.equal(h.states.at(-1), "reconnecting");
  assert.equal(h.timers.size, 0);
  h.source.onopen();
  h.emit("sessions.patch");
  assert.equal(h.states.at(-1), "stale");
  assert.equal(h.received.length, 1, "a patch before the new snapshot cannot merge into old state");
  h.emit("sessions.snapshot");
  h.emit("sessions.patch");
  assert.equal(h.states.at(-1), "live");
  assert.equal(h.received.length, 3);
});

test("protocol faults and rejected snapshots never advertise live updates", () => {
  const h = overviewHarness({ accept: () => false });
  h.emit("sessions.snapshot");
  assert.equal(h.states.at(-1), "stale");
  h.emit("protocol.error", "invalid revision");
  assert.equal(h.states.at(-1), "stale");
  assert.equal(h.faults.length, 1);
  assert.equal(h.timers.size, 0);
});

test("retry closes the previous stream without resetting selection or drafts", () => {
  const selected = { id: "midnight~%4", draft: "Keep this unsent message" };
  const h = overviewHarness({ accept: () => { selected.draft = "should not run"; return true; } });
  const queuedTimer = [...h.timers.values()][0].callback;
  h.stream.close();
  assert.equal(h.source.closed, true);
  assert.equal(h.timers.size, 0);
  h.source.onopen();
  h.source.onerror();
  h.emit("sessions.snapshot");
  h.emit("protocol.error");
  queuedTimer();
  assert.deepEqual(h.states, ["connecting"]);
  assert.deepEqual(h.received, []);
  assert.deepEqual(h.faults, []);
  assert.deepEqual(selected, { id: "midnight~%4", draft: "Keep this unsent message" });
});

test("a callback that replaces the stream cannot let the old stream overwrite connection state", () => {
  let stream;
  const h = overviewHarness({ accept: () => { stream.close(); return true; } });
  stream = h.stream;
  h.emit("sessions.snapshot");
  assert.deepEqual(h.states, ["connecting"]);
  assert.equal(h.timers.size, 0);
});

test("agent links preserve the current deployment URL and encode exact federated pane IDs", () => {
  const link = selectedAgentUrl("https://example.test/atmux/?machine=tron&view=usage&pulseAccount=7#reader", "midnight~%42");
  const url = new URL(link);
  assert.equal(url.origin, "https://example.test");
  assert.equal(url.pathname, "/atmux/");
  assert.equal(url.searchParams.get("session"), "midnight~%42");
  assert.equal(url.searchParams.has("machine"), false);
  assert.equal(url.searchParams.has("view"), false);
  assert.equal(url.searchParams.get("pulseAccount"), "7");
  assert.equal(url.hash, "#reader");
  assert.equal(selectedAgentUrl("https://example.test", null), null);
});

test("copy-link confirms only completed clipboard writes and explains unavailable or denied access", async () => {
  const copied = [];
  let resolveWrite;
  const pending = copySelectedAgentLink("https://example.test", "tron~%1", {
    writeText(value) { copied.push(value); return new Promise((resolve) => { resolveWrite = resolve; }); },
  });
  assert.equal(copied.length, 1, "write starts inside the click's user gesture");
  let done = false;
  pending.then(() => { done = true; });
  await Promise.resolve();
  assert.equal(done, false);
  resolveWrite();
  assert.equal(await pending, copied[0]);
  await assert.rejects(copySelectedAgentLink("https://example.test", "tron~%1"), /Clipboard access is unavailable/);
  await assert.rejects(copySelectedAgentLink("https://example.test", "tron~%1", {
    writeText() { return Promise.reject(new Error("Denied")); },
  }), /Could not copy the link/);
});

function key(key, target = {}, extra = {}) { return { key, target, ...extra }; }

test("search shortcuts never intercept editing, composition, dialogs, modifiers, or agent typing surfaces", () => {
  assert.equal(agentSearchShortcut(key("/")), "focus");
  for (const extra of [
    { defaultPrevented: true }, { isComposing: true }, { repeat: true },
    { ctrlKey: true }, { metaKey: true }, { altKey: true },
  ]) assert.equal(agentSearchShortcut(key("/", {}, extra)), null);
  assert.equal(agentSearchShortcut(key("/"), { dialogOpen: true }), null);
  assert.equal(agentSearchShortcut(key("/"), { searchVisible: false }), null);
  assert.equal(agentSearchShortcut(key("/", { isContentEditable: true })), null);
  for (const name of ["input", "textarea", "select", "[role='textbox']", "#conversation", "#pane"]) {
    const target = { closest(selector) { assert.ok(selector.includes(name)); return {}; } };
    assert.equal(agentSearchShortcut(key("/", target)), null, name);
  }
});

test("Escape clears a focused search once, then leaves it without affecting other controls", () => {
  assert.equal(agentSearchShortcut(key("Escape", { id: "filter", value: "Codex" })), "clear");
  assert.equal(agentSearchShortcut(key("Escape", { id: "filter", value: "" })), "blur");
  assert.equal(agentSearchShortcut(key("Escape", { id: "message", value: "Draft" })), null);
  assert.equal(agentSearchShortcut(key("Escape", { id: "filter", value: "Codex" }), { dialogOpen: true }), null);
});

test("raw output ownership requires the same pane and captures its instance independently", () => {
  const session = { id: "tron~%1", instance_id: "pane-v1-" + "a".repeat(64) };
  const binding = paneOutputBinding(session);
  assert.equal(paneOutputMatchesSession(binding, session), true);
  assert.equal(paneOutputMatchesSession(binding, { ...session, machine: "tron", online: false }), true);
  assert.equal(paneOutputMatchesSession(binding, { ...session, id: "midnight~%1" }), false);
  session.instance_id = "pane-v1-" + "b".repeat(64);
  assert.equal(paneOutputMatchesSession(binding, session), false);
  assert.equal(paneOutputMatchesSession(null, session), false);
  assert.equal(paneOutputMatchesSession(binding, null), false);
  assert.equal(paneOutputBinding(null), null);
});
