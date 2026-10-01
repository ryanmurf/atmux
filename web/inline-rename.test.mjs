import assert from "node:assert/strict";
import test from "node:test";
import app from "./app.js";

class Element {
  constructor(tag) { this.tag = tag; this.children = []; this.style = {}; this.events = new Map(); }
  setAttribute(key, value) { this[key] = value; }
  addEventListener(key, handler) { this.events.set(key, handler); }
  append(...children) { this.children.push(...children); for (const child of children) child.parent = this; }
  remove() { this.parent.children = this.parent.children.filter((child) => child !== this); }
  focus() { this.focused = true; }
  select() { this.selected = true; }
  dispatch(type, values = {}) {
    const event = { preventDefault() { this.prevented = true; }, stopPropagation() { this.stopped = true; }, ...values };
    this.events.get(type)?.(event); return event;
  }
}
const settle = () => new Promise((resolve) => setImmediate(resolve));
const session = () => ({ id: "tron~%1", instance_id: "pane-v1-" + "a".repeat(64), name: "review", description: "Keep user note" });
const document = { createElement: (tag) => new Element(tag) };

test("inline Enter saves a captured generation, Escape cancels, and invalid names stay editable", async () => {
  const original = session(); const host = new Element("li"); const anchor = new Element("span"); const saves = [];
  const editor = app.createInlineRenameEditor({ document, host, anchor, session: original, save: async (edit) => saves.push(edit) });
  assert.equal(editor.input.value, "review"); assert.equal(anchor.style.visibility, "hidden");
  original.instance_id = "pane-v1-" + "b".repeat(64); original.description = "Changed outside editor";
  editor.input.value = "bad name"; editor.input.dispatch("keydown", { key: "Enter" }); await settle();
  assert.equal(saves.length, 0); assert.equal(host.children.length, 1);
  editor.input.value = "review-2"; const event = editor.input.dispatch("keydown", { key: "Enter" }); await settle();
  assert.ok(event.prevented && event.stopped); assert.deepEqual(saves[0], {
    id: "tron~%1", body: { instance_id: "pane-v1-" + "a".repeat(64), name: "review-2" },
  });
  assert.equal(host.children.length, 0); assert.equal(anchor.style.visibility, "");
  const cancelled = app.createInlineRenameEditor({ document, host, anchor, session: session(), save: async (edit) => saves.push(edit) });
  cancelled.input.value = "cancelled"; cancelled.input.dispatch("keydown", { key: "Escape" }); await settle();
  assert.equal(saves.length, 1); assert.equal(host.children.length, 0);
});

test("Suggest fills a validated generated title and never saves implicitly", async () => {
  const host = new Element("header"); const anchor = new Element("h1"); let saves = 0;
  const editor = app.createInlineRenameEditor({ document, host, anchor, session: session(), save: async () => saves++,
    suggest: async () => "Durable session summaries" });
  host.children[0].children[1].dispatch("click"); await settle();
  assert.equal(editor.input.value, "durable-session-summaries"); assert.equal(saves, 0);
  assert.equal(app.generatedSessionName("ATMUX WEB"), "");
  assert.equal(app.generatedSessionName(""), "");
  assert.equal(app.generatedSessionName("Résumé ✨"), "resume");
  editor.close();
});

test("F2 opens only a selected session outside dialogs and text fields; IME Enter does not save", () => {
  assert.equal(app.inlineRenameAction({ key: "F2" }, { selected: true }), "open");
  assert.equal(app.inlineRenameAction({ key: "F2" }, { selected: false }), null);
  assert.equal(app.inlineRenameAction({ key: "F2" }, { selected: true, dialogOpen: true }), null);
  assert.equal(app.inlineRenameAction({ key: "F2", target: { closest: () => ({}) } }, { selected: true }), null);
  assert.equal(app.inlineRenameAction({ key: "F2", ctrlKey: true }, { selected: true }), null);
  assert.equal(app.inlineRenameAction({ key: "Enter", isComposing: true }), null);
  assert.equal(app.inlineRenameAction({ key: "Escape" }), "cancel");
});

test("double-click and a stationary touch hold open rename, movement cancels the hold", () => {
  const node = new Element("span"); let calls = 0; let pending = null;
  const timers = { setTimeout(callback, delay) { assert.equal(delay, 600); pending = callback; return 1; }, clearTimeout() { pending = null; } };
  app.bindInlineRenameGesture(node, () => calls++, timers);
  node.dispatch("dblclick"); assert.equal(calls, 1);
  node.dispatch("pointerdown", { pointerType: "touch", clientX: 0, clientY: 0 });
  node.dispatch("pointermove", { clientX: 20, clientY: 0 }); assert.equal(pending, null);
  node.dispatch("pointerdown", { pointerType: "touch", clientX: 0, clientY: 0 }); pending(); assert.equal(calls, 2);
  assert.ok(node.dispatch("click").prevented);
});
