import assert from "node:assert/strict";
import test from "node:test";
import app from "./app.js";

test("resume picker includes online owners and excludes coordinators and offline targets", () => {
  const local = { id: "mac", kind: "local", online: true };
  const remote = { id: "linux", kind: "remote", online: true };
  assert.deepEqual(app.resumeMachineOptions([local, remote, { id: "home", kind: "coordinator", online: true }, { id: "off", kind: "remote", online: false }]), [local, remote]);
});
test("resume intent binds the durable key when the dialog opens and defaults to copy", () => {
  const key = "019a06d9-8341-7654-8abc-0123456789ab";
  const session = { session_key: key, machine: "linux" };
  const request = app.sessionResumeIntent(session, "mac");
  session.session_key = "new-key";
  assert.deepEqual(request, { session_key: key, machine: "mac", move: false });
  assert.deepEqual(app.sessionResumeIntent({ session_key: key }, "mac", true), { session_key: key, machine: "mac", move: true });
  assert.equal(app.sessionResumeIntent({ session_key: key }, "../unknown"), null);
  assert.equal(app.sessionResumeIntent({}, "mac"), null);
});
