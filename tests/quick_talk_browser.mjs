import assert from "node:assert/strict";

const [baseUrl = "http://127.0.0.1:7356", debugPort = "9224"] = process.argv.slice(2);

async function waitFor(predicate, timeoutMs = 5000) {
  const deadline = Date.now() + timeoutMs;
  while (Date.now() < deadline) {
    const value = await predicate();
    if (value) return value;
    await new Promise((resolve) => setTimeout(resolve, 50));
  }
  throw new Error("timed out waiting for browser state");
}

class Cdp {
  constructor(url) {
    this.nextId = 1;
    this.pending = new Map();
    this.socket = new WebSocket(url);
  }

  async open() {
    await new Promise((resolve, reject) => {
      this.socket.addEventListener("open", resolve, { once: true });
      this.socket.addEventListener("error", reject, { once: true });
    });
    this.socket.addEventListener("message", (event) => {
      const message = JSON.parse(event.data);
      if (!message.id || !this.pending.has(message.id)) return;
      const { resolve, reject } = this.pending.get(message.id);
      this.pending.delete(message.id);
      if (message.error) reject(new Error(message.error.message));
      else resolve(message.result);
    });
  }

  call(method, params = {}) {
    const id = this.nextId++;
    return new Promise((resolve, reject) => {
      this.pending.set(id, { resolve, reject });
      this.socket.send(JSON.stringify({ id, method, params }));
    });
  }

  close() {
    this.socket.close();
  }
}

const overview = await fetch(`${baseUrl}/api/v1/sessions`).then((response) => response.json());
const paneId = overview.sessions?.[0]?.id;
assert.ok(paneId, "atmux overview must expose at least one pane");

const page = await waitFor(async () => {
  const pages = await fetch(`http://127.0.0.1:${debugPort}/json/list`).then((response) => response.json());
  return pages.find((candidate) => candidate.type === "page" && candidate.url === "about:blank");
});
const cdp = new Cdp(page.webSocketDebuggerUrl);
await cdp.open();
await cdp.call("Page.enable");
await cdp.call("Runtime.enable");

const preload = String.raw`
  window.__speechInstances = [];
  class FakeSpeechRecognition {
    constructor() {
      this.starts = 0;
      this.stops = 0;
      this.aborts = 0;
      window.__speechInstances.push(this);
    }
    start() { this.starts += 1; }
    stop() {
      this.stops += 1;
      if (window.__suppressNextSpeechEnd) {
        window.__suppressNextSpeechEnd = false;
        return;
      }
      queueMicrotask(() => this.onend?.());
    }
    abort() { this.aborts += 1; }
  }
  window.SpeechRecognition = FakeSpeechRecognition;
  Element.prototype.setPointerCapture = function setPointerCapture() {};
  window.__sentMessages = [];
  window.__sentImageMessages = [];
  const originalFetch = window.fetch.bind(window);
  window.fetch = (input, options = {}) => {
    const url = String(input);
    if (options.method === "POST" && /\/image-messages$/.test(url)) {
      window.__sentImageMessages.push({ url, body: JSON.parse(options.body) });
      if (window.__holdNextImageMessage) {
        window.__holdNextImageMessage = false;
        return new Promise((resolve) => {
          window.__releaseHeldImageMessage = () => resolve(new Response(null, { status: 204 }));
        });
      }
      return Promise.resolve(new Response(null, { status: 204 }));
    }
    if (options.method === "POST" && /\/messages$/.test(url)) {
      const body = JSON.parse(options.body);
      window.__sentMessages.push({ url, body });
      if (window.__holdNextMessage || window.__holdMessageText === body.text) {
        window.__holdNextMessage = false;
        window.__holdMessageText = null;
        return new Promise((resolve) => {
          window.__releaseHeldMessage = () => resolve(new Response(null, { status: 204 }));
        });
      }
      return Promise.resolve(new Response(null, { status: 204 }));
    }
    return originalFetch(input, options);
  };
`;
await cdp.call("Page.addScriptToEvaluateOnNewDocument", { source: preload });
await cdp.call("Page.navigate", { url: `${baseUrl}/?session=${encodeURIComponent(paneId)}` });

async function evaluate(expression) {
  const result = await cdp.call("Runtime.evaluate", {
    expression,
    awaitPromise: true,
    returnByValue: true,
  });
  if (result.exceptionDetails) throw new Error(result.exceptionDetails.text);
  return result.result.value;
}

await waitFor(async () => evaluate(`Boolean(
  document.querySelector("#talk")
  && !document.querySelector("#talk").disabled
  && !document.querySelector("#agent-view").hidden
)`).catch(() => false));

