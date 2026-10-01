import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
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

test("session edits send only changed fields bound to the pane seen at open", () => {
  const instance = "pane-v1-" + "a".repeat(64);
  const session = { id: "max~%1", instance_id: instance, name: "review", description: "Old note" };
  assert.deepEqual(app.sessionEditRequest(session, " review-2 ", "Old note"), {
    id: "max~%1",
    body: { instance_id: instance, name: "review-2" },
  });
  assert.deepEqual(app.sessionEditRequest(session, "review", "  Payments; phase 2 ✓ "), {
    id: "max~%1",
    body: { instance_id: instance, description: "Payments; phase 2 ✓" },
  });
  assert.deepEqual(app.sessionEditRequest(session, "review", "   ").body, { instance_id: instance, description: "" });
  assert.deepEqual(app.sessionEditRequest({ ...session, description: undefined }, "review", ""), { unchanged: true });
  assert.deepEqual(app.sessionEditRequest(session, "review", "Old note"), { unchanged: true });
  assert.equal(app.sessionEditRequest(session, "-dev", "Old note").body.name, "-dev");

  for (const [name, description] of [
    ["bad name", ""],
    ["", ""],
    ["x".repeat(101), ""],
    ["atmux-web", ""],
    ["review", "two\nlines"],
    ["review", "tab\there"],
    ["review", "é".repeat(121)],
  ]) {
    assert.ok(app.sessionEditRequest(session, name, description).error, JSON.stringify([name, description]));
  }
  assert.ok(app.sessionEditRequest(session, "review", "é".repeat(120)).body);
  assert.equal(app.sessionEditRequest({ ...session, description_source: "auto" }, "review", "Old note").body.description, "Old note", "saving an auto note explicitly makes it user-owned");
  const external = { ...session, name: "notes.2 draft" };
  assert.deepEqual(app.sessionEditRequest(external, "notes.2 draft", "Keep the odd name"), {
    id: "max~%1",
    body: { instance_id: instance, description: "Keep the odd name" },
  }, "a description edit must not require renaming a session created outside atmux");
  assert.ok(app.sessionEditRequest(external, "notes.3 draft", "").error);
  assert.ok(app.sessionEditRequest({ ...session, instance_id: "" }, "renamed", "").error);
  assert.ok(app.sessionEditRequest(null, "renamed", "").error);
});

test("rail rows expose a rename action wired to a pane-bound PATCH", () => {
  const source = readFileSync(new URL("./app.js", import.meta.url), "utf8");
  const html = readFileSync(new URL("./index.html", import.meta.url), "utf8");
  const css = readFileSync(new URL("./app.css", import.meta.url), "utf8");
  assert.match(source, /editButton\.addEventListener\("click", \(\) => openSessionEditDialog\(id\)\)/);
  assert.match(source, /li\.append\(button, pinButton, editButton, deleteButton\)/);
  assert.match(source, /request\(sessionDeletePath\(edit\.id\), \{ method: "PATCH", body: JSON\.stringify\(edit\.body\) \}\)/);
  assert.match(source, /sessionEditRequest\(pending, /, "the request must use the session captured when the dialog opened");
  assert.match(html, /<dialog id="session-edit-dialog"[\s\S]*id="session-edit-name"[^>]*maxlength="100"[\s\S]*id="session-edit-description"[^>]*maxlength="120"/);
  assert.match(css, /@media \(pointer: coarse\) \{[^}]*\.session-row \{ grid-template-columns: minmax\(0, 1fr\) 44px 44px 44px; \}/);
});
