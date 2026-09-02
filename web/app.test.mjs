import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import { createRequire } from "node:module";
import test from "node:test";

const require = createRequire(import.meta.url);
const {
  MAX_MESSAGE_BYTES,
  MAX_IMAGE_ATTACHMENTS,
  MAX_IMAGE_BYTES,
  MAX_TOTAL_IMAGE_BYTES,
  MAX_FILE_REFERENCE_CHARS,
  MAX_FILE_REFERENCE_LINES,
  attachmentDeliveryTarget,
  agentMenuUrl,
  appRoute,
  arrayBufferToBase64,
  applyPanePatch,
  classifyOverviewUpdate,
  composerEnterAction,
  composerSubmissionCanRestore,
  composerSubmissionMatches,
  contentToLines,
  dictationDelivery,
  dictationEndAction,
  dictationErrorPolicy,
  dictationPrefix,
  dictationRestartDelay,
  duplicateLaunchSelection,
  duplicateSourceMatches,
  duplicateSourceSnapshot,
  duplicateSessionName,
  defaultMemoryLimitLabel,
  filterDirectories,
  formatMemoryLimit,
  memoryLimitChoices,
  parseMemoryLimitSelection,
  followsLiveTail,
  formatUptime,
  formatRelativeTime,
  groupSessionsByMachine,
  gpuSummary,
  gpuDetailLines,
  gpuDiagnosticLines,
  systemMetricLines,
  harnessesForProfiles,
  highlightCode,
  inlineTokens,
  isMachineControllable,
  isLaunchCapableMachine,
  isManualDirectory,
  rememberedLaunchDirectories,
  rememberLaunchDirectory,
  availableLaunchDirectories,
  launchDirectoryBrowsePath,
  launchMachines,
  imageFilesFromTransfer,
  machineStatusLabel,
  claudeResumeState,
  modelPickerState,
  markdownBlocks,
  messageFitsByteLimit,
  moveMessageHistory,
  paneTypingText,
  paneErrorLabel,
  paneFilesPath,
  paneGitPath,
  paneNotice,
  parseCompositeId,
  projectEntryKind,
  fileCanEdit,
  fileEditHasUnsavedWork,
  fileReaderPreferences,
  fileReaderPreferenceJson,
  loadFileReaderPreferences,
  conversationVisibilityPreferences,
  conversationVisibilityPreferenceJson,
  loadConversationVisibilityPreferences,
  saveConversationVisibilityPreferences,
  fileReferenceBlock,
  insertComposerReference,
  nextFileLineSelection,
  reconcileSavedFileDraft,
  projectFilePreview,
  projectRelativePath,
  pulseAccountId,
  pulseAccountLabel,
  pulseAccountPath,
  pulseAccounts,
  pulseAlertActionPath,
  pulseCanFollowCursor,
  pulseProfileVisibilityPath,
  pulseProfileSettingsPath,
  pulseForcePollPath,
  pulseEventsPath,
  pulseInvalidationAction,
  pulseIngestTokenPath,
  pulsePricingPath,
  pulseReconnectDelay,
  pulseRevisionId,
  pulseRefreshDelay,
  pulseRequestStillCurrent,
  pulseSubscriptionPath,
  preferredPulseAccount,
  preferredLaunchMachineId,
  remainingAttachmentsAfterDelivery,
  profilesForHarness,
  projectLabel,
  projectPreference,
  reconcileSessions,
  reduceOverview,
  reduceTranscript,
  sessionDeletePath,
  sessionFolderLabel,
  sessionMachineId,
  sessionProfileLabel,
  sourceLanguage,
  safeLinkUrl,
  savedSessionConfirmation,
  savedSessionPreview,
  selectionTouchesPane,
  transcriptAnchorMembers,
  sortSessions,
  presentSessionStatuses,
  WORKING_TO_WAITING_HOLD_MS,
  suggestedSessionName,
  transcriptItemKind,
  transcriptVisibilityKind,
  transcriptItemIsVisible,
  filterTranscriptMessages,
  normalizedToolName,
  coordinationResultSignal,
  execResultClass,
  toolResultSignal,
  collapsibleCoordinationTool,
  internalToolGroupKey,
  compactTranscriptItems,
  toolGroupSummary,
  diffLineKind,
  utf8ByteLength,
  validateImageSelection,
  validContentHash,
} = require("./app.js");

function session(id, status, name = id) {
  return { id, status, name };
}

function federated(machine, pane, status, name = pane) {
  return { id: `${machine}~${pane}`, machine, pane_id: pane, status, name };
}

function machine(id, label, online, extra = {}) {
  return { id, label, kind: id === "local" ? "local" : "remote", online, sessions: 0, ...extra };
}

test("sortSessions keeps click targets in deterministic name/id order across status changes", () => {
  const sorted = sortSessions([
    session("other", "other", "Zulu"),
    session("waiting-b", "waiting", "agent-10"),
    session("working", "working", "Zulu"),
    session("waiting-a", "waiting", "agent-2"),
    session("same-b", "waiting", "same"),
    session("same-a", "waiting", "same"),
  ]);
  assert.deepEqual(sorted.map(({ id }) => id), [
    "waiting-a",
    "waiting-b",
    "same-a",
    "same-b",
    "other",
    "working",
  ]);
  const changed = sorted.map((item) => ({ ...item, status: item.status === "working" ? "waiting" : "working" }));
  assert.deepEqual(sortSessions(changed).map(({ id }) => id), sorted.map(({ id }) => id));
});

test("status presentation shows work immediately and requires a continuous quiet hold for waiting", () => {
  const waiting = session("midnight~%5", "waiting", "planner");
  let view = presentSessionStatuses(new Map(), [waiting], 1_000);
  assert.equal(view.sessions[0].status, "waiting", "an initial waiting state is not hidden");

  view = presentSessionStatuses(view.presentations, [{ ...waiting, status: "working" }], 1_050);
  assert.equal(view.sessions[0].status, "working", "working becomes visible immediately");

  view = presentSessionStatuses(view.presentations, [waiting], 1_100);
  assert.equal(view.sessions[0].status, "working");
  assert.equal(view.nextDelay, WORKING_TO_WAITING_HOLD_MS);

  view = presentSessionStatuses(view.presentations, [waiting], 1_100 + WORKING_TO_WAITING_HOLD_MS - 1);
  assert.equal(view.sessions[0].status, "working");
  assert.equal(view.nextDelay, 1);

  view = presentSessionStatuses(view.presentations, [{ ...waiting, status: "working" }], 2_000);
  view = presentSessionStatuses(view.presentations, [waiting], 2_100);
  view = presentSessionStatuses(view.presentations, [waiting], 2_100 + WORKING_TO_WAITING_HOLD_MS);
  assert.equal(view.sessions[0].status, "waiting", "continuous quiet eventually becomes visible");
  assert.equal(view.nextDelay, null);
});

test("status presentation exposes other states immediately and prunes removed sessions", () => {
  let view = presentSessionStatuses(new Map(), [session("agent", "working")], 1_000);
  view = presentSessionStatuses(view.presentations, [session("agent", "other")], 1_001);
  assert.equal(view.sessions[0].status, "other");
  view = presentSessionStatuses(view.presentations, [], 1_002);
  assert.equal(view.presentations.size, 0);
  assert.equal(view.nextDelay, null);
});

test("reconcileSessions handles authoritative snapshots without replacing unchanged membership", () => {
  const oldA = session("a", "working");
  const updatedA = session("a", "waiting");
  const result = reconcileSessions(new Map([["a", oldA], ["stale", session("stale", "other")]]), {
    revision: 2,
    sessions: [updatedA, session("b", "working")],
  });
  assert.deepEqual([...result.keys()], ["a", "b"]);
  assert.equal(result.get("a"), updatedA);
});

test("reconcileSessions applies incremental removals and upserts", () => {
  const result = reconcileSessions(new Map([["a", session("a", "working")], ["b", session("b", "other")]]), {
    remove: ["a"],
    upsert: [session("b", "waiting"), session("c", "working")],
  });
  assert.deepEqual([...result.keys()], ["b", "c"]);
  assert.equal(result.get("b").status, "waiting");
});

test("contentToLines represents empty pane content as zero lines", () => {
  assert.deepEqual(contentToLines(""), []);
  assert.deepEqual(contentToLines(null), []);
  assert.deepEqual(contentToLines("one\ntwo"), ["one", "two"]);
});

test("live tail following requires the reader to remain at the actual tail", () => {
  const conversation = { scrollHeight: 2_000, scrollTop: 1_560, clientHeight: 400 };
  assert.equal(followsLiveTail(conversation), false);
  conversation.scrollTop = 1_584;
  assert.equal(followsLiveTail(conversation), true);
});