const observed = await evaluate(`(async () => {
  const talk = document.querySelector("#talk");
  const message = document.querySelector("#message");
  const sleep = (milliseconds) => new Promise((resolve) => setTimeout(resolve, milliseconds));
  const speechResult = (transcript) => ({
    resultIndex: 0,
    results: [Object.assign([{ transcript }], { isFinal: true })],
  });
  const pasteImage = (name) => {
    const file = new File([new Uint8Array([137, 80, 78, 71, 13, 10, 26, 10])], name, { type: "image/png" });
    const event = new Event("paste", { bubbles: true, cancelable: true });
    Object.defineProperty(event, "clipboardData", { value: { files: [file] } });
    message.dispatchEvent(event);
  };
  const runSegmentedHold = async (transcript, pointerId, pointerType) => {
    talk.dispatchEvent(new PointerEvent("pointerdown", { bubbles: true, pointerId, pointerType }));
    const firstSegment = window.__speechInstances.at(-1);
    firstSegment.onresult(speechResult(transcript));
    firstSegment.onend();
    await sleep(350);
    const restartedSegment = window.__speechInstances.at(-1);
    talk.dispatchEvent(new PointerEvent("pointerup", { bubbles: true, pointerId, pointerType }));
    await sleep(50);
    return firstSegment !== restartedSegment;
  };

  pasteImage("first.png");
  message.value = "image race";
  window.__holdNextImageMessage = true;
  document.querySelector("#send").click();
  await sleep(50);
  pasteImage("must-not-join.png");
  const previewsDuringImageSend = document.querySelectorAll(".attachment-preview").length;
  window.__releaseHeldImageMessage();
  await sleep(50);
  const previewsAfterImageSend = document.querySelectorAll(".attachment-preview").length;

  const firstRestarted = await runSegmentedHold("quick talk integration", 7, "mouse");
  const secondRestarted = await runSegmentedHold("second quick talk", 8, "touch");

  window.__holdNextMessage = true;
  message.value = "busy send";
  document.querySelector("#send").click();
  await sleep(20);
  talk.dispatchEvent(new PointerEvent("pointerdown", { bubbles: true, pointerId: 9, pointerType: "mouse" }));
  const queuedRecognition = window.__speechInstances.at(-1);
  queuedRecognition.onresult(speechResult("queued speech"));
  talk.dispatchEvent(new PointerEvent("pointerup", { bubbles: true, pointerId: 9, pointerType: "mouse" }));
  await sleep(50);
  const sentBeforeRelease = window.__sentMessages.length;
  window.__holdMessageText = "queued speech";
  window.__releaseHeldMessage();
  await sleep(50);

  talk.dispatchEvent(new PointerEvent("pointerdown", { bubbles: true, pointerId: 12, pointerType: "touch" }));
  const behindQueuedRecognition = window.__speechInstances.at(-1);
  behindQueuedRecognition.onresult(speechResult("second queued"));
  talk.dispatchEvent(new PointerEvent("pointerup", { bubbles: true, pointerId: 12, pointerType: "touch" }));
  await sleep(50);
  const sentBeforeQueuedRelease = window.__sentMessages.length;
  window.__releaseHeldMessage();
  await sleep(100);

  window.__holdNextMessage = true;
  message.value = "validation blocker";
  document.querySelector("#send").click();
  await sleep(20);
  talk.dispatchEvent(new PointerEvent("pointerdown", { bubbles: true, pointerId: 13, pointerType: "mouse" }));
  const oversizedRecognition = window.__speechInstances.at(-1);
  oversizedRecognition.onresult(speechResult("x".repeat(70 * 1024)));
  talk.dispatchEvent(new PointerEvent("pointerup", { bubbles: true, pointerId: 13, pointerType: "mouse" }));
  await sleep(20);
  message.value = "";
  talk.dispatchEvent(new PointerEvent("pointerdown", { bubbles: true, pointerId: 14, pointerType: "mouse" }));
  const afterOversizedRecognition = window.__speechInstances.at(-1);
  afterOversizedRecognition.onresult(speechResult("queue survived validation"));
  talk.dispatchEvent(new PointerEvent("pointerup", { bubbles: true, pointerId: 14, pointerType: "mouse" }));
  await sleep(20);
  window.__releaseHeldMessage();
  await sleep(100);

  window.__suppressNextSpeechEnd = true;
  talk.dispatchEvent(new PointerEvent("pointerdown", { bubbles: true, pointerId: 10, pointerType: "mouse" }));
  const staleRecognition = window.__speechInstances.at(-1);
  const lateResult = staleRecognition.onresult;
  const lateEnd = staleRecognition.onend;
  staleRecognition.onresult(speechResult("first delayed"));
  talk.dispatchEvent(new PointerEvent("pointerup", { bubbles: true, pointerId: 10, pointerType: "mouse" }));
  await sleep(2100);

  talk.dispatchEvent(new PointerEvent("pointerdown", { bubbles: true, pointerId: 11, pointerType: "touch" }));
  const freshRecognition = window.__speechInstances.at(-1);
  lateResult(speechResult("stale text"));
  lateEnd();
  const secondHoldSurvivedLateEvents = talk.classList.contains("recording")
    && freshRecognition.stops === 0;
  freshRecognition.onresult(speechResult("fresh second"));
  talk.dispatchEvent(new PointerEvent("pointerup", { bubbles: true, pointerId: 11, pointerType: "touch" }));
  await sleep(50);

  const recognitions = window.__speechInstances;
  return {
    allContinuous: recognitions.every((recognition) => recognition.continuous),
    firstRestarted,
    secondRestarted,
    starts: recognitions.reduce((sum, recognition) => sum + recognition.starts, 0),
    stops: recognitions.reduce((sum, recognition) => sum + recognition.stops, 0),
    aborts: recognitions.reduce((sum, recognition) => sum + recognition.aborts, 0),
    sentBeforeRelease,
    sentBeforeQueuedRelease,
    previewsDuringImageSend,
    previewsAfterImageSend,
    secondHoldSurvivedLateEvents,
    buttonText: talk.textContent,
    recording: talk.classList.contains("recording"),
    sent: window.__sentMessages,
    sentImages: window.__sentImageMessages,
  };
})()`);

