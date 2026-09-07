import assert from "node:assert/strict";
import test from "node:test";
import { createRequire } from "node:module";

const { installMobileViewportRecovery } = createRequire(import.meta.url)("./app.js");

function fixture({ mobile = true, visualViewport = true } = {}) {
  let now = 0;
  let nextTimer = 0;
  const timers = new Map();
  const viewport = Object.assign(new EventTarget(), { height: 744, scale: 1, offsetTop: 0 });
  const window = Object.assign(new EventTarget(), {
    innerHeight: 744, scrollY: 0, visualViewport: visualViewport ? viewport : undefined,
    setTimeout(callback, delay) { const id = ++nextTimer; timers.set(id, { callback, due: now + delay }); return id; },
    clearTimeout(id) { timers.delete(id); },
  });
  const root = { scrollTop: 0 };
  const body = { top: 0, getBoundingClientRect() { return { top: this.top }; } };
  const document = Object.assign(new EventTarget(), {
    hidden: false, activeElement: body, body, scrollingElement: root, documentElement: root,
  });
  const panelScrolls = { rail: 480, transcript: 810, dialog: 120 };
  const scrolls = [];
  const heights = [];
  window.scrollTo = (position) => {
    scrolls.push(position);
    root.scrollTop = window.scrollY = viewport.offsetTop = body.top = 0;
  };
  const dispose = installMobileViewportRecovery({
    window, document, isMobile: () => mobile,
    syncViewport: () => { heights.push(window.innerHeight); },
  });
  const tick = (milliseconds = 400) => {
    now += milliseconds;
    for (const [id, task] of [...timers]) {
      if (task.due > now) continue;
      timers.delete(id);
      task.callback();
    }
  };
  const event = (target, type) => target.dispatchEvent(new Event(type));
  const focus = (editable = true) => {
    document.activeElement = editable ? { matches: () => true } : body;
    event(document, editable ? "focusin" : "focusout");
  };
  return { window, document, viewport, root, body, panelScrolls, scrolls, heights, tick, event, focus, dispose };
}

test("keyboard dismissal restores a stranded outer viewport while preserving panel scrolls", () => {
  const f = fixture();
  f.focus();
  f.viewport.height = 430;
  f.viewport.offsetTop = 314;
  f.event(f.viewport, "resize");
  f.tick();
  assert.deepEqual(f.scrolls, [], "the browser must reveal the focused composer itself");
  assert.deepEqual(f.heights, [], "recovery must not rewrite layout during focused keyboard animation");

  f.focus(false);
  f.tick();
  assert.deepEqual(f.scrolls, [], "blur is not proof that keyboard dismissal has finished");
  f.viewport.height = 744;
  f.viewport.offsetTop = 84;
  f.event(f.viewport, "resize");
  f.tick(100);
  f.event(f.viewport, "scroll");
  f.tick(100);
  assert.equal(f.scrolls.length, 0, "viewport animation events must settle first");
  f.tick(300);
  assert.deepEqual(f.scrolls, [{ top: 0, left: 0, behavior: "instant" }]);
  assert.equal(f.viewport.offsetTop, 0);
  assert.ok(f.heights.every((height) => height === 744), "visual height never drives app height");
  assert.deepEqual(f.panelScrolls, { rail: 480, transcript: 810, dialog: 120 });
});

test("moving focus into another editor cancels pending recovery", () => {
  const f = fixture();
  f.viewport.offsetTop = 84;
  f.focus(false);
  f.tick(100);
  f.focus();
  f.tick();
  assert.deepEqual(f.scrolls, []);
  f.event(f.viewport, "resize");
  f.tick();
  assert.deepEqual(f.scrolls, [], "full viewport with a focused hardware-keyboard field stays undisturbed");
  f.document.activeElement = { isContentEditable: true };
  f.event(f.viewport, "scroll");
  f.tick();
  assert.deepEqual(f.scrolls, [], "inherited contenteditable is protected too");
});

test("pinch zoom keeps its pan until normal scale returns", () => {
  const f = fixture();
  f.viewport.offsetTop = 100;
  f.viewport.scale = 1.5;
  f.tick();
  assert.deepEqual(f.scrolls, []);
  f.viewport.scale = 1;
  f.event(f.viewport, "resize");
  f.tick();
  assert.equal(f.scrolls.length, 1);
});

test("page restore and foregrounding remeasure then recover stale document scroll", () => {
  const f = fixture();
  f.event(f.window, "pagehide");
  f.root.scrollTop = 84;
  f.tick();
  assert.deepEqual(f.scrolls, []);
  f.window.innerHeight = f.viewport.height = 690;
  f.event(f.window, "pageshow");
  f.tick();
  assert.equal(f.heights.at(-1), 690);
  assert.equal(f.root.scrollTop, 0);
  assert.equal(f.scrolls.length, 1);
  f.document.hidden = true;
  f.event(f.document, "visibilitychange");
  f.window.scrollY = 84;
  f.event(f.viewport, "scroll");
  f.tick();
  assert.equal(f.scrolls.length, 1);
  f.document.hidden = false;
  f.event(f.document, "visibilitychange");
  f.tick();
  assert.equal(f.scrolls.length, 2);
});

test("orientation change uses the new layout height and does not cause repeated resets", () => {
  const f = fixture();
  f.window.innerHeight = f.viewport.height = 390;
  f.window.scrollY = 45;
  f.event(f.window, "orientationchange");
  f.tick();
  assert.deepEqual(f.heights, [390]);
  assert.equal(f.scrolls.length, 1);
  f.event(f.viewport, "scroll");
  f.event(f.window, "resize");
  f.tick();
  assert.equal(f.scrolls.length, 1, "the reset's own events do not trigger another reset");
});

test("WebKit's shifted body can recover even when reported scroll offsets are zero", () => {
  const f = fixture();
  f.body.top = -84;
  f.tick();
  assert.equal(f.scrolls.length, 1);
  assert.equal(f.body.top, 0);
});

test("desktop, missing viewport, and invalid viewport metrics do not reset the document", () => {
  for (const options of [{ mobile: false }, { visualViewport: false }]) {
    const f = fixture(options);
    f.window.scrollY = 84;
    f.tick();
    assert.deepEqual(f.scrolls, []);
  }
  for (const metrics of [{ height: NaN }, { scale: NaN }, { height: 0 }]) {
    const f = fixture();
    f.window.scrollY = 84;
    Object.assign(f.viewport, metrics);
    f.tick();
    assert.deepEqual(f.scrolls, []);
  }
});

test("cleanup removes pending recovery and viewport listeners", () => {
  const f = fixture();
  f.viewport.offsetTop = 84;
  f.dispose();
  f.event(f.viewport, "resize");
  f.event(f.window, "pageshow");
  f.event(f.document, "focusout");
  f.tick();
  assert.deepEqual(f.scrolls, []);
  assert.deepEqual(f.heights, []);
});