test("streaming redraws use semantic transcript anchors and explicit reader intent", () => {
  const source = readFileSync(new URL("./app.js", import.meta.url), "utf8");
  const css = readFileSync(new URL("./app.css", import.meta.url), "utf8");
  assert.match(source, /const shouldFollow = state\.transcriptFollowing\s*&& followsLiveTail\(conversation, LIVE_TAIL_TOLERANCE\)/s);
  assert.match(source, /article\.dataset\.transcriptId = String\(message\.id \|\| ""\)/);
  assert.match(source, /restoreTranscriptReadingAnchor\(conversation, readingAnchor, readingOffset\)/);
  assert.match(source, /details\.dataset\.transcriptMembers = JSON\.stringify/);
  assert.match(source, /node\.dataset\.transcriptId === anchor\.memberId/);
  assert.match(source, /transcriptAnchorMembers\(node\.dataset\.transcriptMembers\)\s*\.includes\(anchor\.memberId\)/s);
  assert.match(source, /state\.paneFollowing = false;/);
  assert.match(source, /state\.transcriptFollowing = false;/);
  assert.match(source, /paneReadingScrollTop: 0/);
  assert.match(source, /state\.paneReadingScrollTop = 0/);
  assert.match(source, /state\.paneExpectedScrollTop/);
  assert.match(source, /state\.transcriptExpectedScrollTop/);
  assert.match(source, /scrollMatchesExpectedPosition/);
  assert.match(source, /const revealRaw = raw && pane\.hidden/);
  assert.match(source, /if \(revealRaw\) \{\s*pane\.scrollTop = state\.paneFollowing/s);
  assert.doesNotMatch(source, /if \(raw\) \{\s*pane\.scrollTop = state\.paneFollowing/s);
  assert.match(source, /pane\.scrollTop = state\.paneFollowing\s*\? pane\.scrollHeight\s*: state\.paneReadingScrollTop;/s);
  assert.match(source, /const readingOffset = paneVisible \? pane\.scrollTop : state\.paneReadingScrollTop/);
  assert.match(source, /if \(pane\.hidden\) return;\s*state\.paneReadingScrollTop = pane\.scrollTop;/s);
  assert.match(css, /#pane \{[^}]*height: 100%;[^}]*min-height: 0;[^}]*max-height: 100%;[^}]*overflow: auto;[^}]*overscroll-behavior: contain;[^}]*overflow-anchor: none;/s);
  assert.match(css, /\.conversation \{[^}]*overscroll-behavior: contain;/s);
});

test("tool-group anchor membership is bounded and malformed hints fail closed", () => {
  assert.deepEqual(transcriptAnchorMembers('["exec-1","exec-2"]'), ["exec-1", "exec-2"]);
  for (const invalid of [
    null,
    "",
    "not-json",
    '{}',
    '[]',
    '[1]',
    '[""]',
    JSON.stringify(Array.from({ length: 25 }, (_, index) => `exec-${index}`)),
    JSON.stringify(["x".repeat(513)]),
    "[" + " ".repeat(128 * 1024) + "]",
  ]) assert.deepEqual(transcriptAnchorMembers(invalid), []);
});

test("conversation visibility defaults to all and independently filters human and internal records", () => {
  const messages = [
    { id: "agent", role: "assistant", markdown: "Agent prose" },
    { id: "human", role: "user", markdown: "Human prompt" },
    { id: "tool", role: "tool", kind: "tool", tool_name: "exec" },
    { id: "system", role: "system", kind: "system", markdown: "System status" },
    { id: "status", role: "assistant", kind: "status", markdown: "Coordination status" },
  ];
  assert.deepEqual(messages.map(transcriptVisibilityKind), [
    "agent", "human", "internal", "internal", "internal",
  ]);
  assert.deepEqual(filterTranscriptMessages(messages, {}).map(({ id }) => id), [
    "agent", "human", "tool", "system", "status",
  ]);
  assert.deepEqual(
    filterTranscriptMessages(messages, { human: false, internal: true }).map(({ id }) => id),
    ["agent", "tool", "system", "status"],
  );
  assert.deepEqual(
    filterTranscriptMessages(messages, { human: true, internal: false }).map(({ id }) => id),
    ["agent", "human"],
  );
  assert.deepEqual(
    filterTranscriptMessages(messages, { human: false, internal: false }).map(({ id }) => id),
    ["agent"],
  );
  assert.equal(transcriptItemIsVisible(messages[0], { human: false, internal: false }), true);
});

test("conversation visibility preferences persist safely and reject malformed storage", () => {
  assert.deepEqual(conversationVisibilityPreferences(null), { human: true, internal: true });
  assert.deepEqual(conversationVisibilityPreferences("not-json"), { human: true, internal: true });
  assert.deepEqual(conversationVisibilityPreferences("[]"), { human: true, internal: true });
  assert.deepEqual(conversationVisibilityPreferences('{"human":false,"internal":true,"agent":false}'), {
    human: false,
    internal: true,
  });
  assert.deepEqual(conversationVisibilityPreferences({ human: "false", internal: false }), {
    human: true,
    internal: false,
  });
  assert.equal(
    conversationVisibilityPreferenceJson({ human: false, internal: false, agent: false }),
    '{"human":false,"internal":false}',
  );
  assert.deepEqual(loadConversationVisibilityPreferences(() => {
    throw new Error("storage denied");
  }), { human: true, internal: true });
  let stored = null;
  assert.equal(saveConversationVisibilityPreferences((value) => { stored = value; }, {
    human: false, internal: true,
  }), true);
  assert.equal(stored, '{"human":false,"internal":true}');
  assert.equal(saveConversationVisibilityPreferences(() => {
    throw new Error("quota exceeded");
  }, { human: false, internal: false }), false);
  assert.equal(saveConversationVisibilityPreferences(() => false, {
    human: false, internal: false,
  }), false);
});

test("browser storage access is centralized behind fail-open initialization helpers", () => {
  const source = readFileSync(new URL("./app.js", import.meta.url), "utf8");
  const calls = [...source.matchAll(/localStorage\.(getItem|setItem|removeItem|clear)\(/g)]
    .map((match) => match[1]);
  assert.deepEqual(calls, ["getItem", "setItem"]);
  assert.match(source, /const readLocalStorage = \(key\) => \{\s*try \{ return localStorage\.getItem\(key\); \} catch \{ return null; \}\s*\};/s);
  assert.match(source, /const writeLocalStorage = \(key, value\) => \{\s*try \{\s*localStorage\.setItem\(key, value\);\s*return true;\s*\} catch \{\s*return false;\s*\}\s*\};/s);
  assert.match(source, /setRailCollapsed\(state\.railCollapsed\)/);
  assert.match(source, /writeLocalStorage\("atmux\.rail-collapsed"/);
});

test("conversation filtering happens before exec grouping and never counts hidden calls", () => {
  const exec = (id) => ({
    id, role: "tool", kind: "tool", tool_name: "exec", tool_output: "ok",
  });
  const messages = [
    exec("exec-1"),
    { id: "human", role: "user", markdown: "a human boundary" },
    exec("exec-2"),
    exec("exec-3"),
    { id: "agent", role: "assistant", markdown: "agent boundary" },
    exec("exec-4"),
  ];
  const withoutHuman = compactTranscriptItems(filterTranscriptMessages(messages, {
    human: false, internal: true,
  }));
  assert.deepEqual(withoutHuman.map((item) => item.kind), ["tool-group", "item", "item"]);
  assert.equal(toolGroupSummary(withoutHuman[0]), "exec ×3");
  assert.deepEqual(withoutHuman[0].messages.map(({ id }) => id), ["exec-1", "exec-2", "exec-3"]);
  const agentOnly = compactTranscriptItems(filterTranscriptMessages(messages, {
    human: false, internal: false,
  }));
  assert.deepEqual(agentOnly.map((item) => item.message.id), ["agent"]);
});

test("conversation filter controls are recoverable, accessible, and text-only", () => {
  const source = readFileSync(new URL("./app.js", import.meta.url), "utf8");
  const html = readFileSync(new URL("./index.html", import.meta.url), "utf8");
  const css = readFileSync(new URL("./app.css", import.meta.url), "utf8");
  assert.match(html, /id="conversation-filters-open"[^>]*aria-haspopup="dialog"[^>]*aria-controls="conversation-filters-dialog"/);
  assert.match(html, /id="conversation-filters-dialog"[^>]*aria-labelledby="conversation-filters-title"[^>]*aria-describedby="conversation-filters-note"/);
  assert.match(html, /<input type="checkbox" checked disabled>\s*<span>Agent messages<\/span>/);
  assert.match(html, /id="conversation-show-human" type="checkbox" checked/);
  assert.match(html, /id="conversation-show-internal" type="checkbox" checked/);
  assert.match(html, /id="conversation-filters-reset"[^>]*>Show all<\/button>/);
  assert.match(source, /indicator\.textContent = hiddenCount \? `\$\{hiddenCount\} off` : "All"/);
  assert.match(source, /drawConversation\(true\)/);
  assert.match(source, /state\.pendingTranscriptFilterChange \|\|= filterChanged/);
  assert.match(source, /drawConversation\(state\.pendingTranscriptFilterChange\)/);
  assert.match(source, /filterTranscriptMessages\(sourceMessages, state\.conversationVisibility\)/);
  assert.match(source, /No agent messages to show\. Change Conversation visibility or choose Show all\./);
  assert.doesNotMatch(source.slice(
    source.indexOf("function renderConversationFilters"),
    source.indexOf("function renderViewMode"),
  ), /innerHTML/);
  assert.match(css, /@media \(max-width: 720px\)[\s\S]*\.conversation-filters-open \{[^}]*min-height: 44px;/s);
  assert.match(css, /@media \(max-width: 720px\)[\s\S]*\.conversation-filter-options input \{[^}]*width: 22px;[^}]*height: 22px;/s);
});

test("adjacent low-signal coordination calls collapse without crossing prose or error boundaries", () => {
  const tool = (id, toolName, toolOutput = null) => ({
    id, kind: "tool", role: "tool", tool_name: toolName, tool_input: `{ "id": "${id}" }`, tool_output: toolOutput,
  });
  const messages = [
    { id: "human", role: "user", markdown: "Keep this visible" },
    tool("wait-1", "collaboration.wait_agent", "timed out"),
    tool("send-1", "send_message", "delivered"),
    tool("follow-1", "functions.collaboration.followup_task", '{"status":"completed"}'),
    { id: "agent", role: "assistant", markdown: "Agent prose stays visible" },
    tool("error", "wait_agent", "Error: failed to contact agent"),
    tool("wait-2", "wait_agent"),
    tool("list-1", "list_agents", '[{"path":"/root/a","status":"waiting"}]'),
    tool("exec", "exec_command", "ok"),
    tool("wait-single", "wait_agent"),
  ];
  const compacted = compactTranscriptItems(messages);
  assert.deepEqual(compacted.map((item) => item.kind), [
    "item", "tool-group", "item", "item", "tool-group", "item", "item",
  ]);
  assert.deepEqual(compacted[1].messages.map((item) => item.id), ["wait-1", "send-1", "follow-1"]);
  assert.deepEqual(compacted[4].messages.map((item) => item.id), ["wait-2", "list-1"]);
  assert.equal(compacted[2].message.id, "agent");
  assert.equal(compacted[3].message.id, "error");
  assert.equal(compacted[5].message.id, "exec");
  assert.equal(compacted[6].message.id, "wait-single");
});

test("adjacent exec variants collapse as exec ×4 without crossing narrative or failures", () => {
  const exec = (id, name, output) => ({
    id, kind: "tool", role: "tool", tool_name: name,
    tool_input: `{ "cmd": "printf ${id}" }`, tool_output: output,
  });
  const messages = [
    { id: "human", role: "user", markdown: "Human plan stays visible" },
    exec("exec-1", "functions.exec", '{"exit_code":0,"output":"command one output"}'),
    exec("exec-2", "exec_command", "Process exited with code 0"),
    exec("exec-3", "tools/exec", "ok"),
    exec("exec-4", "functions.exec_command", '{"result":{"exit_code":0}}'),
    { id: "agent", role: "assistant", markdown: "Agent interpretation stays visible" },
    exec("exec-error", "exec", "Error: command exited with status 1"),
  ];
  const compacted = compactTranscriptItems(messages);
  assert.deepEqual(compacted.map((item) => item.kind), ["item", "tool-group", "item", "item"]);
  assert.deepEqual(compacted[1].messages.map((item) => item.id), ["exec-1", "exec-2", "exec-3", "exec-4"]);
  assert.deepEqual(compacted[1].counts, [{ name: "exec", count: 4 }]);
  assert.equal(toolGroupSummary(compacted[1]), "exec ×4");
  assert.equal(compacted[0].message.markdown, "Human plan stays visible");
  assert.equal(compacted[2].message.markdown, "Agent interpretation stays visible");
  assert.equal(compacted[3].message.id, "exec-error");
});

test("exec failures and unknown output fail open while safe statuses stay compatible", () => {
  const tool = (id, name, output) => ({
    id, kind: "tool", role: "tool", tool_name: name, tool_output: output,
  });
  const messages = [
    tool("timeout", "exec", "timed out"),
    tool("ok-after-timeout", "exec", "ok"),
    tool("json-error", "exec_command", '{"exit_code":1}'),
    tool("json-ok", "exec_command", '{"exit_code":0}'),
    tool("process-error", "functions.exec", "Process exited with code 1"),
    tool("tool-failure", "functions.exec", "tool-call failure"),
    tool("unknown-1", "exec", "command printed useful output"),
    tool("unknown-2", "exec", "another useful result"),
    tool("pending-1", "exec", "running"),
    tool("pending-2", "exec_command", '{"status":"running"}'),
  ];
  assert.deepEqual(messages.map(execResultClass), [
    "error", "success", "error", "success", "error", "error", null, null,
    "pending", "pending",
  ]);
  assert.equal(toolResultSignal(messages[0]), "error");
  assert.equal(toolResultSignal(messages[2]), "error");
  assert.equal(toolResultSignal(messages[4]), "error");
  assert.equal(execResultClass(tool("generic-zero", "exec", '{"code":0}')), null);
  assert.equal(execResultClass(tool("explicit-ok", "exec", '{"ok":true}')), "success");
  assert.deepEqual(messages.map(internalToolGroupKey), [
    null, "repeat:exec:success", null, "repeat:exec:success", null, null, null, null,
    "repeat:exec:pending", "repeat:exec:pending",
  ]);
  const compacted = compactTranscriptItems(messages);
  assert.deepEqual(compacted.map((item) => item.kind), [
    "item", "item", "item", "item", "item", "item", "item", "item", "tool-group",
  ]);
  assert.equal(toolGroupSummary(compacted.at(-1)), "exec ×2");
});

test("meaningful non-exec tools never collapse even when repeated", () => {
  const calls = [
    ["patch", "apply_patch"],
    ["web", "web.run"],
    ["plan", "update_plan"],
  ].flatMap(([prefix, name]) => [1, 2].map((index) => ({
    id: `${prefix}-${index}`, kind: "tool", role: "tool", tool_name: name,
    tool_output: index === 1 ? "ok" : "completed",
  })));
  assert.ok(calls.every((item) => internalToolGroupKey(item) === null));
  assert.deepEqual(
    compactTranscriptItems(calls).map((item) => item.message.id),
    calls.map((item) => item.id),
  );
});

test("meaningful results, approvals, and lifecycle tools always remain visible", () => {
  const tool = (id, name, output) => ({ id, kind: "tool", role: "tool", tool_name: name, tool_output: output });
  const protectedItems = [
    tool("meaningful", "wait_agent", "Agent completed the migration and verified the deployment."),
    tool("approval", "wait_agent", "Approval required before continuing"),
    tool("spawn", "spawn_agent", "queued"),
    tool("interrupt", "interrupt_agent", "ok"),
  ];
  assert.deepEqual(protectedItems.map(collapsibleCoordinationTool), [false, false, false, false]);
  assert.deepEqual(protectedItems.map((item) => coordinationResultSignal(item)), [
    "meaningful", "approval", "status", "status",
  ]);
  assert.ok(compactTranscriptItems(protectedItems).every((item) => item.kind === "item"));
});

test("coordination status JSON fails closed for unknown and negative states", () => {
  const tool = (id, output) => ({
    id, kind: "tool", role: "tool", tool_name: "wait_agent", tool_output: output,
  });
  const benign = [
    tool("completed", '{"status":"completed"}'),
    tool("agents", '{"agents":[{"path":"/root/a","status":"waiting"}]}'),
  ];
  assert.deepEqual(benign.map(coordinationResultSignal), ["status", "status"]);
  assert.equal(compactTranscriptItems(benign)[0].kind, "tool-group");

  const unsafe = [
    tool("cancelled", '{"status":"cancelled"}'),
    tool("forbidden", '{"state":"forbidden"}'),
    tool("unavailable", '{"status":"unavailable"}'),
    tool("not-found", '{"state":"not_found"}'),
    tool("numeric", '{"status":500}'),
    tool("boolean", '{"status":false}'),
    tool("null", '{"state":null}'),
    tool("unknown", '{"status":"paused"}'),
    tool("prose", '{"status":"the agent wrote a useful answer"}'),
  ];
  assert.deepEqual(unsafe.map(coordinationResultSignal), [
    "error", "error", "error", "error", "error", "error", "error", "meaningful", "meaningful",
  ]);
  assert.ok(compactTranscriptItems(unsafe).every((item) => item.kind === "item"));
});

test("coordination groups are bounded at 24 and preserve every call in exact order", () => {
  const calls = Array.from({ length: 49 }, (_, index) => ({
    id: `wait-${index}`, kind: "tool", role: "tool", tool_name: "wait_agent",
  }));
  const groups = compactTranscriptItems(calls);
  assert.deepEqual(groups.map((group) => group.messages.length), [24, 23, 2]);
  assert.deepEqual(groups.flatMap((group) => group.messages.map((item) => item.id)), calls.map((item) => item.id));
  assert.ok(groups.every((group) => group.messages.length >= 2 && group.messages.length <= 24));
});

test("tool summaries normalize namespaces and expose per-tool counts", () => {
  const messages = [
    { id: "a", kind: "tool", tool_name: "functions.collaboration.wait_agent" },
    { id: "b", kind: "tool", tool_name: "mcp__send_message", tool_output: "sent" },
    { id: "c", kind: "tool", tool_name: "wait_agent", tool_output: "ok" },
  ];
  const [group] = compactTranscriptItems(messages);
  assert.equal(normalizedToolName(messages[0]), "wait_agent");
  assert.equal(normalizedToolName(messages[1]), "send_message");
  assert.equal(toolGroupSummary(group), "3 internal calls · wait_agent ×2 · send_message ×1");
});

test("coordination compaction stays inside Conversation and uses text-only DOM rendering", () => {
  const source = readFileSync(new URL("./app.js", import.meta.url), "utf8");
  const renderer = source.slice(source.indexOf("function renderToolCard"), source.indexOf("function flushPendingTranscriptRender"));
  assert.match(renderer, /compactTranscriptItems\(/);
  assert.match(renderer, /summary\.textContent = toolGroupSummary\(group\)/);
  assert.match(renderer, /\$\{summary\.textContent\}; \$\{group\.messages\.length\} calls and results/);
  assert.match(renderer, /pre\.textContent = value/);
  assert.match(renderer, /state\.transcriptRequest === transcriptGeneration/);
  assert.match(renderer, /details\.isConnected/);
  assert.match(renderer, /details\.closest\("#conversation"\) === conversation/);
  assert.doesNotMatch(renderer, /innerHTML/);
  assert.doesNotMatch(source.slice(source.indexOf("function drawPane"), source.indexOf("function drawConversation")), /compactTranscriptItems\(/);
});

test("applyPanePatch returns a new line array for a matching revision", () => {
  const original = ["one", "old", "three"];
  const result = applyPanePatch(original, 4, {
    base_revision: 4,
    revision: 5,
    start_line: 1,
    delete_lines: 1,
    lines: ["new", "two"],
  });
  assert.equal(result.applied, true);
  assert.deepEqual(result.lines, ["one", "new", "two", "three"]);
  assert.deepEqual(original, ["one", "old", "three"]);
  assert.equal(result.revision, 5);
});

test("applyPanePatch rejects stale revisions and invalid ranges", () => {
  const lines = ["one"];
  assert.equal(applyPanePatch(lines, 3, {
    base_revision: 2, revision: 4, start_line: 0, delete_lines: 1, lines: [],
  }).applied, false);
  assert.equal(applyPanePatch(lines, 3, {
    base_revision: 3, revision: 4, start_line: 2, delete_lines: 0, lines: [],
  }).applied, false);
});

test("utf8ByteLength enforces the prompt cap in bytes rather than characters", () => {
  assert.equal(utf8ByteLength("hello"), 5);
  assert.equal(utf8ByteLength("🙂"), 4);
  assert.equal(utf8ByteLength("a".repeat(MAX_MESSAGE_BYTES)), MAX_MESSAGE_BYTES);
  assert.equal(messageFitsByteLimit("a".repeat(MAX_MESSAGE_BYTES)), true);
  assert.equal(messageFitsByteLimit("🙂".repeat(MAX_MESSAGE_BYTES / 4)), true);
  assert.equal(messageFitsByteLimit("🙂".repeat(MAX_MESSAGE_BYTES / 4 + 1)), false);
});

test("image selection enforces portable formats, counts, and byte limits", () => {
  const png = { name: "screen.png", type: "image/png", size: 1024 };
  const jpeg = { name: "photo.jpg", type: "image/jpeg", size: 2048 };
  assert.deepEqual(validateImageSelection([png, jpeg]).files, [png, jpeg]);
  assert.match(validateImageSelection([{ type: "image/webp", size: 20 }]).error, /PNG or JPEG/);
  assert.match(validateImageSelection([{ type: "image/png", size: MAX_IMAGE_BYTES + 1 }]).error, /4 MiB/);
  assert.match(validateImageSelection(
    Array.from({ length: MAX_IMAGE_ATTACHMENTS + 1 }, () => png),
  ).error, /at most 4/);
  assert.match(validateImageSelection([
    { type: "image/png", size: MAX_IMAGE_BYTES },
  ], [
    { file: { type: "image/jpeg", size: MAX_TOTAL_IMAGE_BYTES - MAX_IMAGE_BYTES + 1 } },
  ]).error, /12 MiB/);
});

test("image attachments retain their captured pane and encode bytes without corruption", () => {
  assert.equal(attachmentDeliveryTarget("midnight~%7", "max~%2"), "midnight~%7");
  assert.equal(attachmentDeliveryTarget(null, "max~%2"), "max~%2");
  assert.equal(attachmentDeliveryTarget(null, null), null);
  assert.equal(arrayBufferToBase64(Uint8Array.from([0, 1, 2, 253, 254, 255]).buffer), "AAEC/f7/");
});

test("a completed image send removes only its immutable attachment snapshot", () => {
  const first = { file: { name: "first.png" } };
  const addedLater = { file: { name: "later.png" } };
  assert.deepEqual(remainingAttachmentsAfterDelivery([first, addedLater], [first]), [addedLater]);
  assert.deepEqual(remainingAttachmentsAfterDelivery([addedLater], [first]), [addedLater]);
});

test("clipboard images work through both files and ClipboardItem APIs", () => {
  const png = { type: "image/png", size: 10 };
  assert.deepEqual(imageFilesFromTransfer({ files: [png] }), [png]);
  assert.deepEqual(imageFilesFromTransfer({
    files: [],
    items: [{ kind: "file", type: "image/jpeg", getAsFile: () => png }],
  }), [png]);
  assert.deepEqual(imageFilesFromTransfer({
    items: [{ kind: "string", type: "text/plain", getAsFile: () => png }],
  }), []);
});

test("composer exposes paste/drop/file image attachments with compact removable previews", () => {
  const source = readFileSync(new URL("./app.js", import.meta.url), "utf8");
  const markup = readFileSync(new URL("./index.html", import.meta.url), "utf8");
  const css = readFileSync(new URL("./app.css", import.meta.url), "utf8");
  assert.match(markup, /id="image-input"[^>]+accept="image\/png,image\/jpeg"[^>]+multiple/);
  assert.match(markup, /id="attachment-tray"/);
  assert.match(source, /addEventListener\("paste"/);
  assert.match(source, /addEventListener\("drop"/);
  assert.match(source, /attachmentDeliveryTarget\(state\.attachmentPaneId, paneId\)/);
  assert.match(source, /const attachments = messageOverride === null \? \[\.\.\.state\.attachments\] : \[\]/);
  assert.match(source, /if \(state\.composerSending\) \{\s*toast\("Wait for the current message to finish sending"\)/s);
  assert.match(source, /removeDeliveredAttachments\(attachments\)/);
  assert.match(source, /\/image-messages/);
  assert.match(css, /\.attachment-preview/);
  assert.match(css, /\.attachment-remove/);
});

test("browser branding uses the Midnight atmux logo asset", () => {
  const markup = readFileSync(new URL("./index.html", import.meta.url), "utf8");
  const css = readFileSync(new URL("./app.css", import.meta.url), "utf8");
  assert.match(markup, /rel="icon"[^>]+href="\/atmux-logo\.jpg"/);
  assert.match(markup, /class="brand-logo"[^>]+src="\/atmux-logo\.jpg"/);
  assert.match(css, /\.brand-logo/);
  assert.match(markup, /class="brand-wordmark">atmux<\/span>/);
  assert.match(css, /@media \(max-width: 720px\)[\s\S]*\.brand-wordmark, \.brand small, \.counts \{ display: none; \}/s);
});

test("conversation rows stay full-width and left-anchored when the rail changes", () => {
  const css = readFileSync(new URL("./app.css", import.meta.url), "utf8");
  assert.match(css, /\.message-card \{[^}]*width: 100%;[^}]*max-width: none;[^}]*margin: 0 0 8px;/s);
  assert.match(css, /\.message-card\.user \{[^}]*margin-inline: 0;/s);
  assert.match(css, /\.tool-card \{[^}]*width: 100%;[^}]*max-width: none;[^}]*margin: 0 0 6px;/s);
  assert.match(css, /\.conversation \{[^}]*scrollbar-gutter: stable;[^}]*overflow-anchor: none;/s);
  assert.doesNotMatch(css, /calc\(\(100% - 980px\)/);
  assert.doesNotMatch(css, /\.message-card\.user \{[^}]*max-width: 94%/s);
});

test("mobile controls stay compact and horizontally reachable so the pane gets the viewport", () => {
  const css = readFileSync(new URL("./app.css", import.meta.url), "utf8");
  const html = readFileSync(new URL("./index.html", import.meta.url), "utf8");
  const source = readFileSync(new URL("./app.js", import.meta.url), "utf8");
  assert.match(css, /\.composer-actions \{[^}]*min-width: 0;[^}]*flex-wrap: wrap;/s);
  assert.match(css, /body \{[^}]*height: var\(--app-height, 100dvh\);/s);
  assert.match(css, /@media \(max-width: 720px\)[\s\S]*\.agent-view \{[^}]*grid-template-rows: auto minmax\(96px, 1fr\) auto;/s);
  assert.match(css, /@media \(max-width: 720px\)[\s\S]*\.agent-head \{[^}]*display: grid;[^}]*grid-template-columns: 30px minmax\(0, 1fr\) auto;/s);
  assert.match(css, /@media \(max-width: 720px\)[\s\S]*\.agent-head p, \.agent-meta, \.launch-command, \.model-control, \.actions \{[^}]*display: none !important;/s);
  assert.match(css, /@media \(max-width: 720px\)[\s\S]*\.quick-actions-open \{[^}]*display: inline-flex;/s);
  assert.match(css, /\.agent-head \.model-control, \.agent-head \.actions \{[^}]*display: none !important;/s);
  assert.match(css, /@media \(max-width: 720px\)[\s\S]*\.composer-actions \{[^}]*width: 100%;[^}]*flex-wrap: nowrap;[^}]*overflow-x: auto;/s);
  assert.match(css, /\.composer textarea \{[^}]*font-size: 16px;[^}]*resize: none;/s);
  assert.match(css, /html \{ overflow: hidden; overscroll-behavior: none; \}/);
  assert.match(css, /@media \(max-width: 720px\)[\s\S]*body \{[^}]*position: fixed;[^}]*height: var\(--app-height, 100dvh\);/s);
  assert.match(css, /@media \(max-width: 720px\)[\s\S]*body\.has-selection \.topbar \{ display: none; \}/s);
  assert.doesNotMatch(css, /composer-focused/);
  assert.match(html, /content="width=device-width, initial-scale=1, viewport-fit=cover, interactive-widget=resizes-content"/);
  assert.match(html, /id="quick-actions-dialog"/);
  assert.match(html, /id="quick-agent-model"/);
  assert.match(html, /id="quick-compact"/);
  assert.doesNotMatch(html, /id="compact"/);
  assert.doesNotMatch(source, /window\.scrollTo\(/);
  assert.doesNotMatch(source, /pinMobileDocument/);
  assert.doesNotMatch(source, /classList\.toggle\("composer-focused"/);
  assert.doesNotMatch(source, /messageInput\.addEventListener\("focus", syncMobileViewport/);
});

test("the mobile app box tracks the layout viewport so the iOS keyboard cannot strand it", () => {
  const source = readFileSync(new URL("./app.js", import.meta.url), "utf8");
  const css = readFileSync(new URL("./app.css", import.meta.url), "utf8");
  // These rules are about what the code does, so the prose explaining why is
  // stripped before matching.
  const code = source.replace(/^[ \t]*\/\/.*$/gm, "");
  const sync = code.match(/function syncMobileViewport\(\) \{[\s\S]*?\n  \}/);
  assert.ok(sync, "syncMobileViewport must exist");

  // `body` is `position: fixed` on mobile, so it is laid out against the layout
  // viewport. iOS shrinks only the visual viewport for the keyboard and offsets
  // it by `visualViewport.offsetTop` to reveal the focused field, so sizing the
  // app box from `visualViewport.height` measured one viewport while WebKit
  // scrolled the other: the composer ended up pinned to the top of the screen
  // above dead space, with the transcript pushed off screen.
  assert.match(sync[0], /const height = window\.innerHeight;/);
  assert.doesNotMatch(sync[0], /visualViewport/);

  // Reacting to visual viewport events would reintroduce the keyboard height,
  // and writing styles mid keyboard animation is what the reverted transform
  // attempt did.
  assert.doesNotMatch(code, /visualViewport\?\.addEventListener/);
  assert.doesNotMatch(code, /visualViewport\.offsetTop/);
  assert.doesNotMatch(code, /style\.transform/);
  assert.doesNotMatch(css, /translateY\(/);

  // The app box must still be driven by a real measurement, not left to `dvh`,
  // because Android Chrome resizes the layout viewport for its keyboard.
  assert.match(sync[0], /setProperty\("--app-height", `\$\{Math\.floor\(height\)\}px`\)/);
  assert.match(sync[0], /removeProperty\("--app-height"\)/);
  assert.match(code, /window\.addEventListener\("resize", syncMobileViewport/);
  assert.match(code, /window\.addEventListener\("orientationchange", syncMobileViewport/);

  // WebKit reveals only the focused node, so the row of send/attach controls
  // below the textarea asks to be cleared as well.
  assert.match(css, /@media \(max-width: 720px\)[\s\S]*\.composer textarea \{[^}]*scroll-margin-bottom: 52px;/s);
});

/// Where the app lands on screen once the software keyboard is up.
///
/// `body` is `position: fixed`, so the app box always starts at the top of the
/// layout viewport regardless of the height script gives it, while the band the
/// user can actually see is the visual viewport: `[offsetTop, offsetTop +
/// visualHeight]`. Everything the bug is about falls out of comparing those two.
function keyboardGeometry({ visualHeight, offsetTop, appHeight, composerHeight }) {
  const bandTop = offsetTop;
  const bandBottom = offsetTop + visualHeight;
  const composerTop = appHeight - composerHeight;
  return {
    transcriptVisible: Math.max(0, Math.min(composerTop, bandBottom) - bandTop),
    composerFullyVisible: composerTop >= bandTop && appHeight <= bandBottom,
    deadSpace: Math.max(0, bandBottom - Math.max(appHeight, bandTop)),
  };
}

test("sizing the app to the visual viewport strands it above the iOS keyboard", () => {
  const LAYOUT = 844;      // iPhone layout viewport; iOS never shrinks this.
  const KEYBOARD = 336;
  const COMPOSER = 122;
  const visualHeight = LAYOUT - KEYBOARD;

  // WebKit picks its reveal offset from where the composer sits *before* script
  // reacts to the keyboard, so it scrolls against the full-height layout.
  const offsetTop = LAYOUT - visualHeight;
  assert.equal(offsetTop, KEYBOARD);

  // The reverted behaviour: --app-height = visualViewport.height. The app box
  // shrinks against the layout viewport's top edge while the visible band has
  // already moved down, so the two barely overlap.
  const shrunk = keyboardGeometry({
    visualHeight, offsetTop, appHeight: visualHeight, composerHeight: COMPOSER,
  });
  assert.equal(shrunk.deadSpace, 336);       // a blank strip as tall as the keyboard
  assert.equal(shrunk.transcriptVisible, 50); // nothing left to read

  // Measuring the layout viewport instead keeps the app box and WebKit's reveal
  // scroll in one coordinate space: the composer lands flush against the
  // keyboard and the rest of the band is transcript.
  const layout = keyboardGeometry({
    visualHeight, offsetTop, appHeight: LAYOUT, composerHeight: COMPOSER,
  });
  assert.equal(layout.deadSpace, 0);
  assert.equal(layout.transcriptVisible, visualHeight - COMPOSER);
  assert.ok(layout.composerFullyVisible);
  assert.ok(layout.transcriptVisible > shrunk.transcriptVisible * 7);
});

test("the same layout-viewport measurement holds when the keyboard resizes the layout viewport", () => {
  // Android Chrome honours interactive-widget=resizes-content: the layout
  // viewport itself shrinks and the visual viewport is never offset. So
  // window.innerHeight is the right measurement on both platforms.
  const COMPOSER = 122;
  const innerHeight = 844 - 336;
  const geometry = keyboardGeometry({
    visualHeight: innerHeight, offsetTop: 0, appHeight: innerHeight, composerHeight: COMPOSER,
  });
  assert.equal(geometry.deadSpace, 0);
  assert.ok(geometry.composerFullyVisible);
  assert.equal(geometry.transcriptVisible, innerHeight - COMPOSER);
});

test("mobile Back uses an in-app agent-menu history entry", () => {
  assert.deepEqual(appRoute("https://atmux.test/?session=tron~%25100"), {
    view: "session",
    id: "tron~%100",
  });
  assert.deepEqual(appRoute("https://atmux.test/?machine=midnight"), {
    view: "machine",
    id: "midnight",
  });
  assert.deepEqual(appRoute("https://atmux.test/?view=usage"), { view: "usage", id: null });
  assert.deepEqual(appRoute("https://atmux.test/"), { view: "menu", id: null });
  const menu = agentMenuUrl("https://atmux.test/?session=tron~%25100&pulseAccount=4");
  assert.equal(menu.pathname, "/");
  assert.equal(menu.searchParams.get("session"), null);
  assert.equal(menu.searchParams.get("pulseAccount"), "4");

  const source = readFileSync(new URL("./app.js", import.meta.url), "utf8");
  assert.match(source, /history\.replaceState\(appHistoryState\(\{ view: "menu"/);
  assert.match(source, /history\.pushState\(appHistoryState\(initialRoute\)/);
  assert.match(source, /window\.addEventListener\("popstate"/);
  assert.match(source, /selectSession\(route\.id, "none"\)/);
  assert.match(source, /current\.view === "menu" && route\.view !== "menu"/);
  assert.match(source, /function backToAgentMenu\(\)[\s\S]*selectSession\(null, "replace"\)/);
  assert.doesNotMatch(source.match(/function backToAgentMenu\(\)[\s\S]*?\n  \}/)?.[0] || "", /history\.back\(\)/);
  assert.match(source, /\$\("mobile-back"\)\.addEventListener\("click", backToAgentMenu\)/);
});

test("focused live panes forward only ordinary typing to the message composer", () => {
  assert.equal(paneTypingText({ key: "a" }), "a");
  assert.equal(paneTypingText({ key: "🙂" }), "🙂");
  assert.equal(paneTypingText({ key: " ", shiftKey: true }), " ");
  assert.equal(paneTypingText({ key: "Enter" }), "");
  assert.equal(paneTypingText({ key: "a", ctrlKey: true }), "");
  assert.equal(paneTypingText({ key: "a", metaKey: true }), "");
  assert.equal(paneTypingText({ key: "a", altKey: true }), "");
  assert.equal(paneTypingText({ key: "a", isComposing: true }), "");
});

test("message history moves through sent comments and restores the draft", () => {
  const history = ["first", "second"];
  assert.equal(moveMessageHistory(history, history.length, "up"), 1);
  assert.equal(moveMessageHistory(history, 1, "up"), 0);
  assert.equal(moveMessageHistory(history, 0, "up"), 0);
  assert.equal(moveMessageHistory(history, 0, "down"), 1);
  assert.equal(moveMessageHistory(history, 1, "down"), history.length);
  assert.equal(moveMessageHistory(history, history.length, "down"), null);
  assert.equal(moveMessageHistory([], 0, "up"), null);
});

test("live pane selections hold streaming redraws only for ranges touching the pane", () => {
  const pane = { contains: (node) => node === "pane-text" };
  assert.equal(selectionTouchesPane(pane, {
    isCollapsed: false, anchorNode: "pane-text", focusNode: "outside",
  }), true);
  assert.equal(selectionTouchesPane(pane, {
    isCollapsed: false, anchorNode: "outside", focusNode: "pane-text",
  }), true);
  assert.equal(selectionTouchesPane(pane, {
    isCollapsed: false, anchorNode: "outside", focusNode: "elsewhere",
  }), false);
  assert.equal(selectionTouchesPane(pane, {
    isCollapsed: true, anchorNode: "pane-text", focusNode: "pane-text",
  }), false);
});

test("filterDirectories narrows launch projects case-insensitively as the user types", () => {
  const directories = ["/work/atmux", "/work/mercury", "/work/Atlas"];
  assert.deepEqual(filterDirectories(directories, "at"), ["/work/atmux", "/work/Atlas"]);
  assert.deepEqual(filterDirectories(directories, "MERC"), ["/work/mercury"]);
  assert.deepEqual(filterDirectories(directories, ""), directories);
  assert.deepEqual(filterDirectories(null, "atmux"), []);
});

test("launch dialog uses the Project typeahead itself instead of a separate find field", () => {
  const source = readFileSync(new URL("./app.js", import.meta.url), "utf8");
  const markup = readFileSync(new URL("./index.html", import.meta.url), "utf8");
  assert.doesNotMatch(markup, /launch-directory-filter/);
  assert.match(markup, /id="launch-directory" type="search" list="launch-directory-options"/);
  assert.match(markup, /<datalist id="launch-directory-options">/);
  assert.match(source, /launch-directory-options"\)\.replaceChildren/);
  assert.equal(isManualDirectory("/Users/ryan/work/plain"), true);
  assert.equal(isManualDirectory("~/work/plain"), true);
  assert.equal(isManualDirectory("plain/relative"), false);
  assert.match(source, /type an absolute folder within a configured project root/i);
  assert.match(markup, /id="launch-browse"[^>]*>Browse<\/button>/);
  assert.match(markup, /id="launch-browser"/);
  assert.match(source, /request\(endpoint\)/);
  assert.match(markup, /id="launch-sessions"/);
  assert.match(markup, /id="launch-session"/);
  assert.match(source, /\/api\/v1\/launch-sessions/);
  assert.match(source, /resume_session_id: duplicateFlow \? null : \(\$\("launch-session"\)\.value \|\| null\)/);
  assert.match(source, /\^saved-\[0-9a-f\]\{32\}\$/);
});

test("saved conversation launch requires an explicit, sanitized confirmation", () => {
  assert.equal(
    savedSessionPreview("  Continue\n\tthe\u0000 work  "),
    "Continue the work",
  );
  assert.equal(savedSessionPreview("\n\t"), "Previous conversation");
  assert.equal(savedSessionPreview("x".repeat(200)).length, 160);
  assert.equal(savedSessionConfirmation({
    machineId: "tron",
    machineLabel: "Tron",
    profileLabel: "Max",
    directory: "/workspace/exact folder",
    harness: "claude",
    preview: "Fix this\nnext",
  }), [
    "Resume this saved conversation?",
    "",
    "Machine: Tron (tron)",
    "Profile: Max",
    "Folder: /workspace/exact folder",
    "Agent: claude",
    "Preview: Fix this next",
  ].join("\n"));

  const source = readFileSync(new URL("./app.js", import.meta.url), "utf8");
  assert.match(source, /if \(body\.resume_session_id\) \{[\s\S]*window\.confirm\(savedSessionConfirmation/);
  assert.match(source, /if \(!confirmed\) return;[\s\S]*button\.disabled = true/);
});

test("launch folder selections are remembered per machine and safely encoded", () => {
  const parsed = rememberedLaunchDirectories(JSON.stringify({
    tron: ["/work/custom", "/work/custom", "relative", "/work/other"],
    max: ["/srv/project"],
    "bad id!": ["/secret"],
  }));
  assert.deepEqual(parsed, {
    tron: ["/work/custom", "/work/other"],
    max: ["/srv/project"],
  });
  const updated = rememberLaunchDirectory(parsed, "tron", "/work/new folder");
  assert.deepEqual(updated.tron, ["/work/new folder", "/work/custom", "/work/other"]);
  assert.deepEqual(parsed.tron, ["/work/custom", "/work/other"], "helper does not mutate stored input");
  assert.deepEqual(
    availableLaunchDirectories(
      { id: "tron", directories: ["/work/discovered", "/work/custom"] },
      updated,
    ),
    ["/work/new folder", "/work/custom", "/work/other", "/work/discovered"],
  );
  assert.equal(
    launchDirectoryBrowsePath("tron", "/work/new folder"),
    "/api/v1/launch-directories?machine=tron&path=%2Fwork%2Fnew+folder",
  );
  assert.equal(launchDirectoryBrowsePath("tron", null), "/api/v1/launch-directories?machine=tron");
  assert.equal(launchDirectoryBrowsePath("bad id!", "/work"), null);
  assert.equal(launchDirectoryBrowsePath("tron", "relative"), null);
});

test("conversation Markdown recognizes prose, tables, and expandable fenced code", () => {
  const blocks = markdownBlocks([
    "## Result",
    "",
    "| File | State |",
    "| --- | --- |",
    "| app.js | fixed |",
    "",
    "```js",
    "const answer = 42;",
    "```",
  ].join("\n"));
  assert.deepEqual(blocks.map((block) => block.type), ["heading", "table", "code"]);
  assert.equal(blocks[2].language, "js");
  assert.equal(blocks[2].text, "const answer = 42;");
});

test("conversation Markdown bounds nested blockquote parsing", () => {
  const blocks = markdownBlocks(`${"> ".repeat(32)}bounded`);
  let current = blocks[0];
  let depth = 0;
  while (current?.type === "quote") {
    depth += 1;
    current = current.children[0];
  }
  assert.equal(depth, 5);
  assert.equal(current.type, "paragraph");
});

test("an unavailable transcript clears prior messages and its validator", () => {
  const current = {
    available: true,
    source: "codex",
    messages: [{ id: "old", role: "user", markdown: "private old session" }],
    truncated: false,
    error: null,
  };
  const next = reduceTranscript(current, {
    available: false,
    source: "codex",
    content_hash: "",
    changed: false,
    truncated: false,
    messages: [],
  });
  assert.equal(next.hash, "");
  assert.equal(next.transcript.available, false);
  assert.deepEqual(next.transcript.messages, []);
});

test("an unchanged transcript preserves its prior truncation state", () => {
  const current = {
    available: true,
    source: "claude",
    messages: [{ id: "current", role: "assistant", markdown: "answer" }],
    truncated: true,
    error: null,
  };
  const next = reduceTranscript(current, {
    available: true,
    source: "claude",
    content_hash: "same",
    changed: false,
    truncated: false,
  });
  assert.equal(next.hash, "same");
  assert.equal(next.transcript.truncated, true);
  assert.deepEqual(next.transcript.messages, current.messages);
});

test("conversation items distinguish compact tool calls from chat messages", () => {
  assert.equal(transcriptItemKind({ role: "assistant", kind: "message" }), "message");
  assert.equal(transcriptItemKind({ role: "tool", kind: "tool" }), "tool");
  assert.equal(transcriptItemKind({ role: "tool" }), "tool");
  const source = readFileSync(new URL("./app.js", import.meta.url), "utf8");
  assert.match(source, /details\.className = "tool-card"/);
  assert.match(source, /pre\.textContent = value/);
  assert.doesNotMatch(source, /tool_(?:input|output).*innerHTML/);
});

test("left rail controls expose encoded per-session deletion and persistent collapse", () => {
  assert.equal(sessionDeletePath("midnight~%7"), "/api/v1/sessions/midnight~%257");
  const app = readFileSync(new URL("./app.js", import.meta.url), "utf8");
  const html = readFileSync(new URL("./index.html", import.meta.url), "utf8");
  const css = readFileSync(new URL("./app.css", import.meta.url), "utf8");
  assert.match(app, /deleteButton\.addEventListener\("click", \(\) => openKillDialog\(id\)\)/);
  assert.match(app, /state\.pendingKillId = id/);
  assert.match(app, /writeLocalStorage\("atmux\.rail-collapsed"/);
  assert.match(html, /id="rail-toggle"[^>]+aria-controls="session-rail"/);
  assert.match(css, /body\.rail-collapsed \.workspace/);
  assert.match(css, /\.session-delete/);
});

test("push-to-talk keeps the pane selected when recording began", () => {
  assert.deepEqual(dictationDelivery("midnight~%7", "please", "review this"), {
    paneId: "midnight~%7",
    message: "please review this",
  });
  assert.equal(dictationDelivery(null, "", "review this"), null);
  assert.equal(dictationDelivery("midnight~%7", "draft", "  "), null);
  assert.equal(dictationPrefix("sending now", true, "sending now"), "");
  assert.equal(dictationPrefix("new draft", true, "sending now"), "new draft");
  assert.equal(composerSubmissionMatches("midnight~%7", "midnight~%7", "review this", "review this"), true);
  assert.equal(composerSubmissionMatches("midnight~%8", "midnight~%7", "review this", "review this"), false);
  assert.equal(composerSubmissionMatches("midnight~%7", "midnight~%7", "new draft", "review this"), false);
  const clearedSubmission = {
    paneId: "midnight~%7",
    message: "review this",
    clearedRevision: 4,
  };
  assert.equal(composerSubmissionCanRestore("midnight~%7", "", 4, clearedSubmission), true);
  assert.equal(composerSubmissionCanRestore("midnight~%7", "new draft", 4, clearedSubmission), false);
  assert.equal(composerSubmissionCanRestore("midnight~%7", "", 5, clearedSubmission), false);
  assert.equal(composerSubmissionCanRestore("midnight~%8", "", 4, clearedSubmission), false);
  const source = readFileSync(new URL("./app.js", import.meta.url), "utf8");
  assert.match(source, /sendComposerMessage\(delivery\.paneId, delivery\.message, \{ clearOnAccept: true \}\)/);
  assert.match(source, /restoreComposerSubmission\(composerSubmission\)/);
});

test("push-to-talk restarts recognition while held and stops cleanly on release", () => {
  assert.equal(dictationEndAction(true, false, false), "restart");
  assert.equal(dictationEndAction(false, false, false), "finish");
  assert.equal(dictationEndAction(true, true, false), "finish");
  assert.equal(dictationEndAction(true, false, true), "finish");
  assert.equal(dictationErrorPolicy("no-speech"), "retry");
  assert.equal(dictationErrorPolicy("aborted"), "normal");
  assert.equal(dictationErrorPolicy("not-allowed"), "fail");
  assert.deepEqual([0, 1, 2, 3, 99].map(dictationRestartDelay), [250, 500, 1000, 2000, 2000]);
  const source = readFileSync(new URL("./app.js", import.meta.url), "utf8");
  const css = readFileSync(new URL("./app.css", import.meta.url), "utf8");
  assert.match(source, /recognition\.continuous = true/);
  assert.match(source, /dictationEndAction\([\s\S]*=== "restart"\)[\s\S]*scheduleRestart\(generation\)/);
  assert.match(source, /window\.addEventListener\("blur", stopTalking\)/);
  assert.match(source, /queuedComposerMessages\.push\(\{[\s\S]*options: \{ clearOnAccept, composerSubmission, fromQueue:[^}]+\}[\s\S]*resolve,[\s\S]*\}\)/);
  assert.match(source, /state\.inFlightComposerText = message;/);
  assert.match(source, /Message exceeds the 64 KiB UTF-8 limit"\);\s*if \(options\.fromQueue === true\) drainQueuedComposerMessage\(\);/s);
  assert.match(css, /#talk \{[^}]*touch-action: none;/s);
});

test("agent Markdown links allow only HTTP and HTTPS and code highlighting preserves text", () => {
  assert.equal(safeLinkUrl("javascript:alert(1)"), null);
  assert.equal(safeLinkUrl("data:text/html,bad"), null);
  assert.equal(safeLinkUrl("/api/v1/sessions"), null);
  assert.equal(safeLinkUrl("https://example.com/docs"), "https://example.com/docs");
  const source = "const value = \"<script>\"; // harmless text";
  assert.equal(highlightCode(source).map((segment) => segment.text).join(""), source);
  const tokens = inlineTokens("[unsafe](javascript:alert(1)) and **strong**");
  assert.equal(tokens[0].type, "link");
  assert.equal(tokens.at(-1).type, "strong");
});

test("Files and Git routes encode pane ids and owner-issued relative paths", () => {
  const pane = "midnight~%12/# ?";
  const path = "src/東京 #1?.ts";
  assert.equal(
    paneFilesPath(pane, path),
    `/api/v1/panes/${encodeURIComponent(pane)}/files?path=${encodeURIComponent(path)}`,
  );
  assert.equal(paneFilesPath(pane, ""), `/api/v1/panes/${encodeURIComponent(pane)}/files?path=`);
  assert.equal(paneGitPath(pane), `/api/v1/panes/${encodeURIComponent(pane)}/git`);
  assert.equal(
    paneGitPath(pane, path),
    `/api/v1/panes/${encodeURIComponent(pane)}/git?path=${encodeURIComponent(path)}`,
  );
  for (const unsafe of ["/etc/passwd", "../secret", "src/../secret", "src\\secret", "bad\0name"]) {
    assert.equal(projectRelativePath(unsafe), null);
    assert.equal(paneFilesPath(pane, unsafe), null);
    assert.equal(paneGitPath(pane, unsafe), null);
  }
});

test("project browser classifies bounded file metadata and common source languages", () => {
  assert.equal(projectEntryKind({ kind: "directory" }), "directory");
  assert.equal(projectEntryKind({ type: "dir" }), "directory");
  assert.equal(projectEntryKind({ is_dir: false }), "file");
  assert.equal(projectEntryKind({ kind: "socket" }), null);
  assert.equal(sourceLanguage("src/main.rs"), "rust");
  assert.equal(sourceLanguage("web/app.mjs"), "javascript");
  assert.equal(sourceLanguage("Dockerfile"), "dockerfile");
  assert.equal(sourceLanguage("anything", "TypeScript<script>"), "typescriptscript");
  assert.equal(diffLineKind("+safe"), "added");
  assert.equal(diffLineKind("--- a/file"), "meta");
  assert.equal(diffLineKind("@@ -1 +1 @@"), "hunk");
  assert.equal(highlightCode("const value = '<img onerror=boom>';").map((part) => part.text).join(""), "const value = '<img onerror=boom>';" );
  assert.deepEqual(projectFilePreview({
    binary: true, content: "must not render", language: "text", size: 12, truncated: false,
  }, "image.bin"), {
    path: "image.bin", content: null, binary: true, size: 12, language: "text",
    contentHash: null, lineCount: null, truncated: false,
  });
});

test("file editing requires a complete UTF-8 preview with an owner revision hash", () => {
  const hash = "a".repeat(64);
  assert.equal(validContentHash(hash), true);
  assert.equal(validContentHash("A".repeat(64)), false);
  assert.equal(validContentHash("a".repeat(63)), false);
  const editable = projectFilePreview({
    kind: "file", content: "one\ntwo", content_hash: hash, line_count: 2,
    binary: false, truncated: false, size: 7,
  }, "src/main.rs");
  assert.equal(fileCanEdit(editable), true);
  assert.equal(editable.contentHash, hash);
  assert.equal(editable.lineCount, 2);
  assert.equal(fileCanEdit({ ...editable, truncated: true }), false);
  assert.equal(fileCanEdit({ ...editable, contentHash: null }), false);
  assert.equal(fileEditHasUnsavedWork({ editing: true, file: editable, editDraft: editable.content }), false);
  assert.equal(fileEditHasUnsavedWork({ editing: true, file: editable, editDraft: `${editable.content}!` }), true);
  assert.equal(fileEditHasUnsavedWork({ editing: true, file: editable, editDraft: editable.content, conflict: true }), true);
});

test("a delayed save advances the base hash without erasing newer typing", () => {
  const saved = { path: "src/main.rs", content: "sent", contentHash: "b".repeat(64) };
  assert.deepEqual(reconcileSavedFileDraft("sent", "sent", saved), {
    file: saved, editDraft: "sent", editing: false,
  });
  assert.deepEqual(reconcileSavedFileDraft("sent", "sent\nnewer", saved), {
    file: saved, editDraft: "sent\nnewer", editing: true,
  });
});

test("line-range references are bounded, labelled, fenced, and preserve the whole composer draft", () => {
  let selection = nextFileLineSelection(null, 2);
  assert.deepEqual(selection, { anchor: 2, start: 2, end: 2 });
  selection = nextFileLineSelection(selection, 4);
  assert.deepEqual(selection, { anchor: 2, start: 2, end: 4 });
  selection = nextFileLineSelection(selection, 7);
  assert.deepEqual(selection, { anchor: 7, start: 7, end: 7 });
  selection = nextFileLineSelection(selection, 3, true);
  assert.deepEqual(selection, { anchor: 7, start: 3, end: 7 });

  const block = fileReferenceBlock("src/app.js", "javascript", "zero\none\ntwo\nthree", { start: 2, end: 3 });
  assert.equal(block, "Selected `src/app.js:2-3`:\n\n```javascript\none\ntwo\n```");
  const inserted = insertComposerReference("before after", 6, block);
  assert.equal(inserted.value, `before\n\n${block}\n\n after`);
  assert.ok(inserted.value.includes("before"));
  assert.ok(inserted.value.includes(" after"));

  const oversized = Array.from({ length: MAX_FILE_REFERENCE_LINES + 50 }, (_, index) => `${index} ${"x".repeat(MAX_FILE_REFERENCE_CHARS)}`).join("\n");
  const bounded = fileReferenceBlock("huge.txt", "text", oversized, { start: 1, end: MAX_FILE_REFERENCE_LINES + 50 });
  assert.match(bounded, /huge\.txt:1-200 of 250/);
  assert.match(bounded, /selection truncated by atmux/);
  assert.ok(bounded.length < MAX_FILE_REFERENCE_CHARS + 200);
});

test("file reader preferences default by viewport and persist only bounded choices", () => {
  assert.deepEqual(fileReaderPreferences(null, true), { wrap: true, size: "small" });
  assert.deepEqual(fileReaderPreferences(null, false), { wrap: false, size: "medium" });
  assert.deepEqual(fileReaderPreferences('{"wrap":false,"size":"large"}', true), {
    wrap: false,
    size: "large",
  });
  assert.deepEqual(fileReaderPreferences({ wrap: true, size: "huge" }, false), {
    wrap: true,
    size: "medium",
  });
  assert.deepEqual(fileReaderPreferences("not json", true), { wrap: true, size: "small" });
  assert.equal(
    fileReaderPreferenceJson({ wrap: true, size: "small", ignored: "value" }),
    '{"wrap":true,"size":"small"}',
  );
  assert.deepEqual(
    loadFileReaderPreferences(() => { throw new DOMException("denied", "SecurityError"); }, true),
    { wrap: true, size: "small" },
  );
  assert.deepEqual(
    loadFileReaderPreferences(() => '{"wrap":true,"size":"large"}', false),
    { wrap: true, size: "large" },
  );
});

test("Files and Git are accessible lazy tabs with text-only source rendering", () => {
  const html = readFileSync(new URL("./index.html", import.meta.url), "utf8");
  const source = readFileSync(new URL("./app.js", import.meta.url), "utf8");
  const css = readFileSync(new URL("./app.css", import.meta.url), "utf8");
  for (const [id, controls] of [["conversation-view", "conversation"], ["raw-view", "pane"], ["files-view", "files-panel"], ["git-view", "git-panel"]]) {
    assert.match(html, new RegExp(`id="${id}"[^>]+role="tab"[^>]+aria-controls="${controls}"`));
  }
  assert.match(source, /if \(revealFiles\)[\s\S]+loadFilesDirectory/);
  assert.match(source, /if \(revealGit\)[\s\S]+loadGitSummary/);
  assert.match(source, /state\.filesController\?\.abort\(\)/);
  assert.match(source, /generation !== state\.filesRequest \|\| state\.selected !== paneId/);
  assert.match(source, /generation !== state\.gitRequest \|\| state\.selected !== paneId/);
  assert.match(source, /state\.projectView = null/);
  assert.match(source, /function resetProjectView\(\)[\s\S]+filesPanel\.hidden = true;[\s\S]+gitPanel\.hidden = true;/);
  assert.match(source, /function selectMachine[\s\S]+resetProjectView\(\)/);
  assert.match(source, /function selectPulse[\s\S]+resetProjectView\(\)/);
  assert.match(source, /state\.projectView\.files\.loading = false/);
  assert.match(source, /state\.projectView\.git\.diffLoading = false/);
  assert.match(source, /available: data\?\.available === true/);
  assert.match(source, /method: "PUT"[\s\S]+expected_hash: snapshot\.expectedHash/s);
  assert.match(source, /error\?\.status === 409/);
  assert.match(source, /files\.conflict \|\| !dirty/);
  assert.match(source, /if \(!files\.conflict\) files\.saveError = null/);
  assert.match(source, /reconcileSavedFileDraft\(snapshot\.content, files\.editDraft, saved\)/);
  assert.match(source, /Discard this draft and reload the latest file from disk\?/);
  assert.match(source, /function setViewMode[\s\S]+confirmDiscardFileEdit/s);
  assert.match(source, /function selectSession[\s\S]+confirmDiscardFileEdit/s);
  assert.match(source, /function selectMachine[\s\S]+confirmDiscardFileEdit/s);
  assert.match(source, /function selectPulse[\s\S]+confirmDiscardFileEdit/s);
  assert.match(source, /renderBreadcrumbs[\s\S]+confirmDiscardFileEdit\(files\)/s);
  assert.match(source, /window\.addEventListener\("beforeunload"[\s\S]+fileEditHasUnsavedWork/s);
  assert.match(source, /function connectPane\(resetProject = true\)[\s\S]+if \(resetProject\) resetProjectView\(\)/s);
  assert.match(source, /const invalidatePreview = files\.conflict \|\| files\.saving \|\| files\.reloading/);
  assert.match(source, /Back already exposed the existing Agents entry[\s\S]+history\.pushState\(appHistoryState\(appRoute\(url\)\), "", url\)/s);
  assert.match(source, /input\.focus\(\{ preventScroll: true \}\)/);
  assert.match(source, /function renderAgentBranch/);
  assert.match(source, /state\.projectView !== view \|\| view\.paneId !== paneId/);
  assert.match(source, /token\.textContent = segment\.text/);
  assert.match(source, /writeLocalStorage\([\s\S]*FILE_READER_STORAGE_KEY/);
  assert.match(source, /editor\.wrap = state\.fileReaderPreferences\.wrap \? "soft" : "off"/);
  assert.doesNotMatch(source, /\.innerHTML\s*=/);
  assert.match(css, /\.project-panel \{[^}]*min-height: 0;[^}]*overflow: hidden;[^}]*overscroll-behavior: contain;/s);
  assert.match(css, /\.code-viewer \{[^}]*overflow: auto;[^}]*overscroll-behavior: contain;[^}]*overflow-anchor: none;/s);
  assert.match(css, /\.view-switch \{[^}]*overflow-x: auto;/s);
  assert.match(css, /\.code-viewer\.file-wrap \.code-line-content \{[^}]*white-space: pre-wrap;[^}]*overflow-wrap: anywhere;/s);
  assert.match(css, /\.code-viewer\.file-wrap \.file-editor \{[^}]*white-space: pre-wrap;[^}]*overflow-x: hidden;/s);
  assert.match(css, /\.code-viewer\.file-size-small \{[^}]*--file-font-size: 10\.5px;/s);
  assert.match(css, /@media \(max-width: 720px\)[\s\S]*\.code-viewer\.file-size-small \{ --file-editor-font-size: 16px; \}[\s\S]*\.code-viewer\.file-size-medium \{ --file-editor-font-size: 17px; \}[\s\S]*\.code-viewer\.file-size-large \{ --file-editor-font-size: 19px; \}/s);
  assert.doesNotMatch(css, /\.file-editor[^}]*\{[^}]*transform\s*:/s);
});

test("conversation rendering never assigns agent content through innerHTML", () => {
  const source = readFileSync(new URL("./app.js", import.meta.url), "utf8");
  assert.doesNotMatch(source, /\.innerHTML\s*=/);
  assert.match(source, /body\.append\(markdownFragment\(message\.markdown\)\)/);
  assert.match(source, /details\.className = "code-block"/);
  assert.match(source, /details\.className = "tool-card"/);
});

test("Pulse requests require an explicit positive account and keep it in the route boundary", () => {
  assert.equal(pulseAccountId("42"), 42);
  assert.equal(pulseAccountId("0"), null);
  assert.equal(pulseAccountId("1/../2"), null);
  assert.equal(pulseAccountId(Number.MAX_SAFE_INTEGER + 1), null);
  assert.equal(
    pulseAccountPath(42, "usage", { profile: "Claude / Max", cursor: "100", limit: 100 }),
    "/api/v1/pulse/accounts/42/usage?profile=Claude+%2F+Max&cursor=100&limit=100",
  );
  assert.equal(pulseAccountPath(42, "../../sessions"), null);
  assert.equal(pulseAccountPath(42, "usage", { arbitrary_sql: "DROP", limit: 100 }), "/api/v1/pulse/accounts/42/usage?limit=100");
assert.equal(
  pulseProfileVisibilityPath(42, "../../../other account"),
  "/api/v1/pulse/accounts/42/profiles/..%2F..%2F..%2Fother%20account/visibility",
);
assert.equal(
  pulseProfileSettingsPath(42, "Claude / Max"),
  "/api/v1/pulse/accounts/42/profiles/Claude%20%2F%20Max/settings",
);
assert.equal(pulseProfileSettingsPath(0, "claude"), null);
assert.equal(pulseForcePollPath(42), "/api/v1/pulse/accounts/42/poll");
  assert.equal(pulseAlertActionPath(42, 7, "acknowledge"), "/api/v1/pulse/accounts/42/alerts/7/acknowledge");
  assert.equal(pulseAlertActionPath(42, 7, "delete"), null);
  assert.equal(pulseSubscriptionPath(42, 9), "/api/v1/pulse/accounts/42/alert-subscriptions/9");
  assert.equal(pulseIngestTokenPath(42), "/api/v1/pulse/accounts/42/ingest-tokens");
  assert.equal(pulseIngestTokenPath(42, 9), "/api/v1/pulse/accounts/42/ingest-tokens/9");
  assert.equal(pulseIngestTokenPath(42, "../9"), null);
  assert.equal(pulsePricingPath(42, "claude-3.5"), "/api/v1/pulse/accounts/42/pricing/claude-3.5");
  assert.equal(pulsePricingPath(42, "../other"), null);
});

test("Pulse refresh and pagination are bounded and stale account responses are rejected", () => {
  assert.deepEqual([0, 1, 2, 3, 20].map(pulseRefreshDelay), [60_000, 120_000, 240_000, 300_000, 300_000]);
  assert.equal(pulseCanFollowCursor("100", 1), true);
  assert.equal(pulseCanFollowCursor("400", 4), false);
  assert.equal(pulseCanFollowCursor("", 1), false);
  assert.equal(pulseRequestStillCurrent(41, 41, 8, 8), true);
  assert.equal(pulseRequestStillCurrent(41, 42, 8, 8), false);
  assert.equal(pulseRequestStillCurrent(41, 41, 8, 9), false);
  const source = readFileSync(new URL("./app.js", import.meta.url), "utf8");
  assert.match(source, /setTimeout\(\(\) => \{ void refreshPulse\(\); \}, pulseRefreshDelay\(state\.pulseFailures\)\)/);
  assert.doesNotMatch(source, /setInterval\([^\n]*refreshPulse/);
  assert.match(source, /JSON\.stringify\(\{ profile: profile\.name \}\)/);
  assert.match(source, /Collect this account now[\s\S]*JSON\.stringify\(\{\}\)/);
});

test("Pulse invalidations are account scoped, monotonic, and reconnect safe", () => {
  assert.equal(pulseEventsPath(42), "/api/v1/pulse/accounts/42/events");
  assert.equal(pulseEventsPath("../42"), null);
  assert.equal(pulseRevisionId("18446744073709551615"), "18446744073709551615");
  assert.equal(pulseRevisionId("18446744073709551616"), null);
  assert.equal(pulseRevisionId("01"), null);
  assert.equal(pulseRevisionId("1".repeat(1000)), null);
  assert.equal(pulseInvalidationAction(null, "7", false), "refresh");
  assert.equal(pulseInvalidationAction("7", "7", false), "ignore");
  assert.equal(pulseInvalidationAction("7", "6", false), "ignore");
  assert.equal(pulseInvalidationAction("7", "11", false), "refresh");
  assert.equal(pulseInvalidationAction("11", "11", true), "refresh", "reconnect initial event is authoritative");
  assert.equal(pulseInvalidationAction("11", "x", true), "invalid");
  assert.deepEqual([0, 1, 2, 5, 50].map(pulseReconnectDelay), [1_000, 2_000, 4_000, 30_000, 30_000]);
});

test("Pulse stream lifecycle closes on account switch and while hidden", () => {
  const source = readFileSync(new URL("./app.js", import.meta.url), "utf8");
  assert.match(source, /function stopPulseEvents\(\)[\s\S]*state\.pulseSource\?\.close\(\)[\s\S]*clearTimeout\(state\.pulseReconnectTimer\)/);
  assert.match(source, /function setPulseAccount\(value\)[\s\S]*stopPulseEvents\(\)[\s\S]*connectPulseEvents\(\)/);
  assert.match(source, /visibilitychange[\s\S]*document\.hidden[\s\S]*stopPulseEvents\(\)[\s\S]*loadPulseAccounts\(true\)/);
  assert.match(source, /source\.onerror[\s\S]*source\.close\(\)[\s\S]*pulseReconnectDelay\(state\.pulseEventFailures\)/);
  assert.match(source, /pulseInvalidationTimer[\s\S]*PULSE_INVALIDATION_DEBOUNCE_MS/);
  assert.doesNotMatch(source, /setInterval\([^\n]*connectPulseEvents/);
});

test("Pulse UI discovers accounts and opens the familiar dashboard without numeric ID entry", () => {
  const source = readFileSync(new URL("./app.js", import.meta.url), "utf8");
  const html = readFileSync(new URL("./index.html", import.meta.url), "utf8");
  const css = readFileSync(new URL("./app.css", import.meta.url), "utf8");
  const accounts = pulseAccounts([
    { id: 4, identity: "ryanmurf@gmail.com", display_name: "Ryan" },
    { id: 7, identity: "work@example.test", display_name: null },
  ]);
  assert.deepEqual(accounts.map((account) => account.id), [4, 7]);
  assert.equal(pulseAccountLabel(accounts[0]), "Ryan");
  assert.equal(pulseAccountLabel(accounts[1]), "work@example.test");
  assert.equal(preferredPulseAccount(accounts, 7, 4), 7);
  assert.equal(preferredPulseAccount(accounts, 99, 4), 4);
  assert.equal(preferredPulseAccount(accounts, null, null), 4);
  assert.deepEqual(pulseAccounts([{ id: 4, identity: "" }]), []);
  assert.match(source, /function pulseNode[\s\S]*document\.createElement\(tag\)/);
  assert.match(source, /\$\("pulse-content"\)[\s\S]*replaceChildren|content\.replaceChildren\(renderer\(\)\)/);
  assert.doesNotMatch(source, /\.innerHTML\s*=|insertAdjacentHTML/);
  assert.match(source, /pulseRequestStillCurrent\(account, state\.pulseAccount, generation, state\.pulseGeneration\)/);
  assert.match(html, /<select id="pulse-account"[^>]+disabled>/);
  assert.doesNotMatch(html, /Account ID|inputmode="numeric"/);
  assert.match(html, /id="pulse-account-form"/);
  assert.match(html, /data-pulse-tab="dashboard"[^>]+>Dashboard</);
  assert.match(html, /id="pulse-mobile-back"/);
  assert.match(source, /request\("\/api\/v1\/pulse\/accounts"\)/);
  assert.match(source, /function renderPulseDashboard\(\)[\s\S]*renderPulseOverview\(\)[\s\S]*renderPulseReports\(\)[\s\S]*renderPulseAlerts\(\)/);
  assert.match(source, /No Pulse account is configured on this server/);
  assert.match(source, /Copy the \$\{issued\.machine\} token now\. It cannot be shown again\./);
  assert.match(source, /navigator\.clipboard\.writeText\(issued\.token\)/);
  assert.match(source, /state\.pulseIssuedToken = null/);
  assert.match(source, /item\.scope === "override"[\s\S]*pulsePricingPath\(state\.pulseAccount, rule\.key\)[\s\S]*method: "DELETE"/);
  assert.match(css, /\.pulse-view \{[^}]*min-width: 0;[^}]*overflow: auto;[^}]*contain: layout paint;/s);
  assert.match(css, /\.pulse-token-copy \{[^}]*minmax\(0, 1fr\)/s);
  assert.match(css, /@media \(max-width: 720px\)[\s\S]*\.pulse-view \{[^}]*width: 100%;[^}]*min-width: 0;/s);
  assert.match(css, /\.pulse-offline/);
  assert.match(source, /No visible profiles or quota snapshots are available/);
});

test("Pulse mutations capture account-scoped paths and mobile reply submit remains reachable", () => {
  const source = readFileSync(new URL("./app.js", import.meta.url), "utf8");
  const css = readFileSync(new URL("./app.css", import.meta.url), "utf8");
  assert.match(source, /async function mutatePulse\(path[\s\S]*const account = state\.pulseAccount;[\s\S]*await request\(path, options\)/);
  assert.match(source, /replyForm\.addEventListener\("submit"/);
  assert.match(source, /pulseAlertActionPath\(state\.pulseAccount, event\.id, "reply"\)/);
  assert.match(source, /reply\.maxLength = Number\(state\.pulseData\.limits\?\.max_alert_reply_bytes\) \|\| 2_048/);
  assert.match(source, /monthly_budget_usd: budget\.value === "" \? null : Number\(budget\.value\)/);
  assert.match(source, /filter\(\(item\) => isMachineControllable\(machineOf\(item\)\)\)/);
  assert.match(source, /const needed = alertType\.value !== "auth_failure"/);
  assert.match(source, /paneOption\.disabled = !deliveryCapabilities\.pane \|\| !needed/);
  assert.match(source, /Channel delivery requires a live negotiated client capability/);
  assert.match(css, /\.pulse-alert-actions > button, \.pulse-reply-form input, \.pulse-reply-form button \{ min-height: 44px; \}/);
  assert.match(css, /@media \(max-width: 720px\)[\s\S]*\.pulse-profile-actions \{[^}]*width: 100%;[^}]*flex-direction: column;/s);
});

test("launch picker groups profiles by agent and applies project-local defaults", () => {
  const profiles = [
    { id: "profile-0", harness: "codex", name: "Default" },
    { id: "profile-1", harness: "claude", name: "Focused" },
    { id: "profile-2", harness: "claude", name: "Default" },
  ];
  assert.deepEqual(harnessesForProfiles(profiles), ["codex", "claude"]);
  assert.deepEqual(profilesForHarness(profiles, "CLAUDE").map((profile) => profile.id), ["profile-1", "profile-2"]);
  const directory = "/Users/ryan/IdeaProjects/nes-spring/spring-ws";
  const preferences = projectPreference({
    project_preferences: {
      [directory]: { session_name: "spring-ws-review", harness: "claude", profile: "Focused" },
    },
  }, directory);
  assert.equal(preferences.harness, "claude");
  assert.equal(suggestedSessionName(directory, preferences), "spring-ws-review");
  assert.equal(projectLabel(directory), "nes-spring / spring-ws");
  assert.equal(suggestedSessionName(directory), "spring-ws");
});

test("duplicate launch reuses exact owner profile and mode IDs with a fresh tmux name", () => {
  const options = {
    machines: [{
      id: "max", label: "Max", online: true, directories: ["/workspace/atmux"],
      project_preferences: {},
      profiles: [{
        id: "profile-codex-max", name: "codex-max", harness: "codex",
        modes: [
          { id: "terra-high", model: "gpt-5.6-terra", effort: "high", service_tier: null },
          { id: "sol-fast", model: "gpt-5.6-sol", effort: "xhigh", service_tier: "fast" },
        ],
      }],
    }],
  };
  const running = {
    id: "max~%7", machine: "max", name: "kernel", agent: "codex",
    profile: "codex-max", path: "/workspace/atmux",
  };
  const duplicate = duplicateLaunchSelection(options, running, {
    pane_id: "max~%7", current_mode: "sol-fast", current: "gpt-5.6-sol", effort: "xhigh",
  }, [running, { ...running, id: "max~%8", name: "kernel-copy" }]);
  assert.deepEqual(duplicate, {
    machineId: "max",
    directory: "/workspace/atmux",
    harness: "codex",
    profileId: "profile-codex-max",
    modeId: "sol-fast",
    memoryMaxBytes: null,
    name: "kernel-copy-2",
  });
  assert.equal(launchMachines({ directories: ["/one"], profiles: [] })[0].id, "local");
  assert.equal(duplicateSessionName({ ...running, name: "x".repeat(100) }, []), `${"x".repeat(95)}-copy`);
});

test("memory launch choices parse Default, presets, and bounded whole-GiB custom values", () => {
  const GiB = 1024 ** 3;
  const memory = {
    supported: true,
    default_bytes: 16 * GiB,
    override_max_bytes: 24 * GiB,
    presets_bytes: [8 * GiB, 16 * GiB, 24 * GiB, 32 * GiB, 0, "bad"],
    note: "next relaunch",
  };
  assert.deepEqual(memoryLimitChoices(memory), {
    advertised: true,
    supported: true,
    defaultBytes: 16 * GiB,
    ceiling: 24 * GiB,
    presets: [8 * GiB, 16 * GiB, 24 * GiB],
    note: "next relaunch",
  });
  assert.equal(parseMemoryLimitSelection(memory, "", ""), null);
  assert.equal(parseMemoryLimitSelection(memory, String(8 * GiB), ""), 8 * GiB);
  assert.equal(parseMemoryLimitSelection(memory, "custom", "12"), 12 * GiB);
  assert.throws(() => parseMemoryLimitSelection(memory, "custom", "1.5"), /whole number/);
  assert.throws(() => parseMemoryLimitSelection(memory, "custom", "25"), /at most 24 GiB/);
  assert.throws(() => parseMemoryLimitSelection(memory, String(20 * GiB), ""), /owner-approved/);
  assert.equal(formatMemoryLimit(16 * GiB), "16 GiB");
  assert.deepEqual(memoryLimitChoices(null), {
    advertised: false,
    supported: false,
    defaultBytes: null,
    ceiling: null,
    presets: [],
    note: "Memory limit is owner managed; this owner does not advertise override support.",
  });
  assert.equal(defaultMemoryLimitLabel(null), "Default (owner managed)");
  assert.equal(defaultMemoryLimitLabel(memory), "Default (16 GiB)");
});

test("duplicate preserves an exact owner-allowed cap and rejects stale capability", () => {
  const GiB = 1024 ** 3;
  const session = {
    id: "max~%4", machine: "max", name: "worker", agent: "codex",
    profile: "Default", path: "/workspace", memory_max_bytes: 20 * GiB,
  };
  const machine = {
    id: "max", label: "Max", online: true, directories: [session.path],
    profiles: [{ id: "profile-0", name: "Default", harness: "codex", modes: [] }],
    memory: {
      supported: true, default_bytes: 16 * GiB, override_max_bytes: 24 * GiB,
      presets_bytes: [16 * GiB, 24 * GiB],
    },
  };
  assert.equal(
    duplicateLaunchSelection({ machines: [machine] }, session, null).memoryMaxBytes,
    20 * GiB,
  );
  assert.throws(
    () => duplicateLaunchSelection({ machines: [{ ...machine, memory: null }] }, session, null),
    /no longer allowed/,
  );
  assert.throws(
    () => duplicateLaunchSelection({ machines: [{ ...machine, memory: { ...machine.memory, override_max_bytes: 18 * GiB } }] }, session, null),
    /no longer allowed/,
  );
});

test("duplicate launch refuses stale profile or ambiguous model settings", () => {
  const session = {
    id: "midnight~%5", machine: "midnight", name: "planner", agent: "claude",
    profile: "max", path: "/Users/ryan/IdeaProjects/atmux",
  };
  const machine = {
    id: "midnight", label: "Midnight", online: true, directories: [session.path],
    profiles: [{
      id: "profile-max", name: "max", harness: "claude",
      modes: [{ id: "sonnet", model: "claude-sonnet-4-6" }, { id: "opus", model: "claude-opus-5" }],
    }],
  };
  assert.throws(
    () => duplicateLaunchSelection({ machines: [machine] }, session, null),
    /exact model, effort, or fast mode/,
  );
  assert.throws(
    () => duplicateLaunchSelection({ machines: [{ ...machine, profiles: [] }] }, session, null),
    /Profile max is no longer configured/,
  );
  assert.throws(
    () => duplicateLaunchSelection({ machines: [{ ...machine, online: false }] }, session, null),
    /Midnight is offline/,
  );
  const snapshot = duplicateSourceSnapshot(session);
  assert.equal(duplicateSourceMatches(snapshot, { ...session }), true);
  for (const changed of [
    { path: "/different" },
    { machine: "max" },
    { agent: "codex" },
    { profile: "Default" },
    { id: "midnight~%6" },
  ]) {
    assert.equal(duplicateSourceMatches(snapshot, { ...session, ...changed }), false);
  }
  assert.equal(duplicateSourceMatches(snapshot, null), false);
});

test("duplicate dialog fails closed across live model, stale request, and resume boundaries", () => {
  const source = readFileSync(new URL("./app.js", import.meta.url), "utf8");
  assert.doesNotMatch(source, /capabilitiesRequest[\s\S]{0,300}\.catch\(/);
  assert.match(source, /const generation = \+\+state\.launchDialogGeneration/);
  assert.match(source, /generation !== state\.launchDialogGeneration/);
  assert.match(source, /state\.sessions\.get\(sourceSnapshot\.id\)/);
  assert.match(source, /duplicateSourceMatches\(sourceSnapshot, liveDuplicateSession\)/);
  assert.match(source, /if \(state\.launchFlow === "duplicate"\) \{\s*clearLaunchSessions\(\);\s*return;/s);
  assert.match(source, /resume_session_id: duplicateFlow \? null : \(\$\("launch-session"\)\.value \|\| null\)/);
  assert.match(source, /launch-dialog"\)\.addEventListener\("close"[\s\S]*invalidateLaunchDialog\(false\)/);
});

test("session labels prioritize the folder and suppress an unhelpful Default profile", () => {
  const defaultClaude = {
    agent: "claude",
    profile: "Default",
    path: "/home/ryan/Documents/properties/104-blue-mountain/solar",
  };
  assert.equal(sessionFolderLabel(defaultClaude), "104-blue-mountain / solar");
  assert.equal(sessionProfileLabel(defaultClaude), "");
  assert.equal(sessionProfileLabel({ ...defaultClaude, profile: "claude-max" }), "claude-max");
  assert.equal(sessionFolderLabel({ path: "" }), "");

  const source = readFileSync(new URL("./app.js", import.meta.url), "utf8");
  assert.match(source, /node\.sub\.textContent = \[folder, profile, session\.status, session\.agent\]/);
  assert.match(source, /\$\("agent-meta"\)\.textContent = \[\s*folder, profile,/);
  assert.match(source, /\$\("agent-meta"\)\.title = selected\.path/);
});

test("launch picker keeps agent and profile selectors visible for a single choice", () => {
  const source = readFileSync(new URL("./app.js", import.meta.url), "utf8");
  assert.match(source, /launch-harness-row"\)\.hidden = harnesses\.length === 0/);
  assert.match(source, /launch-profile-row"\)\.hidden = profiles\.length === 0/);
});

test("session rendering preserves nodes and protocol faults avoid EventSource error events", () => {
  const source = readFileSync(new URL("./app.js", import.meta.url), "utf8");
  assert.doesNotMatch(source, /sessionList\.replaceChildren/);
  assert.match(source, /addEventListener\("protocol\.error"/);
  assert.doesNotMatch(source, /addEventListener\("error"/);
});

test("agent details expose the safe tmux launch descriptor beside the session name", () => {
  const source = readFileSync(new URL("./app.js", import.meta.url), "utf8");
  const markup = readFileSync(new URL("./index.html", import.meta.url), "utf8");
  assert.match(source, /selected\.launch_command \|\| selected\.command/);
  assert.match(source, /tmux launch: \$\{launchCommand\}/);
  assert.match(markup, /id="agent-launch"/);
});

test("Actions groups fixed special keys and Compact outside the composer", () => {
  const source = readFileSync(new URL("./app.js", import.meta.url), "utf8");
  const markup = readFileSync(new URL("./index.html", import.meta.url), "utf8");
  assert.match(markup, /id="tmux-prefix-twice"/);
  assert.match(markup, /id="quick-actions-open"/);
  assert.match(markup, /id="quick-duplicate"[^>]*>Duplicate agent</);
  assert.match(markup, /id="quick-compact"/);
  assert.doesNotMatch(markup, /id="compact"/);
  assert.match(source, /special-keys/);
  assert.match(source, /action: "tmux_prefix_twice"/);
  assert.match(source, /compactSelectedAgent/);
  assert.match(source, /text: "\/compact"/);
  assert.match(source, /duplicateLaunchSelection\([\s\S]*\[\.\.\.state\.sessions\.values\(\)\]/);
});

test("composer Send has a dedicated tap handler and stays disabled while sending or relaunching", () => {
  const source = readFileSync(new URL("./app.js", import.meta.url), "utf8");
  const markup = readFileSync(new URL("./index.html", import.meta.url), "utf8");
  assert.match(markup, /<button id="send" class="primary" type="button">Send<\/button>/);
  assert.match(source, /\$\("send"\)\.addEventListener\("click", \(\) => \{ void sendComposerMessage\(\); \}\);/);
  assert.match(source, /\$\("send"\)\.disabled = !controllable \|\| state\.composerSending \|\| resuming;/);
  assert.match(source, /state\.composerSending = true;/);
});

test("composer Enter sends while Ctrl or Command Enter inserts a newline", () => {
  assert.equal(composerEnterAction({ key: "Enter" }), "send");
  assert.equal(composerEnterAction({ key: "Enter", ctrlKey: true }), "newline");
  assert.equal(composerEnterAction({ key: "Enter", metaKey: true }), "newline");
  assert.equal(composerEnterAction({ key: "Enter", shiftKey: true }), "newline");
  assert.equal(composerEnterAction({ key: "Enter", isComposing: true }), null);
  assert.equal(composerEnterAction({ key: "Enter", altKey: true }), null);
  assert.equal(composerEnterAction({ key: "ArrowUp" }), null);
  const source = readFileSync(new URL("./app.js", import.meta.url), "utf8");
  assert.match(source, /action === "send"/);
  assert.match(source, /input\.setRangeText\("\\n", start, end, "end"\)/);
});

test("parseCompositeId splits machine~pane without mangling bare tmux identifiers", () => {
  assert.deepEqual(parseCompositeId("local~%3"), { machine: "local", pane: "%3" });
  assert.deepEqual(parseCompositeId("gpu-box~release-review"), { machine: "gpu-box", pane: "release-review" });
  // Bare pane ids and names round-trip untouched, so old links keep working.
  assert.deepEqual(parseCompositeId("%3"), { machine: null, pane: "%3" });
  assert.deepEqual(parseCompositeId("release-review"), { machine: null, pane: "release-review" });
  assert.deepEqual(parseCompositeId("~leading"), { machine: null, pane: "~leading" });
  assert.deepEqual(parseCompositeId("trailing~"), { machine: null, pane: "trailing~" });
  assert.deepEqual(parseCompositeId(undefined), { machine: null, pane: null });
});

test("sessionMachineId prefers the explicit field and falls back to the composite id", () => {
  assert.equal(sessionMachineId({ id: "gpu-box~%1", machine: "gpu-box" }), "gpu-box");
  assert.equal(sessionMachineId({ id: "gpu-box~%1" }), "gpu-box");
  // A pre-federation payload lands under the coordinator named by the overview.
  assert.equal(sessionMachineId({ id: "%1" }, "tron"), "tron");
  // Older callers without overview context retain their compatibility default.
  assert.equal(sessionMachineId({ id: "%1" }), "local");
});

test("preferredLaunchMachineId skips online owners that cannot actually launch", () => {
  const profile = { id: "profile-default", name: "Default", harness: "codex" };
  const machines = [
    machine("home", "Home", true, { directories: [], profiles: [] }),
    machine("midnight", "Midnight", true, { directories: ["/work/midnight"], profiles: [profile] }),
    machine("max", "Max", false, { directories: ["/work/max"], profiles: [profile] }),
  ];
  assert.equal(preferredLaunchMachineId(machines, "midnight", null), "midnight");
  assert.equal(
    preferredLaunchMachineId(machines, "home", null),
    "midnight",
    "an online but unconfigured contextual machine is not a launch target",
  );
  assert.equal(
    preferredLaunchMachineId(machines, null, { id: "midnight~%7" }),
    "midnight",
  );
  assert.equal(
    preferredLaunchMachineId(machines, null, { id: "max~%9", machine: "max" }),
    "midnight",
    "an offline contextual machine falls back to the first launch-capable target",
  );
  assert.equal(preferredLaunchMachineId(machines, null, null), "midnight");
  assert.equal(
    preferredLaunchMachineId([
      machines[0],
      machine("max", "Max", true, { directories: ["/work/max"], profiles: [profile] }),
      machine("tron", "Tron", true, { directories: ["/work/tron"], profiles: [profile] }),
    ], null, { id: "%9" }, "tron"),
    "tron",
    "a bare hydrated pane uses the overview's local owner instead of a hardcoded local id",
  );
  assert.equal(isLaunchCapableMachine(machines[0]), false);
  assert.equal(isLaunchCapableMachine(machines[1]), true);
  assert.equal(
    isLaunchCapableMachine(machine("profiles-only", "Profiles", true, { directories: [], profiles: [profile] })),
    false,
  );
  assert.equal(
    isLaunchCapableMachine(machine("folders-only", "Folders", true, { directories: ["/work"], profiles: [] })),
    false,
  );
  assert.equal(preferredLaunchMachineId([machines[0], machines[2]], null, null), null);
  assert.equal(preferredLaunchMachineId([], null, null), null);
});

test("groupSessionsByMachine keeps the server's machine order and sorts within a group", () => {
  const machines = [machine("local", "This machine", true), machine("gpu-box", "GPU box", true), machine("mini", "Mini", false)];
  const groups = groupSessionsByMachine([
    federated("gpu-box", "%2", "working", "trainer"),
    federated("local", "%9", "working", "zeta"),
    federated("gpu-box", "%1", "waiting", "evaluator"),
    federated("local", "%8", "waiting", "alpha"),
  ], machines);

  assert.deepEqual(groups.map((group) => group.machine.id), ["local", "gpu-box", "mini"]);
  // Names, rather than volatile status, define the click-target order.
  assert.deepEqual(groups[0].sessions.map((s) => s.name), ["alpha", "zeta"]);
  assert.deepEqual(groups[1].sessions.map((s) => s.name), ["evaluator", "trainer"]);
  // An offline machine still renders its (empty) group.
  assert.deepEqual(groups[2].sessions, []);
});

test("groupSessionsByMachine tolerates sessions from a machine the overview omitted", () => {
  const groups = groupSessionsByMachine([federated("ghost", "%1", "working")], [machine("local", "This machine", true)]);
  assert.deepEqual(groups.map((group) => group.machine.id), ["local", "ghost"]);
  assert.equal(groups[1].sessions.length, 1);
});

test("groupSessionsByMachine keeps a single-machine dashboard identical to before", () => {
  const sessions = [federated("local", "%2", "working", "b"), federated("local", "%1", "waiting", "a")];
  const groups = groupSessionsByMachine(sessions, [machine("local", "This machine", true)]);
  assert.equal(groups.length, 1);
  assert.deepEqual(groups[0].sessions, sortSessions(sessions));
});

test("groupSessionsByMachine assigns legacy bare panes to the overview's local owner", () => {
  const localOwner = { ...machine("tron", "Tron", true), kind: "local" };
  const groups = groupSessionsByMachine([session("%1", "waiting", "legacy")], [localOwner]);
  assert.deepEqual(groups.map((group) => group.machine.id), ["tron"]);
  assert.deepEqual(groups[0].sessions.map((item) => item.id), ["%1"]);
});

test("machineStatusLabel reports counts when online and last-seen when offline", () => {
  const now = 1_700_000_000_000;
  assert.equal(machineStatusLabel(machine("gpu-box", "GPU box", true, { sessions: 2 }), now), "2 agents");
  assert.equal(machineStatusLabel(machine("gpu-box", "GPU box", true, { sessions: 1 }), now), "1 agent");
  assert.equal(
    machineStatusLabel(machine("local", "This machine", true, { sessions: 0, health: "tmux unavailable" }), now),
    "0 agents · tmux unavailable",
  );
  assert.equal(
    machineStatusLabel(machine("mini", "Mini", false, { last_seen_ms: now - 185_000, health: "connection refused" }), now),
    "Offline · last seen 3m ago · connection refused",
  );
  // A machine that was never reachable has no last-seen time to report.
  assert.equal(machineStatusLabel(machine("mini", "Mini", false, { health: "connecting" }), now), "Offline · connecting");
  assert.equal(machineStatusLabel(null, now), "");
});

test("GPU formatting exposes every available bounded device counter without inventing zeros", () => {
  const gpu = {
    id: "0000:03:00.0",
    name: "Radeon RX 7900 XTX",
    vendor: "AMD",
    pci_bus_id: "0000:03:00.0",
    utilization_percent: 42,
    memory_used_bytes: 8 * 1024 ** 3,
    memory_total_bytes: 24 * 1024 ** 3,
    memory_shared: false,
    memory_pressure_percent: 33,
    temperature_celsius: 61.5,
    power_draw_watts: 202.4,
    power_limit_watts: 355,
    graphics_clock_mhz: 2450,
    memory_clock_mhz: 1250,
    video_clock_mhz: 930,
    fan_percent: 48,
    fan_speed_rpm: 1380,
    thermal_state: "nominal",
    performance_state: "high",
    driver_version: "amdgpu",
    runtime_version: "ROCm 7",
    compute_capability: "gfx1100",
    core_count: 96,
    unavailable: ["junction temperature"],
  };
  assert.equal(gpuSummary(gpu), "Radeon RX 7900 XTX · 42% · 8.0 GiB / 24 GiB · 61.5°C");
  assert.deepEqual(gpuDetailLines(gpu), [
    "Identity · AMD · 0000:03:00.0",
    "Memory · VRAM 8.0 GiB / 24 GiB · pressure 33%",
    "Power / thermal · power 202.4 W · limit 355 W · 61.5°C · thermal nominal",
    "Clocks · graphics 2450 MHz · memory 1250 MHz · video 930 MHz",
    "Cooling / performance · fan 48% · 1380 RPM · state high",
    "Driver / compute · driver amdgpu · runtime ROCm 7 · compute gfx1100 · 96 cores",
    "Unavailable · junction temperature",
  ]);
  assert.deepEqual(gpuDiagnosticLines([
    { source: "nvidia-smi", message: "not installed" },
    { source: "", message: "" },
  ]), ["nvidia-smi · not installed"]);
  assert.equal(gpuSummary({ name: "Apple GPU", memory_shared: true }), "Apple GPU");
  assert.deepEqual(gpuDetailLines({ name: "Apple GPU", memory_shared: true }), ["Memory · shared memory"]);
  assert.equal(gpuSummary({ name: "Intel GPU", memory_total_bytes: 8 * 1024 ** 3 }), "Intel GPU · — / 8.0 GiB");
});

test("formatRelativeTime degrades gracefully across scales and bad input", () => {
  const now = 1_700_000_000_000;
  assert.equal(formatRelativeTime(now - 5_000, now), "5s ago");
  assert.equal(formatRelativeTime(now - 90_000, now), "1m ago");
  assert.equal(formatRelativeTime(now - 7_200_000, now), "2h ago");
  assert.equal(formatRelativeTime(now - 172_800_000, now), "2d ago");
  // Clock skew must not produce a negative age.
  assert.equal(formatRelativeTime(now + 10_000, now), "0s ago");
  assert.equal(formatRelativeTime(undefined, now), "");
  assert.equal(formatRelativeTime(now, undefined), "");
});

test("system telemetry formats compact uptime and explicit unavailable values", () => {
  assert.equal(formatUptime(183_840), "2d 3h 4m");
  assert.equal(formatUptime(183_899), "2d 3h 4m");
  assert.equal(formatUptime(3_600), "1h");
  assert.equal(formatUptime(59), "<1m");
  assert.equal(formatUptime(undefined), "Unavailable");
  assert.deepEqual(systemMetricLines({
    uptime_seconds: 183_840,
    kernel_version: "6.8.0-48-generic",
    os_version: "Linux (Ubuntu 24.04)",
  }), [
    "Uptime · 2d 3h 4m",
    "Kernel · 6.8.0-48-generic",
    "OS · Linux (Ubuntu 24.04)",
  ]);
  assert.deepEqual(systemMetricLines({}), [
    "Uptime · Unavailable",
    "Kernel · Unavailable",
    "OS · Unavailable",
  ]);
});

test("isMachineControllable blocks control only for a known-offline machine", () => {
  assert.equal(isMachineControllable(machine("gpu-box", "GPU box", true)), true);
  assert.equal(isMachineControllable(machine("gpu-box", "GPU box", false)), false);
  // An unknown machine stays controllable so single-machine mode is unaffected.
  assert.equal(isMachineControllable(null), true);
});

test("reconcileSessions keys federated sessions by composite id so names may collide", () => {
  const result = reconcileSessions(new Map(), {
    revision: 1,
    sessions: [federated("local", "%1", "working", "review"), federated("gpu-box", "%1", "waiting", "review")],
  });
  assert.deepEqual([...result.keys()], ["local~%1", "gpu-box~%1"]);
  assert.equal(result.get("gpu-box~%1").status, "waiting");

  const patched = reconcileSessions(result, { remove: ["local~%1"], upsert: [] });
  assert.deepEqual([...patched.keys()], ["gpu-box~%1"]);
});

test("composite ids survive URL encoding for pane routes", () => {
  assert.equal(encodeURIComponent("gpu-box~%3"), "gpu-box~%253");
  assert.equal(decodeURIComponent(encodeURIComponent("gpu-box~%3")), "gpu-box~%3");
  // The separator is unreserved, so no proxy has to decode it.
  assert.doesNotMatch(encodeURIComponent("gpu-box~%3"), /%2F/i);
});

test("machine grouping and offline handling are wired into the rendered dashboard", () => {
  const source = readFileSync(new URL("./app.js", import.meta.url), "utf8");
  assert.match(source, /groupSessionsByMachine\(visible, state\.machines\)/);
  assert.match(source, /machine: \$\("launch-machine"\)\.value \|\| null/);
  // Controls are disabled rather than silently failing against an offline node.
  assert.match(source, /\$\(id\)\.disabled = !controllable/);
  assert.match(source, /SpeechRecognition \|\| window\.webkitSpeechRecognition/);
  assert.match(source, /addEventListener\("pointerdown"/);
  assert.match(source, /function renderMachineDetail/);
});

test("classifyOverviewUpdate accepts snapshots and only contiguous patches", () => {
  assert.equal(classifyOverviewUpdate(7, { revision: 9, sessions: [] }), "snapshot");
  assert.equal(classifyOverviewUpdate(7, { base_revision: 7, revision: 8, upsert: [] }), "patch");
  // A patch built on a revision we never saw means we missed one.
  assert.equal(classifyOverviewUpdate(7, { base_revision: 6, revision: 8, upsert: [] }), "resync");
  assert.equal(classifyOverviewUpdate(7, { base_revision: 8, revision: 9, upsert: [] }), "resync");
  // A patch with no base at all is never trusted.
  assert.equal(classifyOverviewUpdate(7, { revision: 8, upsert: [] }), "resync");
  assert.equal(classifyOverviewUpdate(0, { base_revision: 0, revision: 1, upsert: [] }), "patch");
});

test("reduceOverview applies contiguous patches and refuses to merge across a gap", () => {
  const snapshot = reduceOverview({ revision: 0, sessions: new Map() }, {
    revision: 4,
    sessions: [federated("local", "%1", "working", "alpha")],
    machines: [machine("local", "This machine", true)],
  });
  assert.equal(snapshot.resync, false);
  assert.equal(snapshot.revision, 4);
  assert.deepEqual([...snapshot.sessions.keys()], ["local~%1"]);

  const patched = reduceOverview({ revision: 4, sessions: snapshot.sessions }, {
    base_revision: 4,
    revision: 5,
    upsert: [federated("local", "%2", "waiting", "beta")],
    remove: [],
  });
  assert.equal(patched.resync, false);
  assert.equal(patched.revision, 5);
  assert.deepEqual([...patched.sessions.keys()], ["local~%1", "local~%2"]);

  // A stale-based patch leaves the client's state exactly as it was, so the
  // caller can reconnect for a fresh snapshot instead of showing a merge of two
  // different server states.
  const stale = reduceOverview({ revision: 5, sessions: patched.sessions }, {
    base_revision: 4,
    revision: 6,
    upsert: [federated("local", "%3", "working", "gamma")],
    remove: ["local~%1"],
  });
  assert.equal(stale.resync, true);
  assert.equal(stale.revision, 5);
  assert.equal(stale.sessions, patched.sessions);
  assert.deepEqual([...stale.sessions.keys()], ["local~%1", "local~%2"]);
});

test("a stale overview patch reconnects for a snapshot instead of merging", () => {
  const source = readFileSync(new URL("./app.js", import.meta.url), "utf8");
  assert.match(source, /if \(result\.resync\) \{\s*\/\/[\s\S]*?connectOverview\(\);/);
});

test("paneNotice reports a machine outage without claiming a local tmux fault", () => {
  const now = 1_700_000_000_000;
  const offline = machine("gpu-box", "GPU box", false, { last_seen_ms: now - 5_000, health: "connection refused" });
  assert.equal(
    paneNotice(offline, null, now),
    "GPU box is offline. Offline · last seen 5s ago · connection refused",
  );
  // A healthy machine with a failing pane stream shows the stream's own reason.
  const online = machine("gpu-box", "GPU box", true, { sessions: 1 });
  assert.equal(
    paneNotice(online, { error: "machine gpu-box rejected /api/v1/panes/%4 with 500", kind: "upstream" }, now),
    "machine gpu-box rejected /api/v1/panes/%4 with 500",
  );
  assert.equal(paneNotice(online, null, now), "");
  assert.equal(paneNotice(null, null, now), "");
});

test("paneErrorLabel distinguishes an outage from an upstream or protocol fault", () => {
  assert.equal(paneErrorLabel("offline"), "Machine offline");
  assert.equal(paneErrorLabel("upstream"), "Machine unreachable");
  assert.equal(paneErrorLabel("protocol"), "Stream error");
  assert.equal(paneErrorLabel(undefined), "Stream error");
});

test("model picker reports current, unsupported, offline, and in-flight states", () => {
  const claude = { id: "midnight~%4", agent: "claude" };
  const capabilities = {
    pane_id: claude.id,
    current: "sonnet",
    models: [
      { id: "sonnet", label: "Sonnet", switchable: true },
      { id: "claude-opus-4-1", label: "Pinned", switchable: false },
    ],
    note: null,
  };
  assert.deepEqual(modelPickerState(claude, capabilities, true, null), {
    visible: true,
    loading: false,
    current: "sonnet",
    effort: "",
    currentMode: "",
    models: capabilities.models,
    disabled: false,
    status: "Current: sonnet",
  });
  assert.equal(modelPickerState(claude, capabilities, false, null).status, "Machine offline");
  assert.equal(modelPickerState(claude, capabilities, true, claude.id).status, "Switching…");
  assert.equal(modelPickerState(claude, null, true, null).status, "Checking models…");

  const unsupported = { ...capabilities, models: [], note: "codex 0.999 has an unsupported picker" };
  const view = modelPickerState({ id: claude.id, agent: "codex" }, unsupported, true, null);
  assert.equal(view.disabled, true);
  assert.match(view.status, /unsupported/);
  assert.equal(modelPickerState({ id: "%9", agent: "shell" }, null, true, null).visible, false);
});

test("Claude resume action is capability-gated and protects active work", () => {
  const claude = { id: "tron~%7", agent: "claude" };
  const ready = {
    pane_id: claude.id,
    resume_available: true,
    resume_note: null,
  };
  assert.deepEqual(claudeResumeState(claude, ready, true, null), {
    visible: true,
    available: true,
    disabled: false,
    status: "Ready to relaunch the saved conversation",
  });
  const working = {
    pane_id: claude.id,
    resume_available: false,
    resume_note: "Claude is working; wait or interrupt before relaunching",
  };
  const view = claudeResumeState(claude, working, true, null);
  assert.equal(view.visible, true);
  assert.equal(view.disabled, true);
  assert.match(view.status, /working/);
  assert.equal(claudeResumeState(claude, ready, true, claude.id).disabled, true);
  assert.equal(claudeResumeState({ id: "%8", agent: "codex" }, ready, true, null).visible, false);
});

test("model switch captures the pane id before awaiting and routes only a profile mode id", () => {
  const source = readFileSync(new URL("./app.js", import.meta.url), "utf8");
  const handler = source.slice(
    source.indexOf("async function switchAgentModel"),
    source.indexOf('$("message").addEventListener("paste"'),
  );
  assert.match(handler, /const paneId = state\.selected;/);
  assert.match(handler, /encodeURIComponent\(paneId\).*\/model/s);
  assert.match(handler, /JSON\.stringify\(\{ mode_id: modeId \}\)/);
  assert.doesNotMatch(handler, /state\.selected.*\/model/);
  assert.match(source, /modelPickerState\([\s\S]*state\.modelSwitchingPaneId/);
  assert.match(source, /\$\("quick-agent-model"\)\.addEventListener\("change"/);
});

test("Claude resume uses a confirmation and never sends browser-supplied session data", () => {
  const source = readFileSync(new URL("./app.js", import.meta.url), "utf8");
  const markup = readFileSync(new URL("./index.html", import.meta.url), "utf8");
  const handler = source.slice(
    source.indexOf('$("resume-confirm").addEventListener'),
    source.indexOf('$("launch-open").addEventListener'),
  );
  assert.match(markup, /id="quick-resume"/);
  assert.match(markup, /id="resume-dialog"/);
  assert.match(markup, /Any in-flight work is terminated/);
  assert.match(markup, /Custom launch flags that are not configured in atmux are not preserved/);
  assert.match(handler, /encodeURIComponent\(target\)\}\/resume/);
  assert.match(handler, /body: JSON\.stringify\(\{\}\)/);
  assert.doesNotMatch(handler, /session_id:\s*[^,}]+/);
  assert.doesNotMatch(handler, /dangerously-skip-permissions/);
  assert.doesNotMatch(source, /CLAUDE_CONFIG_DIR/);
});

test("Tron Quick Resume is confirmed and sends no browser command or path", () => {
  const source = readFileSync(new URL("./app.js", import.meta.url), "utf8");
  const markup = readFileSync(new URL("./index.html", import.meta.url), "utf8");
  assert.match(markup, /id="recovery-open"/);
  assert.match(markup, /id="recovery-dialog"/);
  assert.match(markup, /Sessions that already exist are preserved/);
  assert.match(source, /\/api\/v1\/machines\/tron\/quick-resume/);
  assert.match(source, /body: JSON\.stringify\(\{\}\)/);
  assert.doesNotMatch(source, /resume-tron\.sh/);
  assert.doesNotMatch(source, /recovery[^\n]*(?:command|script_path|arguments):/i);
});

test("pane failures use a pane-scoped surface and never the tmux health alert", () => {
  const source = readFileSync(new URL("./app.js", import.meta.url), "utf8");
  assert.match(source, /addEventListener\("pane\.error"/);
  // setHealth renders "tmux monitor: …", so only the overview stream may call it.
  const paneHandlers = source.slice(source.indexOf("function connectPane"), source.indexOf("function drawPane"));
  assert.doesNotMatch(paneHandlers, /setHealth\(/);
  assert.match(source, /paneNotice\(machine, state\.paneError, Date\.now\(\)\)/);
});
