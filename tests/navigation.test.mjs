import assert from "node:assert/strict";
import { createRequire } from "node:module";
import test from "node:test";

const require = createRequire(import.meta.url);
const { favoriteSessionKey, navigationPreferences, navigationView } = require("../web/app.js");
const instance = (digit) => `pane-v1-${digit.repeat(64)}`;
const machines = [
  { id: "local", label: "Workstation", kind: "local", online: true },
  { id: "max", label: "Max", kind: "remote", online: true },
  { id: "offline", label: "Offline", kind: "remote", online: false },
];
const sessions = [
  { id: "%1", machine: "local", name: "alpha", path: "/work/api", profile: "daily", agent: "codex", status: "working", instance_id: instance("a") },
  { id: "%2", machine: "local", name: "zebra", path: "/work/web", agent: "claude", status: "waiting", instance_id: instance("b") },
  { id: "max~%1", machine: "max", name: "beta", path: "/work/api", agent: "claude", status: "waiting", instance_id: instance("c") },
];

test("collapsed nodes retain sessions and searches reveal matching agents without changing saved state", () => {
  const options = { collapsed: new Set(["local"]) };
  const normal = navigationView(sessions, machines, options);
  assert.equal(normal.groups[0].collapsed, true);
  assert.equal(normal.groups[0].sessions.length, 2);
  assert.equal(normal.matchedCount, 3, "collapsing is distinct from filtering");
  const search = navigationView(sessions, machines, { ...options, query: "  API  " });
  assert.deepEqual(search.groups.map(({ machine }) => machine.id), ["local", "max"]);
  assert.equal(search.groups[0].collapsed, false);
  assert.equal(search.groups[0].filtering, true);
  assert.equal(search.matchedCount, 2);
  assert.deepEqual([...options.collapsed], ["local"]);
  assert.equal(navigationView(sessions, machines, options).groups[0].collapsed, true);
});

test("status, harness, and text filters intersect and preserve empty machines only without filters", () => {
  const result = navigationView(sessions, machines, {
    status: "waiting", harness: "claude", query: "api", collapsed: ["max"],
  });
  assert.equal(result.totalCount, 3);
  assert.equal(result.matchedCount, 1);
  assert.equal(result.groups[0].machine.id, "max");
  assert.equal(result.groups[0].collapsed, false);
  assert.deepEqual(navigationView(sessions, machines, { status: "working", harness: "claude" }).groups, []);
  assert.equal(navigationView(sessions, machines).groups.length, 3);
  assert.equal(navigationView(sessions, machines, { query: "Workstation" }).matchedCount, 2, "human machine labels are searchable");
  assert.equal(navigationView(sessions, machines, { query: "daily" }).matchedCount, 1, "profiles remain searchable");
});

test("favorite order stays deterministic inside its owning machine and respects filters", () => {
  const favorite = favoriteSessionKey(sessions[1]);
  const options = { favorites: new Set([favorite]) };
  const result = navigationView(sessions, machines, options);
  assert.deepEqual(result.groups.map(({ machine }) => machine.id), ["local", "max", "offline"]);
  assert.deepEqual(result.groups[0].sessions.map(({ name }) => name), ["zebra", "alpha"]);
  const statusChanged = sessions.map((session) => ({ ...session, status: session.status === "working" ? "waiting" : "working" }));
  assert.deepEqual(navigationView(statusChanged, machines, options).groups[0].sessions.map(({ id }) => id), result.groups[0].sessions.map(({ id }) => id));
  assert.deepEqual(navigationView(sessions, machines, { ...options, harness: "codex" }).groups[0].sessions.map(({ name }) => name), ["alpha"]);
  assert.deepEqual(sessions.map(({ name }) => name), ["alpha", "zebra", "beta"], "rendering does not mutate the source snapshot");
});

test("favorites cannot transfer to recycled pane ids or another machine", () => {
  const original = favoriteSessionKey(sessions[0]);
  const recycled = { ...sessions[0], instance_id: instance("d") };
  assert.notEqual(favoriteSessionKey(recycled), original);
  assert.notEqual(favoriteSessionKey({ ...sessions[0], machine: "max" }), original);
  assert.equal(favoriteSessionKey({ ...sessions[0], instance_id: undefined }), null);
  assert.equal(favoriteSessionKey({ ...sessions[0], instance_id: "pane-v1-invalid" }), null);
  assert.equal(favoriteSessionKey({ ...sessions[0], machine: "../../escape" }), null);
  assert.equal(favoriteSessionKey(null), null);
  assert.equal(favoriteSessionKey({ ...sessions[0], machine: undefined }, "local"), original);
});

test("navigation preferences validate corrupt storage and bound accumulated stale pane generations", () => {
  for (const raw of [null, "{", "[]", "null", "x".repeat(65_537)]) {
    assert.deepEqual(navigationPreferences(raw), { collapsed: [], favorites: [] });
  }
  const favorite = favoriteSessionKey(sessions[1]);
  assert.deepEqual(navigationPreferences(JSON.stringify({
    collapsed: ["local", "local", "bad machine", "<script>", 3],
    favorites: [favorite, favorite, "%1", `ephemeral:%1`, `pane:%ZZ:${instance("a")}`],
  })), { collapsed: ["local"], favorites: [favorite] });
  const many = Array.from({ length: 300 }, (_, index) => `node-${index}`);
  const prefs = navigationPreferences({ collapsed: many, favorites: many.map((machine) => `pane:${machine}:${instance("a")}`) });
  assert.equal(prefs.collapsed.length, 256);
  assert.equal(prefs.favorites.length, 256);
  assert.equal(prefs.collapsed.at(-1), "node-299", "newest user choices survive eviction");
  assert.deepEqual(navigationPreferences(JSON.stringify(prefs)), prefs);
});
