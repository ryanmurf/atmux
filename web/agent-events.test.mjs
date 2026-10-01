import test from "node:test";
import assert from "node:assert/strict";
import { createRequire } from "node:module";
const { reconcileAgentEvents, needsInputReason, needsInputLabel } = createRequire(import.meta.url)("./app.js");

const session = { session_key: "key", machine: "tron", instance_id: "generation", status: "working" };
const record = (type, reason = null, instance_id = "generation") => ({ event: { type, reason, instance_id, session_key: "key", machine: "tron" } });

test("needs input events are immediate, reasoned, and cleared on working", () => {
  let events = reconcileAgentEvents(new Map(), { events: [record("agent.needs_input", "permission")] });
  assert.equal(needsInputReason(session, events), "permission");
  assert.equal(needsInputLabel("permission"), "Needs input · permission");
  events = reconcileAgentEvents(events, { events: [record("agent.working")] });
  assert.equal(needsInputReason({ ...session, status: "waiting" }, events), null);
  events = reconcileAgentEvents(events, { events: [record("agent.turn_completed")] });
  assert.equal(needsInputReason(session, events), "idle_prompt");
});

test("badge falls back to status and rejects stale generations and unknown reasons", () => {
  const stale = reconcileAgentEvents(new Map(), { events: [record("agent.needs_input", "startup_prompt", "old")] });
  assert.equal(needsInputReason(session, stale), null);
  assert.equal(needsInputReason({ ...session, status: "waiting" }, stale), "idle_prompt");
  assert.equal(needsInputLabel("<script>"), "");
  assert.equal(needsInputReason(session, new Map()), null);
  assert.equal(needsInputReason({ ...session, status: "waiting" }, new Map()), "idle_prompt");
});

test("retention reset discards stale attention and unrelated events preserve it", () => {
  const prior = reconcileAgentEvents(new Map(), { events: [record("agent.needs_input", "question")] });
  const preserved = reconcileAgentEvents(prior, { events: [record("agent.summary_updated")] });
  assert.equal(needsInputReason(session, preserved), "question");
  assert.equal(reconcileAgentEvents(prior, { reset: true, events: [] }).size, 0);
});
