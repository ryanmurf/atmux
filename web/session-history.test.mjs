import assert from "node:assert/strict";
import { createRequire } from "node:module";
import test from "node:test";
const require = createRequire(import.meta.url);
const { appRoute, sessionHistoryQuery, sessionHistoryRow, sessionHistoryResumeRequest } = require("./app.js");

test("history navigation and bounded filters share the REST contract", () => {
  assert.deepEqual(appRoute("https://atmux.test/?view=sessions"), { view: "sessions", id: null });
  const url = new URL(sessionHistoryQuery({ text: " Unicode ✓ & task ", machine: "owner", state: "archived", project: "repo" }, "cursor"), "https://atmux.test");
  assert.equal(url.pathname, "/api/v1/session-history");
  assert.equal(url.searchParams.get("text"), "Unicode ✓ & task");
  assert.equal(url.searchParams.get("limit"), "100");
  assert.equal(url.searchParams.get("cursor"), "cursor");
  assert.equal(new URL(sessionHistoryQuery({ text: "x".repeat(9000) }), "https://atmux.test").searchParams.get("text").length, 2048);
});

test("history rows and resume hook contain only the public stable identity", () => {
  const record = { session_key: "stable", machine: "peer", name: "<script>hello</script>", description: "Task", project: { remote: "https://github.com/org/repo" }, state: "archived", last_active_ms: 1000,
    native: { session_id: "private-id", config_root: "/private/root" } };
  const row = sessionHistoryRow(record);
  assert.equal(row.name, "<script>hello</script>");
  assert.equal(row.project, "https://github.com/org/repo");
  assert.equal(row.state, "archived");
  assert.equal(JSON.stringify(row).includes("private"), false);
  assert.deepEqual(sessionHistoryResumeRequest(row), { session_key: "stable", machine: "peer" });
  assert.equal(sessionHistoryRow({ state: "future-state" }).state, "unknown");
});
