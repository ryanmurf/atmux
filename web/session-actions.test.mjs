import assert from "node:assert/strict";
import test from "node:test";
import app from "./app.js";

test("restart confirmation captures identity independently of live selection", () => {
  const session = { id: "max~%1", instance_id: "pane-v1-" + "a".repeat(64) };
  const capabilities = { pane_id: session.id, restart_token: "restart-v1-" + "c".repeat(64) };
  const request = app.agentRestartRequest(session, capabilities);
  session.instance_id = "pane-v1-" + "b".repeat(64);
  capabilities.restart_token = "restart-v1-" + "d".repeat(64);
  assert.equal(request.instance_id, "pane-v1-" + "a".repeat(64));
  assert.equal(request.id, "max~%1");
  assert.equal(request.restart_token, "restart-v1-" + "c".repeat(64));
  assert.equal(app.agentRestartRequest({ id: "%1" }), null);
  assert.equal(app.agentRestartRequest(session, { ...capabilities, pane_id: "max~%2" }), null);
});

test("raw output download preserves text and bounds the filename", () => {
  const result = app.paneOutputDownload({ id: "max~%1", name: "../../agent <script>" }, ["<script>hello</script>", "  spacing", ""]);
  assert.equal(result.content, "<script>hello</script>\n  spacing\n\n");
  assert.ok(!result.filename.includes("/"));
  assert.ok(!result.filename.includes("<"));
  assert.equal(app.paneOutputDownload({ id: "%1" }, []), null);
  assert.equal(app.paneOutputDownload(null, ["old pane"]), null);
});