assert.equal(observed.allContinuous, true);
assert.equal(observed.firstRestarted, true, "recognition must restart while the mouse remains held");
assert.equal(observed.secondRestarted, true, "a later touch hold must restart independently");
assert.equal(observed.starts, 10);
assert.equal(observed.stops, 8);
assert.equal(observed.aborts, 1);
assert.equal(observed.sentBeforeRelease, 3, "dictation must wait behind the in-flight message");
assert.equal(observed.sentBeforeQueuedRelease, 4, "a later dictation must wait behind queued dictation");
assert.equal(observed.previewsDuringImageSend, 1, "a second paste must not mutate an in-flight image snapshot");
assert.equal(observed.previewsAfterImageSend, 0, "the delivered image snapshot must be removed exactly once");
assert.equal(observed.secondHoldSurvivedLateEvents, true);
assert.equal(observed.buttonText, "Hold to talk");
assert.equal(observed.recording, false);
assert.deepEqual(observed.sent, [
  {
    url: `/api/v1/panes/${encodeURIComponent(paneId)}/messages`,
    body: { text: "quick talk integration", submit: true },
  },
  {
    url: `/api/v1/panes/${encodeURIComponent(paneId)}/messages`,
    body: { text: "second quick talk", submit: true },
  },
  {
    url: `/api/v1/panes/${encodeURIComponent(paneId)}/messages`,
    body: { text: "busy send", submit: true },
  },
  {
    url: `/api/v1/panes/${encodeURIComponent(paneId)}/messages`,
    body: { text: "queued speech", submit: true },
  },
  {
    url: `/api/v1/panes/${encodeURIComponent(paneId)}/messages`,
    body: { text: "second queued", submit: true },
  },
  {
    url: `/api/v1/panes/${encodeURIComponent(paneId)}/messages`,
    body: { text: "validation blocker", submit: true },
  },
  {
    url: `/api/v1/panes/${encodeURIComponent(paneId)}/messages`,
    body: { text: "queue survived validation", submit: true },
  },
  {
    url: `/api/v1/panes/${encodeURIComponent(paneId)}/messages`,
    body: { text: "first delayed", submit: true },
  },
  {
    url: `/api/v1/panes/${encodeURIComponent(paneId)}/messages`,
    body: { text: "fresh second", submit: true },
  },
]);
assert.equal(observed.sentImages.length, 1);
assert.equal(observed.sentImages[0].url, `/api/v1/panes/${encodeURIComponent(paneId)}/image-messages`);
assert.equal(observed.sentImages[0].body.text, "image race");
assert.equal(observed.sentImages[0].body.images.length, 1);

cdp.close();
console.log(`Quick Talk browser integration passed for ${paneId}`);
