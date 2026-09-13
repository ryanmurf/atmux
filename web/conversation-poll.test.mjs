import assert from "node:assert/strict";
import { createRequire } from "node:module";
import test from "node:test";

const { createTranscriptPoller } = createRequire(import.meta.url)("./app.js");

function clock() {
  let time = 0;
  let sequence = 0;
  const timers = new Map();
  return {
    now: () => time,
    setTimer(fn, delay) {
      const id = ++sequence;
      timers.set(id, { fn, at: time + delay });
      return id;
    },
    clearTimer: (id) => timers.delete(id),
    async tick(duration) {
      const target = time + duration;
      // Promise.race + async finally settle before the next simulated timer.
      const flush = async () => { for (let i = 0; i < 10; i += 1) await Promise.resolve(); };
      await flush();
      while (true) {
        const next = [...timers].sort((a, b) => a[1].at - b[1].at)[0];
        if (!next || next[1].at > target) break;
        time = next[1].at;
        timers.delete(next[0]);
        next[1].fn();
        await flush();
      }
      time = target;
      await flush();
    },
    size: () => timers.size,
  };
}

function deferred() {
  let resolve, reject;
  const promise = new Promise((yes, no) => { resolve = yes; reject = no; });
  return { promise, resolve, reject };
}

test("slow Conversation reads are accepted without overlapping or poll supersession", async () => {
  const time = clock();
  const read = deferred();
  const received = [];
  let calls = 0;
  const poll = createTranscriptPoller({
    ...time, load: () => { calls += 1; return read.promise; },
    onData: (data) => received.push(data), onError: assert.fail,
  });
  poll.schedule(0);
  await time.tick(6000);
  assert.equal(calls, 1);
  read.resolve("slow but current");
  await time.tick(0);
  assert.deepEqual(received, ["slow but current"]);
  await time.tick(2499);
  assert.equal(calls, 1, "idle interval begins after the response");
  await time.tick(1);
  assert.equal(calls, 2);
  poll.close();
  assert.equal(time.size(), 0);
});

test("continuous pane patches cannot debounce Conversation reads forever", async () => {
  const time = clock();
  const starts = [];
  const poll = createTranscriptPoller({
    ...time, load: async () => { starts.push(time.now()); return "current"; },
    onData() {}, onError: assert.fail,
  });
  for (let i = 0; i < 30; i += 1) {
    poll.schedule(350);
    await time.tick(100);
  }
  assert.equal(starts[0], 350);
  assert.ok(starts.length >= 3 && starts.length <= 4, JSON.stringify(starts));
  assert.ok(starts.slice(1).every((at, i) => at - starts[i] >= 750));
  poll.close();
});

test("activity during a slow read coalesces into one bounded follow-up", async () => {
  const time = clock();
  const read = deferred();
  let calls = 0;
  const poll = createTranscriptPoller({
    ...time, load: () => { calls += 1; return read.promise; },
    onData() {}, onError: assert.fail,
  });
  poll.schedule(0);
  await time.tick(0);
  for (let i = 0; i < 60; i += 1) { poll.schedule(350); await time.tick(100); }
  assert.equal(calls, 1);
  read.resolve("current");
  await time.tick(749);
  assert.equal(calls, 1);
  await time.tick(1);
  assert.equal(calls, 2);
  await time.tick(2499);
  assert.equal(calls, 2, "60 patches must not become 60 requests");
  poll.close();
});

test("retiring a pane aborts its request and rejects late responses and errors", async () => {
  for (const failure of [false, true]) {
    const time = clock();
    const read = deferred();
    let signal;
    const poll = createTranscriptPoller({
      ...time, load: (value) => { signal = value; return read.promise; },
      onData: assert.fail, onError: assert.fail,
    });
    poll.schedule(0);
    await time.tick(0);
    poll.close();
    assert.equal(signal.aborted, true);
    if (failure) read.reject(new Error("retired pane"));
    else read.resolve("retired pane");
    poll.schedule(0);
    await time.tick(30_000);
    assert.equal(time.size(), 0);
  }
});

test("a stalled read times out, aborts and retries without clearing received data", async () => {
  const time = clock();
  const errors = [];
  const received = [];
  const signals = [];
  const poll = createTranscriptPoller({
    ...time,
    load(signal) {
      signals.push(signal);
      if (signals.length === 1) return Promise.resolve("last good view");
      if (signals.length === 2) return new Promise(() => {});
      return Promise.resolve("recovered");
    },
    onData: (data) => received.push(data), onError: (error) => errors.push(error.message),
  });
  poll.schedule(0);
  await time.tick(17_500);
  assert.deepEqual(received, ["last good view"]);
  assert.match(errors[0], /timed out; retrying automatically/);
  assert.equal(signals[1].aborted, true);
  await time.tick(2500);
  assert.deepEqual(received, ["last good view", "recovered"]);
  poll.close();
  assert.equal(time.size(), 0);
});
