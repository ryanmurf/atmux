"use strict";

const WORKING_TO_WAITING_HOLD_MS = 2_500;
const MAX_MESSAGE_BYTES = 64 * 1024;
const MAX_MESSAGE_HISTORY_ENTRIES = 50;
const MAX_IMAGE_ATTACHMENTS = 4;
const MAX_IMAGE_BYTES = 4 * 1024 * 1024;
const MAX_TOTAL_IMAGE_BYTES = 12 * 1024 * 1024;
const IMAGE_MESSAGE_TEXT_RESERVE = 2 * 1024;
const MAX_QUEUED_COMPOSER_MESSAGES = 4;
const MAX_REMEMBERED_LAUNCH_DIRECTORIES = 32;
const MAX_LAUNCH_DIRECTORY_CANDIDATES = 4_096;
const MAX_LAUNCH_DIRECTORY_SUGGESTIONS = 40;
const LAUNCH_DIRECTORY_SEARCH_DEBOUNCE_MS = 140;
const MAX_PROJECT_ENTRIES = 512;
const MAX_PROJECT_SOURCE_CHARS = 256 * 1024;
const MAX_PROJECT_SOURCE_LINES = 4_000;
const MAX_FILE_REFERENCE_CHARS = 12_000;
const MAX_FILE_REFERENCE_LINES = 200;
const CONTENT_HASH_PATTERN = /^[a-f0-9]{64}$/;
const LIVE_TAIL_TOLERANCE = 2;
// Conversation cards end on fractional pixel boundaries and mobile zoom adds
// its own rounding, so the reader counts as parked at the tail well before the
// scroll offset matches exactly.
const STICKY_BOTTOM_TOLERANCE = 24;
// One uninterrupted run stays one row, up to the server's transcript limit.
const MAX_COLLAPSED_TOOL_RUN = 240;
const MIN_COLLAPSED_TOOL_RUN = 2;
// Bare URLs only: markdown links are already tokenized, and the closing set is
// trimmed afterwards so surrounding prose never joins the address.
const AUTOLINK_PATTERN = /https?:\/\/[^\s<>"'`]+/gi;
const MAX_TRANSCRIPT_ANCHOR_MEMBER_CHARS = 512;
const MAX_TRANSCRIPT_ANCHOR_JSON_CHARS = 128 * 1024;
const COLLAPSIBLE_COORDINATION_TOOLS = new Set([
  "followup_task", "list_agents", "send_message", "wait_agent",
]);
const INTERNAL_TOOL_ALIASES = new Map([
  ["exec_command", "exec"],
  ["exec", "exec"],
]);
const BENIGN_COORDINATION_STATUSES = new Set([
  "ok", "sent", "queued", "delivered", "acknowledged", "waiting", "idle", "running",
  "complete", "completed", "success", "succeeded", "timed out", "timeout", "no update",
  "no updates", "no activity",
]);
const LAUNCH_DIRECTORY_STORAGE_KEY = "atmux.launch-directories";
const FILE_READER_STORAGE_KEY = "atmux.file-reader-preferences";
const CONVERSATION_VISIBILITY_STORAGE_KEY = "atmux.conversation-visibility";
const COMPOSER_DRAFT_STORAGE_KEY = "atmux.composer-drafts.v1";
const NAVIGATION_STORAGE_KEY = "atmux.navigation.v1";
const MAX_NAVIGATION_PREFERENCES = 256;
const MAX_COMPOSER_DRAFT_ENTRIES = 64;
const MAX_COMPOSER_DRAFT_TOMBSTONES = 256;
const MAX_COMPOSER_DRAFT_STORAGE_CHARS = 512 * 1024;
const MAX_COMPOSER_DRAFT_TEXT_CHARS = 65_536;
const COMPOSER_DRAFT_TOMBSTONE_TTL_MS = 7 * 24 * 60 * 60 * 1000;
const PANE_INSTANCE_PATTERN = /^pane-v1-[a-f0-9]{64}$/;
const MACHINE_ID_PATTERN = /^[a-z0-9][a-z0-9_-]{0,31}$/;
const PANE_SPECIAL_KEY_ACTIONS = new Set([
  "up", "down", "left", "right", "enter", "tmux_prefix_twice",
]);
const MAX_QUEUED_PANE_KEYS = 16;
const MAX_PANE_KEY_STATUSES = 64;
const PERSISTENT_COMPOSER_DRAFT_KEY_PATTERN = /^pane:([A-Za-z0-9_.%~-]{1,96}):(pane-v1-[a-f0-9]{64})$/;

const FILE_READER_SIZES = new Set(["small", "medium", "large"]);
const SUPPORTED_IMAGE_TYPES = new Set(["image/png", "image/jpeg"]);
const COMPOSITE_SEPARATOR = "~";
const PULSE_REFRESH_BASE_MS = 60_000;
const PULSE_REFRESH_MAX_MS = 5 * 60_000;
const PULSE_INVALIDATION_DEBOUNCE_MS = 100;
const PULSE_RECONNECT_BASE_MS = 1_000;
const PULSE_RECONNECT_MAX_MS = 30_000;
const PULSE_MAX_PAGES = 4;
const PULSE_PAGE_LIMIT = 100;
const PULSE_RESOURCES = new Set([
  "usage", "pace", "context", "gemini", "reports", "profiles", "alerts",
  "alert-subscriptions", "pricing", "limits", "machines",
  "ingest-tokens",
  "health",
  "poll",
]);
const PULSE_QUERY_KEYS = new Set([
  "acknowledged", "cursor", "days", "drill", "granularity", "limit", "machine",
  "profile", "through_day",
]);

function pulseAccountId(value) {
  const text = String(value ?? "").trim();
  if (!/^[1-9]\d*$/.test(text)) return null;
  const account = Number(text);
  return Number.isSafeInteger(account) ? account : null;
}

function pulseAccounts(value) {
  if (!Array.isArray(value) || value.length > 32) return [];
  const seen = new Set();
  const accounts = [];
  for (const item of value) {
    const id = pulseAccountId(item?.id);
    const identity = typeof item?.identity === "string" ? item.identity.trim() : "";
    const displayName = typeof item?.display_name === "string" ? item.display_name.trim() : "";
    if (!id || !identity || identity.length > 320 || displayName.length > 320 || seen.has(id)) continue;
    seen.add(id);
    accounts.push({ id, identity, display_name: displayName || null });
  }
  return accounts;
}

function pulseAccountLabel(account) {
  if (!account) return "Pulse account";
  return account.display_name || account.identity || `Account ${account.id}`;
}

function preferredPulseAccount(accounts, requested, remembered) {
  const ids = new Set((accounts || []).map((account) => pulseAccountId(account?.id)).filter(Boolean));
  for (const candidate of [requested, remembered]) {
    const id = pulseAccountId(candidate);
    if (id && ids.has(id)) return id;
  }
  return ids.values().next().value || null;
}

function pulseRefreshDelay(failures) {
  const exponent = Math.min(Math.max(Number(failures) || 0, 0), 3);
  return Math.min(PULSE_REFRESH_BASE_MS * (2 ** exponent), PULSE_REFRESH_MAX_MS);
}

function pulseAccountPath(account, resource, query = {}) {
  const id = pulseAccountId(account);
  if (!id || !PULSE_RESOURCES.has(resource)) return null;
  const params = new URLSearchParams();
  for (const [key, value] of Object.entries(query)) {
    if (!PULSE_QUERY_KEYS.has(key) || value === null || value === undefined || value === "") continue;
    params.set(key, String(value));
  }
  const suffix = params.toString();
  return `/api/v1/pulse/accounts/${id}/${resource}${suffix ? `?${suffix}` : ""}`;
}

function pulseProfileVisibilityPath(account, profile) {
  const id = pulseAccountId(account);
  const name = String(profile ?? "");
  if (!id || !name || name.length > 128) return null;
  return `/api/v1/pulse/accounts/${id}/profiles/${encodeURIComponent(name)}/visibility`;
}

function pulseProfileSettingsPath(account, profile) {
  const id = pulseAccountId(account);
  const name = String(profile ?? "");
  if (!id || !name || name.length > 128) return null;
  return `/api/v1/pulse/accounts/${id}/profiles/${encodeURIComponent(name)}/settings`;
}

function pulseForcePollPath(account) {
  return pulseAccountPath(account, "poll");
}

function pulseEventsPath(account) {
  const id = pulseAccountId(account);
  return id ? `/api/v1/pulse/accounts/${id}/events` : null;
}

function pulseRevisionId(value) {
  const text = String(value ?? "").trim();
  if (!/^(?:0|[1-9]\d{0,19})$/.test(text)) return null;
  if (text.length === 20 && text > "18446744073709551615") return null;
  return text;
}

function comparePulseRevisions(left, right) {
  if (left.length !== right.length) return left.length < right.length ? -1 : 1;
  if (left === right) return 0;
  return left < right ? -1 : 1;
}

/// Every stream's first event is authoritative, even when it repeats the last
/// revision observed before reconnect. Later duplicate/out-of-order events do
/// not amplify requests, while a gap still causes one full account refresh.
function pulseInvalidationAction(previous, incoming, initial = false) {
  const revision = pulseRevisionId(incoming);
  if (!revision) return "invalid";
  const prior = pulseRevisionId(previous);
  if (initial || !prior) return "refresh";
  return comparePulseRevisions(prior, revision) < 0 ? "refresh" : "ignore";
}

function pulseReconnectDelay(failures) {
  const exponent = Math.min(Math.max(Number(failures) || 0, 0), 5);
  return Math.min(PULSE_RECONNECT_BASE_MS * (2 ** exponent), PULSE_RECONNECT_MAX_MS);
}

function pulseAlertActionPath(account, alertId, action) {
  const id = pulseAccountId(account);
  const event = pulseAccountId(alertId);
  if (!id || !event || !new Set(["acknowledge", "reply"]).has(action)) return null;
  return `/api/v1/pulse/accounts/${id}/alerts/${event}/${action}`;
}

function pulseSubscriptionPath(account, subscriptionId = null) {
  const base = pulseAccountPath(account, "alert-subscriptions");
  if (!base) return null;
  if (subscriptionId === null) return base;
  const id = pulseAccountId(subscriptionId);
  return id ? `${base}/${id}` : null;
}

function pulseIngestTokenPath(account, tokenId = null) {
  const base = pulseAccountPath(account, "ingest-tokens");
  if (!base) return null;
  if (tokenId === null) return base;
  const id = pulseAccountId(tokenId);
  return id ? `${base}/${id}` : null;
}

function pulsePricingPath(account, key = null) {
  const base = pulseAccountPath(account, "pricing");
  if (!base || key === null) return base;
  const stableKey = String(key ?? "");
  if (!/^[A-Za-z0-9_.-]{1,128}$/.test(stableKey)) return null;
  return `${base}/${encodeURIComponent(stableKey)}`;
}

function pulseCanFollowCursor(cursor, pagesLoaded, maxPages = PULSE_MAX_PAGES) {
  return typeof cursor === "string" && cursor.length > 0
    && Number.isInteger(pagesLoaded) && pagesLoaded < maxPages;
}

function pulseRequestStillCurrent(capturedAccount, currentAccount, capturedGeneration, currentGeneration) {
  return capturedAccount === currentAccount && capturedGeneration === currentGeneration;
}

function compareSessions(left, right) {
  const name = left.name.localeCompare(right.name, undefined, { numeric: true, sensitivity: "base" });
  if (name) return name;
  return left.id.localeCompare(right.id);
}

function sortSessions(sessions) {
  return [...sessions].sort(compareSessions);
}

/// Keeps transient quiet samples from making a working agent flash yellow.
/// Work is shown immediately; waiting is shown only after a continuous quiet
/// hold. Other/error-like states remain immediate, and removed sessions are
/// pruned because only the supplied sessions are copied into the next map.
function presentSessionStatuses(previous, sessions, now, holdMs = WORKING_TO_WAITING_HOLD_MS) {
  const prior = previous instanceof Map ? previous : new Map();
  const next = new Map();
  const effective = [];
  let nextDelay = null;
  for (const session of sessions) {
    const raw = session.status;
    const old = prior.get(session.id);
    let shown = raw;
    let waitingSince = null;
    if (raw === "waiting" && old?.shown === "working") {
      waitingSince = old.raw === "waiting" && Number.isFinite(old.waitingSince)
        ? old.waitingSince
        : now;
      const remaining = Math.max(0, holdMs - Math.max(0, now - waitingSince));
      if (remaining > 0) {
        shown = "working";
        nextDelay = nextDelay === null ? remaining : Math.min(nextDelay, remaining);
      }
    }
    next.set(session.id, { shown, raw, waitingSince });
    effective.push(shown === raw ? session : { ...session, status: shown });
  }
  return { presentations: next, sessions: effective, nextDelay };
}

function reconcileSessions(current, update) {
  const next = new Map(current);
  if (Array.isArray(update.sessions)) {
    const present = new Set(update.sessions.map((session) => session.id));
    for (const id of next.keys()) if (!present.has(id)) next.delete(id);
    for (const session of update.sessions) next.set(session.id, session);
    return next;
  }
  for (const id of update.remove || []) next.delete(id);
  for (const session of update.upsert || []) next.set(session.id, session);
  return next;
}

/// Splits `machine~pane` without ever mangling a bare tmux pane id or name.
function parseCompositeId(id) {
  if (typeof id !== "string") return { machine: null, pane: null };
  const index = id.indexOf(COMPOSITE_SEPARATOR);
  if (index <= 0 || index === id.length - 1) return { machine: null, pane: id || null };
  return { machine: id.slice(0, index), pane: id.slice(index + 1) };
}

function sessionMachineId(session, localMachineId = "local") {
  return session.machine || parseCompositeId(session.id).machine || localMachineId;
}

/// Captures the complete immutable target for one pane-key request. The
/// request path binds the pane (and its coordinator machine namespace), while
/// the body independently binds the owning machine and pane generation.
function paneSpecialKeyDelivery(session, action, localMachineId = "local") {
  if (!session || typeof session.id !== "string" || !session.id
      || !PANE_SPECIAL_KEY_ACTIONS.has(action)
      || !PANE_INSTANCE_PATTERN.test(String(session.instance_id || ""))) return null;
  const machine = sessionMachineId(session, localMachineId);
  const compositeMachine = parseCompositeId(session.id).machine;
  if (!MACHINE_ID_PATTERN.test(machine)
      || (compositeMachine && compositeMachine !== machine)) return null;
  return Object.freeze({
    paneId: session.id,
    machine,
    instanceId: session.instance_id,
    action,
  });
}

function paneSpecialKeyTarget(delivery) {
  return delivery
    ? `${delivery.machine}\u0000${delivery.paneId}\u0000${delivery.instanceId}`
    : null;
}

/// Returns an incarnation-safe identity for browser-local composer state.
/// Old owners without `instance_id` still get same-page isolation, but their
/// recyclable pane ids are deliberately never written to persistent storage.
function composerDraftIdentity(session, localMachineId = "local") {
  if (!session || typeof session.id !== "string" || !session.id) return null;
  const instanceId = typeof session.instance_id === "string" ? session.instance_id : "";
  if (PANE_INSTANCE_PATTERN.test(instanceId)) {
    return {
      key: `pane:${encodeURIComponent(sessionMachineId(session, localMachineId))}:${instanceId}`,
      persistent: true,
      instanceId,
    };
  }
  return { key: `ephemeral:${session.id}`, persistent: false };
}

function composerDraftMachine(key) {
  const match = typeof key === "string" ? key.match(PERSISTENT_COMPOSER_DRAFT_KEY_PATTERN) : null;
  if (!match) return null;
  try { return decodeURIComponent(match[1]); } catch { return null; }
}

function composerDraftInstanceId(key) {
  const match = typeof key === "string" ? key.match(PERSISTENT_COMPOSER_DRAFT_KEY_PATTERN) : null;
  return match?.[2] || null;
}

function sessionMatchesComposerIdentity(session, identityKey, localMachineId = "local") {
  if (!PERSISTENT_COMPOSER_DRAFT_KEY_PATTERN.test(String(identityKey || ""))) return false;
  return composerDraftIdentity(session, localMachineId)?.key === identityKey;
}

/// Only owners explicitly reported online in a complete snapshot have an
/// authoritative inventory. Drafts for offline or not-yet-connected owners
/// survive coordinator startup until that owner can report its panes.
function staleComposerDraftKeys(drafts, sessions, machines) {
  const authoritativeMachines = new Set((Array.isArray(machines) ? machines : [])
    .filter((machine) => machine?.online === true && typeof machine.id === "string")
    .map((machine) => machine.id));
  if (!authoritativeMachines.size) return [];
  const live = new Set();
  const currentSessions = sessions instanceof Map ? sessions.values() : sessions || [];
  for (const session of currentSessions) {
    const identity = composerDraftIdentity(session);
    if (identity?.persistent) live.add(identity.key);
  }
  const stale = [];
  for (const key of drafts instanceof Map ? drafts.keys() : []) {
    const machine = composerDraftMachine(key);
    if (machine && authoritativeMachines.has(machine) && !live.has(key)) stale.push(key);
  }
  return stale;
}

function normalizedComposerDraft(entry) {
  if (!entry || typeof entry !== "object" || Array.isArray(entry)
      || typeof entry.text !== "string" || !entry.text
      || entry.text.length > MAX_COMPOSER_DRAFT_TEXT_CHARS) return null;
  const selectionStart = Number.isSafeInteger(entry.selectionStart)
    ? Math.max(0, Math.min(entry.text.length, entry.selectionStart)) : entry.text.length;
  const selectionEnd = Number.isSafeInteger(entry.selectionEnd)
    ? Math.max(selectionStart, Math.min(entry.text.length, entry.selectionEnd)) : selectionStart;
  return {
    text: entry.text,
    selectionStart,
    selectionEnd,
    version: Number.isSafeInteger(entry.version) && entry.version > 0 ? entry.version : 1,
    updatedAt: Number.isSafeInteger(entry.updatedAt) && entry.updatedAt > 0 ? entry.updatedAt : 1,
  };
}

/// Keeps both persistent and legacy in-memory drafts within a fixed live
/// budget. Map insertion order is the LRU order, so pruning is linear and the
/// serializer never has an unbounded collection to sort or stringify.
function pruneComposerDraftEntries(value, protectedKeys = []) {
  const drafts = value instanceof Map ? value : new Map();
  const protectedSet = protectedKeys instanceof Set ? protectedKeys : new Set(protectedKeys || []);
  const sizes = new Map();
  let totalChars = 32;
  for (const [key, entry] of drafts) {
    const draft = normalizedComposerDraft(entry);
    if (typeof key !== "string" || key.length > 192 || !draft) {
      drafts.delete(key);
      continue;
    }
    const size = JSON.stringify({ key, ...draft }).length + 1;
    sizes.set(key, size);
    totalChars += size;
  }
  for (const key of drafts.keys()) {
    if (drafts.size <= MAX_COMPOSER_DRAFT_ENTRIES
        && totalChars <= MAX_COMPOSER_DRAFT_STORAGE_CHARS) break;
    if (protectedSet.has(key)) continue;
    totalChars -= sizes.get(key) || 0;
    drafts.delete(key);
  }
  return drafts;
}

/// Parses a bounded array representation instead of an object keyed by
/// attacker-controlled strings. Draft text remains plain textarea data.
function composerDraftEntries(value) {
  const drafts = new Map();
  let parsed = value;
  if (typeof value === "string") {
    if (value.length > MAX_COMPOSER_DRAFT_STORAGE_CHARS) return drafts;
    try { parsed = JSON.parse(value); } catch { return drafts; }
  }
  if (!parsed || typeof parsed !== "object" || parsed.version !== 1
      || !Array.isArray(parsed.drafts)) return drafts;
  for (const item of parsed.drafts.slice(-MAX_COMPOSER_DRAFT_ENTRIES)) {
    if (!item || typeof item.key !== "string" || item.key.length > 192
        || !PERSISTENT_COMPOSER_DRAFT_KEY_PATTERN.test(item.key)) continue;
    const draft = normalizedComposerDraft(item);
    if (draft) drafts.set(item.key, draft);
  }
  return pruneComposerDraftEntries(drafts);
}

function composerDraftTombstones(value, now = Date.now()) {
  const tombstones = new Map();
  let parsed = value;
  if (typeof value === "string") {
    if (value.length > MAX_COMPOSER_DRAFT_STORAGE_CHARS) return tombstones;
    try { parsed = JSON.parse(value); } catch { return tombstones; }
  }
  if (!parsed || typeof parsed !== "object" || parsed.version !== 1
      || !Array.isArray(parsed.tombstones)) return tombstones;
  const cutoff = now - COMPOSER_DRAFT_TOMBSTONE_TTL_MS;
  for (const item of parsed.tombstones.slice(-MAX_COMPOSER_DRAFT_TOMBSTONES)) {
    if (!item || typeof item.key !== "string"
        || !PERSISTENT_COMPOSER_DRAFT_KEY_PATTERN.test(item.key)
        || !Number.isSafeInteger(item.deletedAt) || item.deletedAt <= cutoff) continue;
    const prior = tombstones.get(item.key);
    if (!prior || item.deletedAt > prior.deletedAt) {
      tombstones.delete(item.key);
      tombstones.set(item.key, { deletedAt: item.deletedAt });
    }
  }
  return tombstones;
}

function composerDraftIsNewer(candidate, prior) {
  if (!prior || candidate.updatedAt !== prior.updatedAt) {
    return !prior || candidate.updatedAt > prior.updatedAt;
  }
  return JSON.stringify([
    candidate.version, candidate.text, candidate.selectionStart, candidate.selectionEnd,
  ]) > JSON.stringify([
    prior.version, prior.text, prior.selectionStart, prior.selectionEnd,
  ]);
}

/// Merges a storage snapshot into this tab's live state by per-entry clocks.
/// Tombstones win ties so a stale tab cannot revive a successfully submitted
/// or deleted pane draft with a whole-map last-writer-wins update.
function mergeComposerDraftState(drafts, tombstones, incoming, now = Date.now(), protectedKeys = []) {
  const localDrafts = drafts instanceof Map ? drafts : new Map();
  const localTombstones = tombstones instanceof Map ? tombstones : new Map();
  const cutoff = now - COMPOSER_DRAFT_TOMBSTONE_TTL_MS;
  for (const [key, tombstone] of localTombstones) {
    if (!Number.isSafeInteger(tombstone?.deletedAt) || tombstone.deletedAt <= cutoff) {
      localTombstones.delete(key);
    }
  }
  for (const [key, tombstone] of composerDraftTombstones(incoming, now)) {
    const prior = localTombstones.get(key);
    if (!prior || tombstone.deletedAt > prior.deletedAt) {
      localTombstones.delete(key);
      localTombstones.set(key, tombstone);
    }
  }
  for (const [key, draft] of composerDraftEntries(incoming)) {
    const deletedAt = localTombstones.get(key)?.deletedAt || 0;
    const prior = localDrafts.get(key);
    if (draft.updatedAt > deletedAt && composerDraftIsNewer(draft, prior)) {
      localDrafts.delete(key);
      localDrafts.set(key, draft);
    }
  }
  for (const [key, tombstone] of localTombstones) {
    if ((localDrafts.get(key)?.updatedAt || 0) <= tombstone.deletedAt) localDrafts.delete(key);
    else localTombstones.delete(key);
  }
  while (localTombstones.size > MAX_COMPOSER_DRAFT_TOMBSTONES) {
    localTombstones.delete(localTombstones.keys().next().value);
  }
  pruneComposerDraftEntries(localDrafts, protectedKeys);
  return { drafts: localDrafts, tombstones: localTombstones };
}

function composerDraftJson(value, protectedKeys = [], tombstoneValue = new Map()) {
  const candidates = [...pruneComposerDraftEntries(value, protectedKeys)]
    .filter(([key, draft]) => typeof key === "string"
      && PERSISTENT_COMPOSER_DRAFT_KEY_PATTERN.test(key)
      && normalizedComposerDraft(draft))
    .map(([key, draft]) => ({ key, ...normalizedComposerDraft(draft) }))
    .slice(-MAX_COMPOSER_DRAFT_ENTRIES);
  const tombstones = [...(tombstoneValue instanceof Map ? tombstoneValue : new Map())]
    .filter(([key, tombstone]) => PERSISTENT_COMPOSER_DRAFT_KEY_PATTERN.test(key)
      && Number.isSafeInteger(tombstone?.deletedAt) && tombstone.deletedAt > 0)
    .slice(-MAX_COMPOSER_DRAFT_TOMBSTONES)
    .map(([key, tombstone]) => ({ key, deletedAt: tombstone.deletedAt }));
  while (candidates.length || tombstones.length) {
    const encoded = JSON.stringify({ version: 1, drafts: candidates, tombstones });
    if (encoded.length <= MAX_COMPOSER_DRAFT_STORAGE_CHARS) return encoded;
    // In-flight drafts stay pinned in memory for rollback, but browser storage
    // has a non-negotiable hard cap even when escaping expands every entry.
    if (candidates.length) candidates.shift();
    else tombstones.shift();
  }
  return JSON.stringify({ version: 1, drafts: [], tombstones: [] });
}

function composerDraftCanClear(draft, submission) {
  return Boolean(draft && submission)
    && draft.version === submission.draftVersion
    && draft.text === submission.message;
}

/// A launch needs both an owner-configured profile and a project root. The
/// latter is also the capability that makes bounded folder browsing possible.
function isLaunchCapableMachine(machine) {
  const profiles = Array.isArray(machine?.profiles) ? machine.profiles : [];
  const directories = Array.isArray(machine?.directories) ? machine.directories : [];
  return machine?.online === true
    && harnessesForProfiles(profiles).length > 0
    && directories.some(validRememberedLaunchDirectory);
}

/// Chooses the launch target from the current navigation context when that
/// owner is online and launch-capable, otherwise using the first owner that is.
/// A bare pre-federation pane id belongs to the coordinator identified by the
/// overview, rather than an assumed machine literally named `local`.
function preferredLaunchMachineId(
  machines,
  selectedMachineId,
  selectedSession,
  localMachineId = "local",
) {
  const available = Array.isArray(machines) ? machines : [];
  const contextualId = selectedMachineId
    || (selectedSession ? sessionMachineId(selectedSession, localMachineId) : null);
  const contextual = available.find((machine) => machine?.id === contextualId);
  if (isLaunchCapableMachine(contextual)) return contextual.id;
  return available.find(isLaunchCapableMachine)?.id || null;
}

/// Groups sessions under their owning machine, preserving the server's machine
/// order (this machine first) and appending any machine the overview omitted.
function groupSessionsByMachine(sessions, machines) {
  const known = Array.isArray(machines) ? machines : [];
  const localMachineId = known.find((machine) => machine?.kind === "local")?.id || "local";
  const groups = new Map();
  for (const machine of known) {
    groups.set(machine.id, { machine, sessions: [] });
  }
  for (const session of sortSessions(sessions)) {
    const id = sessionMachineId(session, localMachineId);
    if (!groups.has(id)) {
      groups.set(id, { machine: { id, label: id, kind: "remote", online: true }, sessions: [] });
    }
    groups.get(id).sessions.push(session);
  }
  return [...groups.values()];
}

/// Favorites belong to an actual pane incarnation, never a reusable tmux id.
function favoriteSessionKey(session, localMachineId = "local") {
  if (!session || !MACHINE_ID_PATTERN.test(sessionMachineId(session, localMachineId))) return null;
  const identity = composerDraftIdentity(session, localMachineId);
  return identity?.persistent ? identity.key : null;
}

function navigationPreferences(raw) {
  let value = raw;
  if (typeof raw === "string") {
    if (raw.length > 64 * 1024) return { collapsed: [], favorites: [] };
    try { value = JSON.parse(raw); } catch { value = null; }
  }
  const bounded = (items, valid) => [...new Set((Array.isArray(items) ? items : [])
    .filter((item) => typeof item === "string" && valid(item)))].slice(-MAX_NAVIGATION_PREFERENCES);
  return {
    collapsed: bounded(value?.collapsed, (item) => MACHINE_ID_PATTERN.test(item)),
    favorites: bounded(value?.favorites, (item) =>
      PERSISTENT_COMPOSER_DRAFT_KEY_PATTERN.test(item)
      && MACHINE_ID_PATTERN.test(composerDraftMachine(item) || "")),
  };
}

/// Filters reveal matching children without changing a saved collapsed node.
/// Pinning changes order only within that child's owning machine.
function navigationView(sessions, machines, options = {}) {
  const query = String(options.query || "").trim().toLowerCase();
  const status = ["working", "waiting", "other"].includes(options.status) ? options.status : "";
  const harness = ["claude", "codex", "other"].includes(options.harness) ? options.harness : "";
  const filtering = Boolean(query || status || harness);
  const localMachineId = machines.find((machine) => machine.kind === "local")?.id || "local";
  const machineLabels = new Map(machines.map((machine) => [machine.id, machine.label || machine.id]));
  const favorites = new Set(options.favorites || []);
  const collapsed = new Set(options.collapsed || []);
  const visible = sessions.filter((session) => {
    const machineId = sessionMachineId(session, localMachineId);
    const searchable = [session.name, session.description, session.path, session.agent, session.profile, machineId, machineLabels.get(machineId)]
      .filter(Boolean).join(" ").toLowerCase();
    return (!query || searchable.includes(query))
      && (!status || session.status === status)
      && (!harness || session.agent === harness);
  });
  const groups = groupSessionsByMachine(visible, machines)
    .filter((group) => group.sessions.length > 0 || !filtering)
    .map((group) => ({
      ...group,
      filtering,
      collapsed: !filtering && collapsed.has(group.machine.id),
      sessions: group.sessions.sort((left, right) =>
        Number(favorites.has(favoriteSessionKey(right, localMachineId)))
        - Number(favorites.has(favoriteSessionKey(left, localMachineId)))),
    }));
  return { groups, filtering, matchedCount: visible.length, totalCount: sessions.length };
}

function formatRelativeTime(timestamp, now) {
  if (!Number.isFinite(timestamp) || !Number.isFinite(now)) return "";
  const seconds = Math.max(0, Math.round((now - timestamp) / 1000));
  if (seconds < 60) return `${seconds}s ago`;
  if (seconds < 3600) return `${Math.floor(seconds / 60)}m ago`;
  if (seconds < 86400) return `${Math.floor(seconds / 3600)}h ago`;
  return `${Math.floor(seconds / 86400)}d ago`;
}

/// One line of machine health for the rail header.
function machineStatusLabel(machine, now) {
  if (!machine) return "";
  const count = `${machine.sessions ?? 0} agent${(machine.sessions ?? 0) === 1 ? "" : "s"}`;
  if (machine.online) {
    return machine.health ? `${count} · ${machine.health}` : count;
  }
  const seen = formatRelativeTime(machine.last_seen_ms, now);
  const detail = machine.health ? ` · ${machine.health}` : "";
  return seen ? `Offline · last seen ${seen}${detail}` : `Offline${detail}`;
}

function isMachineControllable(machine) {
  return !machine || machine.online !== false;
}

/// Poll cadence for the fleet update roster. A machine that is mid-update is
/// the only thing worth watching closely; everything else is background news.
const UPDATE_POLL_ACTIVE_MS = 5000;
const UPDATE_POLL_IDLE_MS = 60000;
/// Node states that mean work is under way on that machine right now.
const UPDATE_BUSY_STATES = new Set([
  "checking", "downloading", "verifying", "applying", "restarting",
]);

function machineUpdateInFlight(entry) {
  return UPDATE_BUSY_STATES.has(entry?.update?.state);
}

/// How long to wait before reading the fleet roster again.
function fleetUpdatePollDelay(entries) {
  const roster = Array.isArray(entries) ? entries : [];
  return roster.some(machineUpdateInFlight) ? UPDATE_POLL_ACTIVE_MS : UPDATE_POLL_IDLE_MS;
}

/// A machine may install a release only when it owns its own executable and
/// the node itself reported a verified newer one. An unverified release is
/// never offered, however new it claims to be.
function machineCanUpdate(entry) {
  const update = entry?.update;
  return Boolean(update
    && update.mode === "self"
    && update.latest?.verified
    && !machineUpdateInFlight(entry));
}

function machineCanRollback(entry) {
  const update = entry?.update;
  return Boolean(update && update.mode === "self" && update.previous && !machineUpdateInFlight(entry));
}

function machineCanCheck(entry) {
  return Boolean(entry?.update?.mode === "self" && !machineUpdateInFlight(entry));
}

/// The compact landing-page marker, empty when there is nothing to install.
function machineUpdatePill(entry) {
  return machineCanUpdate(entry) ? `\u2191 v${entry.update.latest.version}` : "";
}

function updatableMachines(entries) {
  return (Array.isArray(entries) ? entries : []).filter(machineCanUpdate);
}

/// Poll cadence for the fleet Quick Resume roster: quick only while a
/// machine's recovery script is running.
const RECOVERY_POLL_ACTIVE_MS = 2000;
const RECOVERY_POLL_IDLE_MS = 60000;

/// Machines whose owning node answered the Quick Resume read with its own
/// document. A node with no roster still answers (as unavailable, with the
/// reason), so the dialog can say why; an offline machine has no document.
function recoveryMachines(entries) {
  return (Array.isArray(entries) ? entries : [])
    .filter((entry) => entry && entry.recovery && typeof entry.recovery === "object");
}

function recoveryInFlight(entries) {
  return recoveryMachines(entries).some((entry) => entry.recovery.phase === "running");
}

function recoveryPollDelay(entries) {
  return recoveryInFlight(entries) ? RECOVERY_POLL_ACTIVE_MS : RECOVERY_POLL_IDLE_MS;
}

/// One dialog row. The owner's own document decides whether the button is
/// enabled; the browser never chooses a script, path, or command.
function recoveryRowState(entry, busy) {
  const recovery = entry?.recovery && typeof entry.recovery === "object" ? entry.recovery : null;
  const running = recovery?.phase === "running";
  return {
    id: entry?.id || "",
    label: entry?.label || entry?.id || "machine",
    message: entry?.error || recovery?.message || "",
    running,
    canStart: recovery?.available === true && !running && !busy && entry?.online !== false,
    action: running ? "Resuming\u2026" : "Resume missing sessions",
  };
}

/// The one thing an operator needs to know before pressing Update.
function updateRestartWarning(labels) {
  const names = (Array.isArray(labels) ? labels : [labels]).filter(Boolean);
  const subject = names.length ? names.join(", ") : "this machine";
  return `atmux restarts on ${subject}; agent sessions keep running in tmux.`;
}

/// What the confirmation says for one verb.
///
/// Rolling back restarts the node exactly as installing does, so it is
/// confirmed the same way rather than firing on a single tap.
function updateConfirmCopy(action, labels) {
  const names = (Array.isArray(labels) ? labels : [labels]).filter(Boolean);
  const subject = names.length === 1 ? names[0] : `${names.length} machines`;
  const note = updateRestartWarning(names);
  return action === "rollback"
    ? {
      title: "Roll back atmux?",
      target: `Restore the previously installed atmux on ${subject}.`,
      note,
      confirm: "Roll back",
    }
    : {
      title: "Install the new atmux?",
      target: `Install the newest verified atmux on ${subject}.`,
      note,
      confirm: "Update",
    };
}

function updateProgressLabel(progress) {
  if (!progress || !Number.isFinite(progress.downloaded)) return "";
  const done = formatBytes(progress.downloaded);
  return Number.isFinite(progress.total) && progress.total > 0
    ? `${done} of ${formatBytes(progress.total)}`
    : done;
}

const UPDATE_STATE_LABELS = {
  checking: "Checking for a new release\u2026",
  downloading: "Downloading\u2026",
  verifying: "Verifying the signature and checksum\u2026",
  applying: "Installing\u2026",
  restarting: "Restarting into the new version\u2026",
  failed: "The last update attempt failed",
};

/// Everything the Software card shows, as text the renderer only has to place.
///
/// The node decides what it can do; this only reads the document it published,
/// so a coordinator can never offer an action the owner would refuse.
function softwareCardModel(entry, now = Date.now()) {
  const update = entry?.update || null;
  if (!update) {
    return {
      version: "Software state unavailable",
      latest: entry?.error || "This machine did not report its software state.",
      state: "",
      error: null,
      canCheck: false,
      canUpdate: false,
      canRollback: false,
    };
  }
  let latest;
  if (update.mode === "managed_externally") latest = "Managed by container image";
  else if (update.mode === "disabled") latest = "Self-update disabled on this machine";
  else if (update.latest) {
    const published = formatRelativeTime(Date.parse(update.latest.published_at), now);
    latest = [
      `v${update.latest.version} available`,
      update.latest.verified ? "verified" : "unverified",
      published ? `published ${published}` : "",
    ].filter(Boolean).join(" \u00b7 ");
  } else latest = "Up to date";
  const stateLabel = UPDATE_STATE_LABELS[update.state] || "";
  const progress = update.state === "downloading" ? updateProgressLabel(update.progress) : "";
  return {
    version: `atmux v${update.version} \u00b7 ${update.target}`,
    latest,
    state: [stateLabel, progress].filter(Boolean).join(" "),
    error: update.last_error || entry?.error || null,
    canCheck: machineCanCheck(entry),
    canUpdate: machineCanUpdate(entry),
    canRollback: machineCanRollback(entry),
  };
}

function contentToLines(content) {
  return typeof content === "string" && content.length > 0 ? content.split("\n") : [];
}

function utf8ByteLength(value) {
  return new TextEncoder().encode(value).byteLength;
}

function messageFitsByteLimit(value) {
  return utf8ByteLength(value) <= MAX_MESSAGE_BYTES;
}

function validateImageSelection(files, existing = []) {
  const candidates = Array.from(files || []);
  const current = Array.from(existing || []);
  if (!candidates.length) return { files: [], error: "Choose a PNG or JPEG image" };
  if (current.length + candidates.length > MAX_IMAGE_ATTACHMENTS) {
    return { files: [], error: `Attach at most ${MAX_IMAGE_ATTACHMENTS} images` };
  }
  let total = current.reduce((sum, item) => sum + Number(item?.file?.size || item?.size || 0), 0);
  for (const file of candidates) {
    if (!SUPPORTED_IMAGE_TYPES.has(file?.type)) {
      return { files: [], error: "Images must be PNG or JPEG" };
    }
    if (!Number.isFinite(file.size) || file.size <= 0 || file.size > MAX_IMAGE_BYTES) {
      return { files: [], error: "Each image must be 4 MiB or smaller" };
    }
    total += file.size;
    if (total > MAX_TOTAL_IMAGE_BYTES) {
      return { files: [], error: "Combined images must be 12 MiB or smaller" };
    }
  }
  return { files: candidates, error: null };
}

function attachmentDeliveryTarget(capturedPaneId, selectedPaneId) {
  return capturedPaneId || selectedPaneId || null;
}

function attachmentSelectionMatches(capturedPaneId, capturedInstanceKey, selectedPaneId, selectedInstanceKey) {
  return Boolean(capturedPaneId && capturedInstanceKey)
    && PERSISTENT_COMPOSER_DRAFT_KEY_PATTERN.test(capturedInstanceKey)
    && capturedPaneId === selectedPaneId
    && capturedInstanceKey === selectedInstanceKey;
}

function remainingAttachmentsAfterDelivery(current, delivered) {
  const sent = new Set(delivered || []);
  return Array.from(current || []).filter((attachment) => !sent.has(attachment));
}

function imageFilesFromTransfer(transfer) {
  const direct = Array.from(transfer?.files || [])
    .filter((file) => typeof file?.type === "string" && file.type.startsWith("image/"));
  if (direct.length) return direct;
  return Array.from(transfer?.items || [])
    .filter((item) => item?.kind === "file" && item.type?.startsWith("image/"))
    .map((item) => item.getAsFile?.())
    .filter(Boolean);
}

function arrayBufferToBase64(buffer) {
  const bytes = new Uint8Array(buffer);
  let binary = "";
  const chunkSize = 32 * 1024;
  for (let offset = 0; offset < bytes.length; offset += chunkSize) {
    binary += String.fromCharCode(...bytes.subarray(offset, offset + chunkSize));
  }
  return btoa(binary);
}

function composerEnterAction(event) {
  if (!event || event.key !== "Enter" || event.isComposing || event.altKey) return null;
  return event.ctrlKey || event.metaKey || event.shiftKey ? "newline" : "send";
}

/// Returns text that should move from a focused live pane into the composer.
/// Keyboard shortcuts and non-text keys deliberately remain with the page.
function paneTypingText(event) {
  if (!event || event.isComposing || event.ctrlKey || event.metaKey || event.altKey) return "";
  const key = event.key;
  return typeof key === "string" && Array.from(key).length === 1 ? key : "";
}

/// Moves through a chronological message history. `history.length` is the
/// draft position after the newest message, and `null` means do not consume
/// the key because there is no history move to make.
function moveMessageHistory(history, index, direction) {
  const entries = Array.isArray(history) ? history : [];
  if (!entries.length || (direction !== "up" && direction !== "down")) return null;
  const current = Number.isInteger(index)
    ? Math.max(0, Math.min(index, entries.length))
    : entries.length;
  if (direction === "up") return Math.max(0, current - 1);
  return current < entries.length ? current + 1 : null;
}

/// Keep arrows available for editing multiline drafts, but let consecutive
/// history keys traverse recalled messages regardless of the caret position.
function messageHistoryDirection(event, {
  value = "", selectionStart = value.length, selectionEnd = selectionStart,
  browsing = false, fromPane = false,
} = {}) {
  if (!event || event.isComposing || event.ctrlKey || event.metaKey || event.altKey || event.shiftKey) return null;
  const direction = event.key === "ArrowUp" ? "up" : event.key === "ArrowDown" ? "down" : null;
  if (!direction) return null;
  if (!fromPane) {
    if (selectionStart !== selectionEnd) return null;
    if (!browsing) {
      if (direction === "up" && value.slice(0, selectionStart).includes("\n")) return null;
      if (direction === "down" && value.slice(selectionEnd).includes("\n")) return null;
    }
  }
  return direction;
}

function filterDirectories(directories, query, limit = MAX_LAUNCH_DIRECTORY_SUGGESTIONS) {
  const normalized = typeof query === "string" ? query.trim().toLowerCase() : "";
  const boundedLimit = Math.max(0, Math.min(
    Number.isSafeInteger(limit) ? limit : MAX_LAUNCH_DIRECTORY_SUGGESTIONS,
    MAX_LAUNCH_DIRECTORY_SUGGESTIONS,
  ));
  const matches = [];
  if (!boundedLimit) return matches;
  for (const directory of Array.isArray(directories) ? directories : []) {
    if (!normalized
        || `${directory} ${projectLabel(directory)}`.toLowerCase().includes(normalized)) {
      matches.push(directory);
      if (matches.length === boundedLimit) break;
    }
  }
  return matches;
}

function isManualDirectory(value) {
  const directory = typeof value === "string" ? value.trim() : "";
  return directory.startsWith("/") || directory === "~" || directory.startsWith("~/");
}

function validRememberedLaunchDirectory(value) {
  const directory = typeof value === "string" ? value.trim() : "";
  return directory.length <= 4096
    && !/[\u0000-\u001f\u007f]/.test(directory)
    && isManualDirectory(directory);
}

function rememberedLaunchDirectories(value) {
  let parsed = value;
  if (typeof value === "string") {
    try { parsed = JSON.parse(value); } catch { return {}; }
  }
  if (!parsed || typeof parsed !== "object" || Array.isArray(parsed)) return {};
  const remembered = {};
  for (const [machine, directories] of Object.entries(parsed)) {
    if (!/^[A-Za-z0-9._-]{1,64}$/.test(machine) || !Array.isArray(directories)) continue;
    const unique = [];
    for (const directory of directories) {
      if (!validRememberedLaunchDirectory(directory) || unique.includes(directory.trim())) continue;
      unique.push(directory.trim());
      if (unique.length === MAX_REMEMBERED_LAUNCH_DIRECTORIES) break;
    }
    if (unique.length) remembered[machine] = unique;
  }
  return remembered;
}

function rememberLaunchDirectory(remembered, machine, directory) {
  const current = rememberedLaunchDirectories(remembered);
  if (!/^[A-Za-z0-9._-]{1,64}$/.test(String(machine || ""))
      || !validRememberedLaunchDirectory(directory)) return current;
  const normalized = directory.trim();
  current[machine] = [normalized, ...(current[machine] || []).filter((item) => item !== normalized)]
    .slice(0, MAX_REMEMBERED_LAUNCH_DIRECTORIES);
  return current;
}

function availableLaunchDirectories(machine, remembered) {
  const listed = Array.isArray(machine?.directories) ? machine.directories : [];
  const saved = remembered?.[machine?.id] || [];
  const directories = [];
  const seen = new Set();
  let inspected = 0;
  candidateSources: for (const source of [saved, listed]) {
    for (const directory of source) {
      inspected += 1;
      if (inspected > MAX_LAUNCH_DIRECTORY_CANDIDATES * 4) break candidateSources;
      if (!validRememberedLaunchDirectory(directory) || seen.has(directory)) continue;
      seen.add(directory);
      directories.push(directory);
      if (directories.length === MAX_LAUNCH_DIRECTORY_CANDIDATES) break candidateSources;
    }
  }
  return directories;
}

function launchDirectoryBrowsePath(machine, path) {
  if (!/^[A-Za-z0-9._-]{1,64}$/.test(String(machine || ""))) return null;
  const params = new URLSearchParams({ machine: String(machine) });
  if (path !== null && path !== undefined && path !== "") {
    if (!validRememberedLaunchDirectory(path)) return null;
    params.set("path", path.trim());
  }
  return `/api/v1/launch-directories?${params}`;
}

function validLaunchChildName(value) {
  const name = typeof value === "string" ? value.trim() : "";
  return name.length > 0
    && new TextEncoder().encode(name).length <= 240
    && !name.startsWith("-")
    && !/[\/\\\u0000-\u001f\u007f]/.test(name)
    && name !== "."
    && name !== "..";
}

function repositoryDestinationName(value) {
  const repository = typeof value === "string" ? value.trim() : "";
  const withoutSuffix = repository.split(/[?#]/, 1)[0].replace(/\/+$/, "");
  const segment = withoutSuffix.split(/[/:]/).pop()?.replace(/\.git$/, "").trim() || "";
  return validLaunchChildName(segment) ? segment : "";
}

function harnessesForProfiles(profiles) {
  const seen = new Set();
  return (Array.isArray(profiles) ? profiles : [])
    .map((profile) => profile?.harness)
    .filter((harness) => {
      if (typeof harness !== "string" || !harness) return false;
      const key = harness.toLowerCase();
      if (seen.has(key)) return false;
      seen.add(key);
      return true;
    });
}

function profilesForHarness(profiles, harness) {
  return (Array.isArray(profiles) ? profiles : [])
    .filter((profile) => profile?.harness?.toLowerCase() === String(harness || "").toLowerCase());
}

function projectPreference(machine, directory) {
  const preferences = machine?.project_preferences;
  if (!preferences || typeof preferences !== "object") return {};
  const preference = preferences[directory];
  return preference && typeof preference === "object" ? preference : {};
}

function projectLabel(directory) {
  const parts = String(directory || "").split("/").filter(Boolean);
  return parts.slice(-2).join(" / ") || String(directory || "Project");
}

function sessionFolderLabel(session) {
  const directory = String(session?.path || "").trim();
  return directory ? projectLabel(directory) : "";
}

function sessionProfileLabel(session) {
  const profile = String(session?.profile || "").trim();
  return profile && profile.toLowerCase() !== "default" ? profile : "";
}

function suggestedSessionName(directory, preference = {}) {
  const saved = typeof preference.session_name === "string" ? preference.session_name.trim() : "";
  const leaf = saved || String(directory || "").split("/").filter(Boolean).pop() || "agent";
  return leaf.toLowerCase().replace(/[^a-z0-9_-]+/g, "-").replace(/^-|-$/g, "") || "agent";
}

/// Gives a duplicate its own tmux identity while retaining a recognizable
/// relationship to the source. Names are unique only within the owning tmux
/// server, so sessions on other machines do not consume suffixes.
function duplicateSessionName(session, sessions) {
  const machine = sessionMachineId(session);
  const source = String(session?.name || "agent")
    .replace(/[^A-Za-z0-9_-]+/g, "-")
    .replace(/^-+|-+$/g, "") || "agent";
  const used = new Set((Array.isArray(sessions) ? sessions : [])
    .filter((candidate) => sessionMachineId(candidate) === machine)
    .map((candidate) => String(candidate?.name || "")));
  for (let number = 1; number <= used.size + 2; number += 1) {
    const suffix = number === 1 ? "-copy" : `-copy-${number}`;
    const candidate = `${source.slice(0, 100 - suffix.length)}${suffix}`;
    if (!used.has(candidate)) return candidate;
  }
  // The loop has more candidates than the finite used-name set, so this is
  // unreachable unless the uniqueness relation above changes.
  throw new Error("Could not choose a unique duplicate session name");
}

function launchMachines(options) {
  return Array.isArray(options?.machines) && options.machines.length
    ? options.machines
    : [{
      id: "local",
      label: "This machine",
      online: true,
      directories: options?.directories || [],
      profiles: options?.profiles || [],
      project_preferences: options?.project_preferences || {},
      memory: options?.memory || null,
      note: null,
    }];
}

const GIBIBYTE_BYTES = 1024 * 1024 * 1024;

function safeMemoryBytes(value) {
  return Number.isSafeInteger(value) && value > 0 ? value : null;
}

function formatMemoryLimit(bytes) {
  const safe = safeMemoryBytes(bytes);
  if (!safe) return "No cap";
  const gib = safe / GIBIBYTE_BYTES;
  return `${Number.isInteger(gib) ? gib : gib.toFixed(1)} GiB`;
}

/// Returns only bounded owner-advertised choices. This is presentation
/// validation; the owner repeats all checks against current configuration.
function memoryLimitChoices(memory) {
  const advertised = memory !== null && typeof memory === "object";
  const defaultBytes = safeMemoryBytes(memory?.default_bytes);
  const ceiling = safeMemoryBytes(memory?.override_max_bytes);
  const supported = memory?.supported === true && defaultBytes !== null;
  const presets = [...new Set((Array.isArray(memory?.presets_bytes) ? memory.presets_bytes : [])
    .map(safeMemoryBytes)
    .filter((value) => value !== null && ceiling !== null && value <= ceiling))]
    .sort((left, right) => left - right);
  const note = advertised
    ? String(memory?.note || "")
    : "Memory limit is owner managed; this owner does not advertise override support.";
  return { advertised, supported, defaultBytes, ceiling, presets, note };
}

function parseMemoryLimitSelection(memory, selected, customGiB) {
  const choices = memoryLimitChoices(memory);
  if (selected === "") return null;
  if (!choices.supported || choices.ceiling === null) {
    throw new Error("This machine does not allow per-agent memory overrides");
  }
  if (selected === "custom") {
    const gib = Number(customGiB);
    if (!Number.isSafeInteger(gib) || gib < 1) {
      throw new Error("Custom memory must be a whole number of GiB");
    }
    const bytes = gib * GIBIBYTE_BYTES;
    if (!Number.isSafeInteger(bytes) || bytes > choices.ceiling) {
      throw new Error(`Custom memory must be at most ${formatMemoryLimit(choices.ceiling)}`);
    }
    return bytes;
  }
  const bytes = Number(selected);
  if (!Number.isSafeInteger(bytes) || !choices.presets.includes(bytes)) {
    throw new Error("Choose an owner-approved memory limit");
  }
  return bytes;
}

function defaultMemoryLimitLabel(memory) {
  const choices = memoryLimitChoices(memory);
  if (!choices.advertised) return "Default (owner managed)";
  return choices.defaultBytes === null
    ? "Default (no configured cap)"
    : `Default (${formatMemoryLimit(choices.defaultBytes)})`;
}

/// Resolves a running pane back to owner-issued launch IDs. The browser never
/// manufactures a profile or mode from model text: a duplicate either uses an
/// exact configured choice or stops with a useful error.
function duplicateLaunchSelection(options, session, capabilities, sessions = []) {
  if (!session) throw new Error("Select an agent to duplicate");
  const machineId = sessionMachineId(session);
  const machine = launchMachines(options).find((candidate) => candidate?.id === machineId);
  if (!machine) throw new Error(`Machine ${machineId} no longer offers launch settings`);
  if (machine.online === false) throw new Error(`Machine ${machine.label || machineId} is offline`);
  const harness = String(session.agent || "").toLowerCase();
  const profileName = String(session.profile || "").trim();
  const matchingProfiles = (Array.isArray(machine.profiles) ? machine.profiles : [])
    .filter((profile) => String(profile?.harness || "").toLowerCase() === harness)
    .filter((profile) => String(profile?.name || "").toLowerCase() === profileName.toLowerCase());
  if (!profileName || matchingProfiles.length !== 1) {
    throw new Error(`Profile ${profileName || "(unknown)"} is no longer configured on ${machine.label || machineId}`);
  }
  const profile = matchingProfiles[0];
  const modes = Array.isArray(profile.modes) ? profile.modes : [];
  const observedMode = capabilities?.pane_id === session.id
    && typeof capabilities.current_mode === "string"
    ? capabilities.current_mode
    : "";
  let modeId = null;
  if (modes.length) {
    const mode = modes.find((candidate) => candidate?.id === observedMode);
    if (!mode) {
      throw new Error(`The exact model, effort, or fast mode for ${session.name} is no longer configured`);
    }
    modeId = mode.id;
  }
  const directory = String(session.path || "").trim();
  if (!validRememberedLaunchDirectory(directory)) {
    throw new Error(`The project folder for ${session.name} cannot be reused`);
  }
  const observedMemory = session.memory_max_bytes == null
    ? null
    : safeMemoryBytes(session.memory_max_bytes);
  if (session.memory_max_bytes != null && observedMemory === null) {
    throw new Error(`The memory cap for ${session.name} is invalid`);
  }
  if (observedMemory !== null) {
    const memory = memoryLimitChoices(machine.memory);
    const allowed = memory.supported && (observedMemory === memory.defaultBytes
      || (memory.ceiling !== null
        && observedMemory <= memory.ceiling
        && observedMemory % GIBIBYTE_BYTES === 0));
    if (!allowed) {
      throw new Error(`The ${formatMemoryLimit(observedMemory)} cap for ${session.name} is no longer allowed on ${machine.label || machineId}`);
    }
  }
  return {
    machineId,
    directory,
    harness: profile.harness,
    profileId: profile.id,
    modeId,
    memoryMaxBytes: observedMemory,
    name: duplicateSessionName(session, sessions),
  };
}

function duplicateSourceSnapshot(session) {
  if (!session || typeof session.id !== "string") return null;
  return {
    id: session.id,
    machine: sessionMachineId(session),
    path: String(session.path || ""),
    agent: String(session.agent || "").toLowerCase(),
    profile: String(session.profile || ""),
  };
}

function duplicateSourceMatches(snapshot, session) {
  if (!snapshot || !session || session.id !== snapshot.id) return false;
  return sessionMachineId(session) === snapshot.machine
    && String(session.path || "") === snapshot.path
    && String(session.agent || "").toLowerCase() === snapshot.agent
    && String(session.profile || "") === snapshot.profile;
}

/// Phrases the Claude and Codex CLIs print once an account has no usage left.
/// They are hints for a checkbox default only; the launch itself never depends
/// on terminal text.
const USAGE_LIMIT_MARKERS = [
  "usage limit reached",
  "you've reached your usage limit",
  "hit your usage limit",
  "hit your limit",
  "5-hour limit reached",
  "weekly limit reached",
  "out of usage",
  "quota exceeded",
  "rate limit",
];

/// Decides how the duplicate dialog offers "Resume from summary".
///
/// Only a Claude or Codex pane keeps a readable conversation, and only the
/// machine that owns the pane can run the summary within one launch request,
/// so a federated pane is not offered one. The box is pre-checked when the
/// visible pane says that account ran out of usage, which is exactly when
/// swapping credential profiles matters.
function duplicateSummaryState(session, paneLines = [], paneSessionId = null, localMachineId = "local") {
  const harness = String(session?.agent || "").toLowerCase();
  if (!session || !["claude", "codex"].includes(harness)) return { available: false, checked: false };
  if (sessionMachineId(session, localMachineId) !== localMachineId) {
    return { available: false, checked: false };
  }
  const visible = paneSessionId === session.id && Array.isArray(paneLines) ? paneLines : [];
  const text = visible.slice(-40).join("\n").toLowerCase();
  return { available: true, checked: USAGE_LIMIT_MARKERS.some((marker) => text.includes(marker)) };
}

/// Classifies an overview event against the revision this client holds.
///
/// A snapshot is authoritative and always applies. A patch applies only when it
/// continues the exact revision the client has; anything else means the client
/// missed an update and must resynchronize rather than merge into a gap.
function classifyOverviewUpdate(revision, update) {
  if (Array.isArray(update.sessions)) return "snapshot";
  if (!Number.isInteger(update.base_revision) || update.base_revision !== revision) return "resync";
  return "patch";
}

/// Folds one overview event into the client's session map.
///
/// Returns `resync: true` and leaves state untouched when the update cannot be
/// applied safely.
function reduceOverview(current, update) {
  const kind = classifyOverviewUpdate(current.revision, update);
  if (kind === "resync") {
    return { resync: true, revision: current.revision, sessions: current.sessions };
  }
  return {
    resync: false,
    revision: Number.isInteger(update.revision) ? update.revision : current.revision,
    sessions: reconcileSessions(current.sessions, update),
  };
}

/// Short stream-state label for a pane error, by its server-supplied kind.
function paneErrorLabel(kind) {
  if (kind === "offline") return "Machine offline";
  if (kind === "upstream") return "Machine unreachable";
  return "Stream error";
}

/// The pane-scoped notice. A machine outage explains itself; anything else
/// falls back to the last pane stream error. Neither is local tmux health.
function paneNotice(machine, paneError, now) {
  if (!isMachineControllable(machine)) {
    return `${machine?.label || "This machine"} is offline. ${machineStatusLabel(machine, now)}`;
  }
  return paneError?.error || "";
}

/// Whether a non-empty browser selection touches the live pane. Stream redraws
/// must wait for this selection to clear so selecting and copying output stays
/// stable while an agent is producing new lines.
function selectionTouchesPane(pane, selection) {
  if (!pane || !selection || selection.isCollapsed) return false;
  return [selection.anchorNode, selection.focusNode]
    .some((node) => Boolean(node) && pane.contains(node));
}

function applyPanePatch(lines, revision, patch) {
  const validRange = Number.isInteger(patch.start_line)
    && Number.isInteger(patch.delete_lines)
    && patch.start_line >= 0
    && patch.delete_lines >= 0
    && patch.start_line <= lines.length
    && patch.start_line + patch.delete_lines <= lines.length;
  if (patch.base_revision !== revision || !validRange || !Array.isArray(patch.lines)) {
    return { applied: false, lines, revision };
  }
  const next = lines.slice();
  next.splice(patch.start_line, patch.delete_lines, ...patch.lines);
  return { applied: true, lines: next, revision: patch.revision };
}

/// Produces a small Markdown block tree. Rendering is deliberately performed
/// with DOM construction and textContent below; agent-authored Markdown never
/// reaches innerHTML.
function markdownBlocks(markdown, depth = 0) {
  const lines = String(markdown || "").replace(/\r\n?/g, "\n").split("\n");
  const blocks = [];
  for (let index = 0; index < lines.length;) {
    const line = lines[index];
    if (!line.trim()) { index += 1; continue; }
    const fence = line.match(/^ {0,3}(`{3,}|~{3,})\s*([\w.+-]*)\s*$/);
    if (fence) {
      const body = [];
      const marker = fence[1][0];
      const width = fence[1].length;
      index += 1;
      while (index < lines.length && !new RegExp(`^ {0,3}${marker}{${width},}\\s*$`).test(lines[index])) {
        body.push(lines[index]); index += 1;
      }
      if (index < lines.length) index += 1;
      blocks.push({ type: "code", language: fence[2] || "text", text: body.join("\n") });
      continue;
    }
    const heading = line.match(/^ {0,3}(#{1,6})\s+(.+?)\s*#*$/);
    if (heading) {
      blocks.push({ type: "heading", level: heading[1].length, text: heading[2] });
      index += 1; continue;
    }
    if (/^ {0,3}([-*_])(?:\s*\1){2,}\s*$/.test(line)) {
      blocks.push({ type: "rule" }); index += 1; continue;
    }
    if (/^ {0,3}>/.test(line)) {
      const quoted = [];
      while (index < lines.length && /^ {0,3}>/.test(lines[index])) {
        quoted.push(lines[index].replace(/^ {0,3}> ?/, "")); index += 1;
      }
      const quotedText = quoted.join("\n");
      blocks.push({
        type: "quote",
        children: depth >= 4
          ? [{ type: "paragraph", text: quotedText }]
          : markdownBlocks(quotedText, depth + 1),
      });
      continue;
    }
    const list = line.match(/^\s*([-+*]|\d+[.)])\s+(.+)$/);
    if (list) {
      const ordered = /^\d/.test(list[1]);
      const items = [];
      while (index < lines.length) {
        const item = lines[index].match(/^\s*([-+*]|\d+[.)])\s+(.+)$/);
        if (!item || /^\d/.test(item[1]) !== ordered) break;
        items.push(item[2]); index += 1;
      }
      blocks.push({ type: "list", ordered, items });
      continue;
    }
    if (index + 1 < lines.length && line.includes("|")
      && /^\s*\|?\s*:?-{3,}:?\s*(\|\s*:?-{3,}:?\s*)+\|?\s*$/.test(lines[index + 1])) {
      const rows = [splitTableRow(line)];
      index += 2;
      while (index < lines.length && lines[index].includes("|") && lines[index].trim()) {
        rows.push(splitTableRow(lines[index])); index += 1;
      }
      blocks.push({ type: "table", rows });
      continue;
    }
    const paragraph = [line.trim()];
    index += 1;
    while (index < lines.length && lines[index].trim()
      && !/^ {0,3}(`{3,}|~{3,}|#{1,6}\s|>)/.test(lines[index])
      && !/^ {0,3}([-*_])(?:\s*\1){2,}\s*$/.test(lines[index])
      && !/^\s*([-+*]|\d+[.)])\s+/.test(lines[index])) {
      paragraph.push(lines[index].trim()); index += 1;
    }
    blocks.push({ type: "paragraph", text: paragraph.join("\n") });
  }
  return blocks;
}

function splitTableRow(line) {
  return line.trim().replace(/^\||\|$/g, "").split("|").map((cell) => cell.trim());
}

function inlineTokens(text, depth = 0) {
  const value = String(text || "");
  if (depth > 4 || !value) return value ? [{ type: "text", text: value }] : [];
  const tokens = [];
  let plain = "";
  const flush = () => { if (plain) { tokens.push({ type: "text", text: plain }); plain = ""; } };
  for (let index = 0; index < value.length;) {
    if (value[index] === "`" && value.indexOf("`", index + 1) > index) {
      const end = value.indexOf("`", index + 1); flush();
      tokens.push({ type: "code", text: value.slice(index + 1, end) }); index = end + 1; continue;
    }
    if (value[index] === "[" && value.indexOf("](", index + 1) > index) {
      const middle = value.indexOf("](", index + 1);
      const end = value.indexOf(")", middle + 2);
      if (end > middle) {
        flush();
        tokens.push({ type: "link", url: value.slice(middle + 2, end), children: inlineTokens(value.slice(index + 1, middle), depth + 1) });
        index = end + 1; continue;
      }
    }
    const marker = value.startsWith("**", index) || value.startsWith("__", index)
      ? value.slice(index, index + 2)
      : (value.startsWith("~~", index) ? "~~" : null);
    if (marker) {
      const end = value.indexOf(marker, index + marker.length);
      if (end > index + marker.length) {
        flush();
        tokens.push({
          type: marker === "~~" ? "strike" : "strong",
          children: inlineTokens(value.slice(index + marker.length, end), depth + 1),
        });
        index = end + marker.length; continue;
      }
    }
    if ((value[index] === "*" || value[index] === "_") && value.indexOf(value[index], index + 1) > index + 1) {
      const end = value.indexOf(value[index], index + 1); flush();
      tokens.push({ type: "emphasis", children: inlineTokens(value.slice(index + 1, end), depth + 1) });
      index = end + 1; continue;
    }
    if (value[index] === "\n") { flush(); tokens.push({ type: "break" }); index += 1; continue; }
    plain += value[index]; index += 1;
  }
  flush();
  return tokens;
}

function safeLinkUrl(value, base = "https://atmux.invalid/") {
  try {
    const raw = String(value || "").trim();
    if (raw.length > 2048 || !/^https?:\/\//i.test(raw)) return null;
    const url = new URL(raw, base);
    return url.protocol === "https:" || url.protocol === "http:" ? url.href : null;
  } catch { return null; }
}

/// Splits plain text around bare http(s) URLs. Kept free of the DOM so the
/// scheme check and the trailing-punctuation rule stay unit-testable, and so
/// every rendered segment is still built as a text node or a checked anchor.
function linkifyTokens(text) {
  const value = String(text ?? "");
  const tokens = [];
  let position = 0;
  for (const match of value.matchAll(AUTOLINK_PATTERN)) {
    const candidate = trimmedAutolink(match[0]);
    const url = candidate ? safeLinkUrl(candidate) : null;
    if (!url) continue;
    if (match.index > position) tokens.push({ type: "text", text: value.slice(position, match.index) });
    tokens.push({ type: "link", text: candidate, url });
    position = match.index + candidate.length;
  }
  if (position < value.length) tokens.push({ type: "text", text: value.slice(position) });
  return tokens;
}

/// Prose ends sentences and wraps links in brackets; those characters belong to
/// the writing, not the address. Closers only leave the URL when the URL itself
/// never opened them.
function trimmedAutolink(value) {
  let text = String(value || "");
  const pairs = { ")": "(", "]": "[", "}": "{" };
  while (text) {
    const last = text[text.length - 1];
    if (".,;:!?'\"".includes(last)) { text = text.slice(0, -1); continue; }
    const opener = pairs[last];
    if (opener && text.split(last).length > text.split(opener).length) { text = text.slice(0, -1); continue; }
    break;
  }
  return text;
}

function linkifyInto(parent, text) {
  for (const token of linkifyTokens(text)) {
    if (token.type === "text") { parent.append(document.createTextNode(token.text)); continue; }
    const anchor = document.createElement("a");
    anchor.textContent = token.text;
    anchor.href = token.url;
    anchor.target = "_blank";
    anchor.rel = "noopener noreferrer";
    parent.append(anchor);
  }
  return parent;
}

// ---------------------------------------------------------------------------
// Source highlighting and navigation helpers for the Files viewer.
//
// The tokenizer runs over a whole file so block comments, text blocks and
// template literals keep their meaning across lines, then splits tokens back
// into lines for rendering. It never evaluates source text; every token is
// rendered with textContent.

const MAX_CODE_SYMBOL_CHARS = 128;
const CODE_SYMBOL_PATTERN = /^[A-Za-z_$][A-Za-z0-9_$]{0,127}$/;
const MAX_CODE_NAV_RESULTS = 200;
const CODE_NAV_OPERATIONS = new Set(["definitions", "references", "resolve"]);
const MAX_CODE_NAV_HISTORY = 50;

const codeWords = (value) => new Set(String(value).split(/\s+/).filter(Boolean));

const C_KEYWORDS = "auto break case char const continue default do double else enum extern float for goto if inline int long register restrict return short signed sizeof static struct switch typedef union unsigned void volatile while _Alignas _Alignof _Atomic _Bool _Complex _Generic _Noreturn _Static_assert _Thread_local";
const JS_KEYWORDS = "as async await break case catch class const continue debugger default delete do else export extends finally for from function get if import in instanceof let new of return set static super switch this throw try typeof var void while with yield";

const CODE_LANGUAGE_SPECS = (() => {
  const base = {
    lineComments: ["//"],
    blockComment: ["/*", "*/"],
    quotes: ['"', "'"],
    triple: [],
    backtick: false,
    keywords: new Set(),
    types: new Set(),
    literals: codeWords("true false null"),
    annotation: null,
    preprocessor: false,
    rust: false,
    typeCase: true,
    caseInsensitive: false,
    dollarVariables: false,
    ruby: false,
    keyStrings: false,
    hashBoundary: false,
    identifier: /[A-Za-z_$][\w$]*/y,
    stringPrefixes: null,
    css: false,
    navigable: true,
  };
  const spec = (overrides) => ({ ...base, ...overrides });
  const specs = {
    java: spec({
      keywords: codeWords("abstract assert break case catch class const continue default do else enum exports extends final finally for goto if implements import instanceof interface module native new non-sealed open opens package permits private protected provides public record requires return sealed static strictfp super switch synchronized this throw throws to transient transitive try uses var void volatile when while with yield"),
      types: codeWords("boolean byte char double float int long short"),
      triple: ['"""'],
      annotation: "@",
    }),
    kotlin: spec({
      keywords: codeWords("abstract actual annotation as break by catch class companion const constructor continue crossinline data do else enum expect external final finally for fun get if import in infix init inline inner interface internal is lateinit noinline object open operator out override package private protected public reified return sealed set super suspend tailrec this throw try typealias typeof val value var vararg when where while"),
      types: codeWords("Any Unit Nothing Int Long Short Byte Double Float Boolean Char String"),
      triple: ['"""'],
      annotation: "@",
    }),
    scala: spec({
      keywords: codeWords("abstract case catch class def do else enum export extends extension final finally for forSome given if implicit import lazy match new object override package private protected return sealed super then this throw trait try type using val var while with yield"),
      triple: ['"""'],
      annotation: "@",
    }),
    groovy: spec({
      keywords: codeWords("abstract as assert break case catch class const continue def default do else enum extends final finally for goto if implements import in instanceof interface native new package private protected public return static super switch synchronized this threadsafe throw throws trait transient try var while"),
      types: codeWords("boolean byte char double float int long short void"),
      triple: ['"""', "'''"],
      annotation: "@",
    }),
    rust: spec({
      keywords: codeWords("as async await break const continue crate dyn else enum extern fn for if impl in let loop match mod move mut pub ref return self Self static struct super trait type union unsafe use where while yield macro_rules"),
      types: codeWords("bool char str u8 u16 u32 u64 u128 usize i8 i16 i32 i64 i128 isize f32 f64 String Vec Option Result Box"),
      literals: codeWords("true false None Some Ok Err"),
      quotes: ['"'],
      rust: true,
    }),
    typescript: spec({
      keywords: codeWords(`${JS_KEYWORDS} abstract any asserts declare enum implements infer interface is keyof module namespace never override private protected public readonly require satisfies type unique unknown`),
      types: codeWords("string number boolean bigint symbol object void undefined never unknown any"),
      literals: codeWords("true false null undefined NaN Infinity"),
      backtick: true,
      annotation: "@",
    }),
    javascript: spec({
      keywords: codeWords(JS_KEYWORDS),
      literals: codeWords("true false null undefined NaN Infinity"),
      backtick: true,
      annotation: "@",
    }),
    python: spec({
      lineComments: ["#"],
      blockComment: null,
      triple: ['"""', "'''"],
      keywords: codeWords("and as assert async await break case class continue def del elif else except finally for from global if import in is lambda match nonlocal not or pass raise return try type while with yield"),
      types: codeWords("int float str bool bytes list dict set tuple object"),
      literals: codeWords("True False None self cls"),
      annotation: "@",
      stringPrefixes: /[rRbBuUfF]{1,2}(?=["'])/y,
    }),
    go: spec({
      keywords: codeWords("break case chan const continue default defer else fallthrough for func go goto if import interface map package range return select struct switch type var"),
      types: codeWords("bool byte complex64 complex128 error float32 float64 int int8 int16 int32 int64 rune string uint uint8 uint16 uint32 uint64 uintptr any"),
      literals: codeWords("true false nil iota"),
      backtick: true,
    }),
    c: spec({
      keywords: codeWords(C_KEYWORDS),
      types: codeWords("size_t ssize_t int8_t int16_t int32_t int64_t uint8_t uint16_t uint32_t uint64_t bool FILE"),
      literals: codeWords("true false NULL"),
      preprocessor: true,
      typeCase: false,
    }),
    cpp: spec({
      keywords: codeWords(`${C_KEYWORDS} alignas alignof and asm bool catch class concept consteval constexpr constinit co_await co_return co_yield decltype delete dynamic_cast explicit export final friend mutable namespace new noexcept not operator or override private protected public reinterpret_cast requires static_assert static_cast template this thread_local throw try typeid typename using virtual`),
      types: codeWords("size_t std string vector map unique_ptr shared_ptr bool wchar_t char8_t char16_t char32_t"),
      literals: codeWords("true false nullptr NULL"),
      preprocessor: true,
    }),
    csharp: spec({
      keywords: codeWords("abstract as async await base break case catch checked class const continue default delegate do else enum event explicit extern finally fixed for foreach get goto if implicit in init interface internal is lock namespace new operator out override params partial private protected public readonly record ref required return sealed set sizeof stackalloc static struct switch this throw try typeof unchecked unsafe using value var virtual void volatile when where while with yield"),
      types: codeWords("bool byte char decimal double dynamic float int long nint nuint object sbyte short string uint ulong ushort"),
      preprocessor: true,
      stringPrefixes: /(?:\$@|@\$|\$|@)(?=")/y,
    }),
    swift: spec({
      keywords: codeWords("actor any as associatedtype async await break case catch class continue default defer deinit do else enum extension fallthrough fileprivate final for func guard if import in indirect init inout internal is lazy let mutating nonmutating open operator override private protocol public repeat required rethrows return self Self some static struct subscript super switch throw throws try typealias var weak where while"),
      types: codeWords("Int Double Float Bool String Character Array Dictionary Set Optional Void"),
      literals: codeWords("true false nil"),
      triple: ['"""'],
      annotation: "@",
      preprocessor: true,
    }),
    ruby: spec({
      lineComments: ["#"],
      blockComment: null,
      keywords: codeWords("alias and begin break case class def defined do else elsif end ensure for if in module next not or redo rescue retry return self super then undef unless until when while yield require require_relative include extend attr_accessor attr_reader attr_writer private protected public"),
      literals: codeWords("true false nil"),
      ruby: true,
      identifier: /[A-Za-z_][\w]*[?!]?/y,
    }),
    php: spec({
      lineComments: ["//", "#"],
      keywords: codeWords("abstract and array as break callable case catch class clone const continue declare default do echo else elseif empty enddeclare endfor endforeach endif endswitch endwhile enum extends final finally fn for foreach function global goto if implements include include_once instanceof insteadof interface isset list match namespace new or print private protected public readonly require require_once return static switch throw trait try unset use var while xor yield"),
      types: codeWords("int float string bool array object mixed void never iterable"),
      literals: codeWords("true false null TRUE FALSE NULL"),
      dollarVariables: true,
      hashBoundary: true,
      annotation: "#[",
    }),
    shell: spec({
      lineComments: ["#"],
      blockComment: null,
      keywords: codeWords("if then else elif fi case esac for select while until do done in function time coproc return exit break continue local export readonly declare typeset unset shift source alias eval exec set trap"),
      literals: codeWords("true false"),
      dollarVariables: true,
      hashBoundary: true,
      typeCase: false,
      identifier: /[A-Za-z_][\w-]*/y,
    }),
    sql: spec({
      lineComments: ["--"],
      quotes: ["'", '"'],
      keywords: codeWords("add all alter and any as asc begin between by case cascade check column commit constraint create cross database default delete desc distinct drop else end exists foreign from full function grant group having if in index inner insert intersect into is join key left like limit not null offset on or order outer primary procedure references replace returning revoke right rollback schema select sequence set table then to transaction trigger truncate union unique update using values view when where with"),
      types: codeWords("int integer bigint smallint serial bigserial decimal numeric real double precision float boolean bool char varchar text date time timestamp timestamptz interval uuid json jsonb bytea"),
      literals: codeWords("true false null"),
      caseInsensitive: true,
      typeCase: false,
    }),
    protobuf: spec({
      keywords: codeWords("syntax edition package import option message enum service rpc returns stream repeated optional required oneof map reserved extend extensions to max public weak"),
      types: codeWords("double float int32 int64 uint32 uint64 sint32 sint64 fixed32 fixed64 sfixed32 sfixed64 bool string bytes"),
    }),
    graphql: spec({
      lineComments: ["#"],
      blockComment: null,
      quotes: ['"'],
      triple: ['"""'],
      keywords: codeWords("query mutation subscription fragment on type interface union enum input scalar schema extend implements directive repeatable"),
      types: codeWords("Int Float String Boolean ID"),
      dollarVariables: true,
      annotation: "@",
    }),
    json: spec({
      quotes: ['"'],
      keyStrings: true,
      typeCase: false,
      navigable: false,
    }),
    css: spec({
      lineComments: [],
      css: true,
      typeCase: false,
      navigable: false,
      identifier: /-?-?[A-Za-z_][\w-]*/y,
      keywords: codeWords("important"),
      literals: new Set(),
    }),
    scss: spec({
      css: true,
      typeCase: false,
      navigable: false,
      dollarVariables: true,
      identifier: /-?-?[A-Za-z_][\w-]*/y,
      keywords: codeWords("important"),
      literals: new Set(),
    }),
    generic: spec({
      lineComments: ["//", "#"],
      hashBoundary: true,
      backtick: true,
      keywords: codeWords("async await break case class const continue def else enum fn for function if impl import in let match mod new pub return self static struct throw trait try type use var while"),
      typeCase: false,
      navigable: false,
    }),
  };
  specs.jsx = specs.javascript;
  specs.tsx = specs.typescript;
  return specs;
})();

const CODE_LINE_MODES = new Set(["yaml", "toml", "markdown", "dockerfile", "makefile", "ini", "gitignore", "text"]);
const CODE_MARKUP_MODES = new Set(["html", "xml"]);

const CODE_LANGUAGE_ALIASES = new Map(Object.entries({
  ts: "typescript", mts: "typescript", cts: "typescript", tsx: "tsx", jsx: "jsx", js: "javascript",
  mjs: "javascript", cjs: "javascript", node: "javascript", py: "python", python3: "python",
  rs: "rust", sh: "shell", bash: "shell", zsh: "shell", ksh: "shell", dash: "shell", console: "shell",
  yml: "yaml", htm: "html", xhtml: "html", svg: "xml", kt: "kotlin", kts: "kotlin", cs: "csharp",
  "c++": "cpp", cc: "cpp", cxx: "cpp", hpp: "cpp", h: "c", rb: "ruby", golang: "go", proto: "protobuf",
  gql: "graphql", md: "markdown", docker: "dockerfile", make: "makefile", mk: "makefile",
  gradle: "groovy", sass: "scss", less: "scss", jsonc: "json", json5: "json", vue: "html",
  svelte: "html", plaintext: "text", txt: "text",
}));

const CODE_EXTENSIONS = new Map(Object.entries({
  java: "java", kt: "kotlin", kts: "kotlin", scala: "scala", sc: "scala", groovy: "groovy",
  gradle: "groovy", rs: "rust", ts: "typescript", mts: "typescript", cts: "typescript",
  tsx: "tsx", js: "javascript", mjs: "javascript", cjs: "javascript", jsx: "jsx", py: "python",
  pyi: "python", go: "go", c: "c", h: "c", cc: "cpp", cpp: "cpp", cxx: "cpp", hpp: "cpp",
  hh: "cpp", hxx: "cpp", m: "cpp", mm: "cpp", cs: "csharp", swift: "swift", rb: "ruby",
  rake: "ruby", gemspec: "ruby", php: "php", sh: "shell", bash: "shell", zsh: "shell",
  ksh: "shell", sql: "sql", json: "json", jsonc: "json", json5: "json", yaml: "yaml", yml: "yaml",
  toml: "toml", xml: "xml", xsd: "xml", xsl: "xml", svg: "xml", plist: "xml", pom: "xml",
  html: "html", htm: "html", vue: "html", svelte: "html", css: "css", scss: "scss", sass: "scss",
  less: "scss", md: "markdown", markdown: "markdown", proto: "protobuf", graphql: "graphql",
  gql: "graphql", ini: "ini", cfg: "ini", conf: "ini", properties: "ini", env: "ini",
}));

const CODE_FILE_NAMES = new Map(Object.entries({
  dockerfile: "dockerfile", containerfile: "dockerfile", makefile: "makefile",
  gnumakefile: "makefile", jenkinsfile: "groovy", rakefile: "ruby", gemfile: "ruby",
  podfile: "ruby", vagrantfile: "ruby", build: "python", workspace: "python",
  ".bashrc": "shell", ".zshrc": "shell", ".profile": "shell", ".bash_profile": "shell",
  ".gitignore": "gitignore", ".dockerignore": "gitignore", ".editorconfig": "ini",
}));

/// Resolves the highlighting language from the owner's hint, the file name,
/// and a shebang for extensionless scripts.
function codeLanguage(path, hint = "", content = "") {
  const declared = String(hint || "").trim().toLowerCase().replace(/[^a-z0-9_+-]/g, "");
  const known = (value) => {
    const resolved = CODE_LANGUAGE_ALIASES.get(value) || value;
    return CODE_LANGUAGE_SPECS[resolved] || CODE_LINE_MODES.has(resolved)
      || CODE_MARKUP_MODES.has(resolved) || resolved === "diff" ? resolved : null;
  };
  if (declared && declared !== "text") {
    const resolved = known(declared);
    if (resolved) return resolved;
  }
  const name = String(path || "").split("/").pop().toLowerCase();
  if (CODE_FILE_NAMES.has(name)) return CODE_FILE_NAMES.get(name);
  if (name.startsWith("dockerfile.") || name.endsWith(".dockerfile")) return "dockerfile";
  const extension = name.includes(".") ? name.split(".").pop() : "";
  if (extension && CODE_EXTENSIONS.has(extension)) return CODE_EXTENSIONS.get(extension);
  const firstLine = String(content || "").slice(0, 200).split("\n", 1)[0];
  const shebang = /^#!\s*(?:\S*\/)?(?:env\s+(?:-\S+\s+)*)?([A-Za-z0-9_.+-]+)/.exec(firstLine);
  if (shebang) {
    const interpreter = shebang[1].toLowerCase().replace(/[0-9.]+$/, "");
    const mapped = {
      python: "python", node: "javascript", deno: "typescript", bun: "typescript",
      bash: "shell", sh: "shell", zsh: "shell", ksh: "shell", dash: "shell", ruby: "ruby",
      php: "php", groovy: "groovy", swift: "swift", kotlin: "kotlin",
    }[interpreter];
    if (mapped) return mapped;
  }
  return declared && known(declared) ? known(declared) : "text";
}

/// Whether identifiers in this language can be sent to the owner for
/// definition and reference search.
function codeLanguageNavigable(language) {
  const spec = CODE_LANGUAGE_SPECS[language];
  return Boolean(spec?.navigable);
}

function codeSymbolValid(symbol) {
  return typeof symbol === "string" && CODE_SYMBOL_PATTERN.test(symbol);
}

function sourceLineEnd(source, index) {
  const end = source.indexOf("\n", index);
  return end < 0 ? source.length : end;
}

function closingQuoteIndex(source, from, quote, escapes = true, multiline = false) {
  for (let index = from; index < source.length; index += 1) {
    const character = source[index];
    if (escapes && character === "\\") { index += 1; continue; }
    if (character === quote) return index;
    if (character === "\n" && !multiline) return index - 1;
  }
  return source.length - 1;
}

function nextSignificantCharacter(source, index) {
  let cursor = index;
  while (cursor < source.length && (source[cursor] === " " || source[cursor] === "\t")) cursor += 1;
  return source[cursor] || "";
}

function classifyCodeWord(word, spec, source, end) {
  const lookup = spec.caseInsensitive ? word.toLowerCase() : word;
  if (spec.keywords.has(lookup)) return "keyword";
  if (spec.literals.has(lookup)) return "literal";
  if (spec.types.has(lookup)) return "type";
  const next = nextSignificantCharacter(source, end);
  if (next === "(") return "function";
  if (spec.rust && next === "!" && source[end + 1] !== "=") return "function";
  if (spec.typeCase && /^[A-Z]/.test(word)) {
    return word.length > 1 && word === word.toUpperCase() ? "constant" : "type";
  }
  return "identifier";
}

const CODE_NUMBER_PATTERN = /(?:0[xX][\da-fA-F_]+|0[bB][01_]+|0[oO][0-7_]+|(?:\d[\d_]*)?\.?\d[\d_]*(?:[eE][+-]?\d+)?)[A-Za-z%]*/y;

/// Tokenizes C-family, scripting, and data languages in one pass.
function tokenizeCode(source, spec) {
  const tokens = [];
  const length = source.length;
  let index = 0;
  let plain = 0;
  let depth = 0;
  let lineStart = true;
  let lastKind = "";
  let lastText = "";
  const emit = (kind, start, end) => {
    if (start > plain) tokens.push({ kind: "plain", text: source.slice(plain, start) });
    tokens.push({ kind, text: source.slice(start, end) });
    plain = end;
    index = end;
    lastKind = kind;
    lastText = tokens[tokens.length - 1].text;
  };
  const sticky = (pattern) => {
    pattern.lastIndex = index;
    const match = pattern.exec(source);
    return match ? match[0] : null;
  };
  while (index < length) {
    const character = source[index];
    if (character === "\n") { lineStart = true; index += 1; continue; }
    if (character === " " || character === "\t" || character === "\r") { index += 1; continue; }
    const atLineStart = lineStart;
    lineStart = false;
    const previous = index > 0 ? source[index - 1] : "\n";
    const boundary = !/[\w$]/.test(previous);

    const comment = spec.lineComments.find((marker) => source.startsWith(marker, index)
      && (marker !== "#" || !spec.hashBoundary || /\s/.test(previous) || index === 0)
      && !(marker === "#" && spec.annotation === "#[" && source[index + 1] === "["));
    if (comment) { emit("comment", index, sourceLineEnd(source, index)); continue; }
    if (spec.blockComment && source.startsWith(spec.blockComment[0], index)) {
      const close = source.indexOf(spec.blockComment[1], index + spec.blockComment[0].length);
      emit("comment", index, close < 0 ? length : close + spec.blockComment[1].length);
      continue;
    }
    if (spec.preprocessor && character === "#" && atLineStart) {
      const directive = sticky(/#\s*[A-Za-z_]\w*/y);
      if (directive) {
        emit("annotation", index, index + directive.length);
        if (/include|import/.test(directive)) {
          while (source[index] === " " || source[index] === "\t") index += 1;
          if (source[index] === "<") {
            const close = source.indexOf(">", index);
            const end = close < 0 || close > sourceLineEnd(source, index) ? sourceLineEnd(source, index) : close + 1;
            emit("string", index, end);
          }
        }
        continue;
      }
    }
    if ((spec.rust || spec.annotation === "#[") && character === "#"
      && (source[index + 1] === "[" || (source[index + 1] === "!" && source[index + 2] === "["))) {
      const lineEnd = sourceLineEnd(source, index);
      let bracket = 0;
      let end = lineEnd;
      for (let cursor = index; cursor < lineEnd; cursor += 1) {
        if (source[cursor] === "[") bracket += 1;
        else if (source[cursor] === "]") { bracket -= 1; if (bracket === 0) { end = cursor + 1; break; } }
      }
      emit("annotation", index, end);
      continue;
    }
    const triple = spec.triple.find((quote) => source.startsWith(quote, index));
    if (triple) {
      const close = source.indexOf(triple, index + triple.length);
      emit("string", index, close < 0 ? length : close + triple.length);
      continue;
    }
    if (spec.rust && boundary && (character === "r" || (character === "b" && source[index + 1] === "r"))) {
      const raw = sticky(/b?r(#*)"/y);
      if (raw) {
        const hashes = raw.slice(raw.indexOf("r") + 1, -1);
        const close = source.indexOf(`"${hashes}`, index + raw.length);
        emit("string", index, close < 0 ? length : close + 1 + hashes.length);
        continue;
      }
    }
    if (spec.stringPrefixes && boundary) {
      const prefix = sticky(spec.stringPrefixes);
      if (prefix) {
        const quoteIndex = index + prefix.length;
        const quote = source[quoteIndex];
        const tripleQuote = spec.triple.find((value) => source.startsWith(value, quoteIndex));
        if (tripleQuote) {
          const close = source.indexOf(tripleQuote, quoteIndex + 3);
          emit("string", index, close < 0 ? length : close + 3);
        } else {
          const verbatim = prefix.includes("@");
          const close = closingQuoteIndex(source, quoteIndex + 1, quote, !verbatim, verbatim);
          emit("string", index, close + 1);
        }
        continue;
      }
    }
    if (character === "`" && spec.backtick) {
      const close = closingQuoteIndex(source, index + 1, "`", true, true);
      emit("string", index, close + 1);
      continue;
    }
    if (spec.rust && character === "b" && source[index + 1] === "'" && boundary) {
      const literal = sticky(/b'(?:\\.|[^\\'\n])'/y);
      if (literal) { emit("string", index, index + literal.length); continue; }
    }
    if (spec.rust && character === "'") {
      const literal = sticky(/'(?:\\(?:x[0-9a-fA-F]{2}|u\{[0-9a-fA-F]{1,6}\}|.)|[^\\'\n])'/y);
      if (literal) { emit("string", index, index + literal.length); continue; }
      const lifetime = sticky(/'[A-Za-z_]\w*/y);
      if (lifetime) { emit("annotation", index, index + lifetime.length); continue; }
    }
    if (spec.quotes.includes(character)) {
      const close = closingQuoteIndex(source, index + 1, character, true, spec.rust);
      emit("string", index, close + 1);
      if (spec.keyStrings && nextSignificantCharacter(source, index) === ":") {
        tokens[tokens.length - 1].kind = "property";
        lastKind = "property";
      }
      continue;
    }
    if (spec.dollarVariables && character === "$") {
      const variable = sticky(/\$(?:\{[^}\n]*\}|\([^)\n]*\)|[A-Za-z_][\w]*|[0-9@#?$!*-])/y);
      if (variable) { emit("variable", index, index + variable.length); continue; }
    }
    if (spec.ruby && (character === "@" || (character === ":" && source[index + 1] !== ":" && previous !== ":"))) {
      const word = sticky(character === "@" ? /@@?[A-Za-z_]\w*/y : /:[A-Za-z_]\w*[?!]?/y);
      if (word) { emit(character === "@" ? "variable" : "literal", index, index + word.length); continue; }
    }
    if (spec.annotation === "@" && character === "@") {
      const annotation = sticky(spec.css ? /@[A-Za-z-]+/y : /@[A-Za-z_][\w.]*/y);
      if (annotation) { emit(spec.css ? "keyword" : "annotation", index, index + annotation.length); continue; }
    }
    if (spec.css) {
      if (character === "{") depth += 1;
      else if (character === "}") depth = Math.max(0, depth - 1);
      if (character === "#" && /[0-9a-fA-F]/.test(source[index + 1] || "")) {
        const color = sticky(/#[0-9a-fA-F]{3,8}\b/y);
        if (color) { emit("number", index, index + color.length); continue; }
      }
      if (character === "@") {
        const rule = sticky(/@[A-Za-z-]+/y);
        if (rule) { emit("keyword", index, index + rule.length); continue; }
      }
      if (character === "!" && source.startsWith("!important", index)) { emit("keyword", index, index + 10); continue; }
    }
    if (boundary && (/[0-9]/.test(character) || (character === "." && /[0-9]/.test(source[index + 1] || "")))) {
      const number = sticky(CODE_NUMBER_PATTERN);
      if (number) { emit("number", index, index + number.length); continue; }
    }
    if (boundary && (/[A-Za-z_$]/.test(character) || (spec.css && character === "-" && /[-A-Za-z_]/.test(source[index + 1] || "")))) {
      const word = sticky(spec.identifier);
      if (word) {
        let kind = classifyCodeWord(word, spec, source, index + word.length);
        if (spec.css) {
          kind = depth > 0 && nextSignificantCharacter(source, index + word.length) === ":" ? "property" : "plain";
        } else if (lastKind === "keyword" && kind === "identifier"
          && /^(?:class|interface|enum|struct|trait|record|type|typealias|object|protocol|union|namespace|module|actor)$/.test(lastText)) {
          kind = "type";
        } else if (lastKind === "keyword" && ["identifier", "type"].includes(kind)
          && /^(?:function|fn|def|func|fun)$/.test(lastText)) {
          kind = "function";
        }
        emit(kind, index, index + word.length);
        continue;
      }
    }
    index += 1;
  }
  if (plain < length) tokens.push({ kind: "plain", text: source.slice(plain) });
  return tokens;
}

/// Tokenizes XML and HTML, including embedded script and style blocks.
function tokenizeMarkup(source, html) {
  const tokens = [];
  let index = 0;
  let plain = 0;
  const flush = (end) => { if (end > plain) tokens.push({ kind: "plain", text: source.slice(plain, end) }); };
  const emit = (kind, start, end) => { flush(start); tokens.push({ kind, text: source.slice(start, end) }); plain = end; index = end; };
  while (index < source.length) {
    if (source.startsWith("<!--", index)) {
      const close = source.indexOf("-->", index + 4);
      emit("comment", index, close < 0 ? source.length : close + 3); continue;
    }
    if (source.startsWith("<![CDATA[", index)) {
      const close = source.indexOf("]]>", index);
      emit("string", index, close < 0 ? source.length : close + 3); continue;
    }
    if (source.startsWith("<?", index) || source.startsWith("<!", index)) {
      const close = source.indexOf(">", index);
      emit("annotation", index, close < 0 ? source.length : close + 1); continue;
    }
    if (source[index] === "&") {
      const entity = /&(?:#\d+|#x[0-9a-fA-F]+|[A-Za-z]+);/y;
      entity.lastIndex = index;
      const match = entity.exec(source);
      if (match) { emit("literal", index, index + match[0].length); continue; }
    }
    if (source[index] === "<" && /[A-Za-z/]/.test(source[index + 1] || "")) {
      const name = /<\/?([A-Za-z][\w:.-]*)/y;
      name.lastIndex = index;
      const match = name.exec(source);
      if (match) {
        flush(index);
        const opening = source[index + 1] !== "/";
        const bracketLength = opening ? 1 : 2;
        tokens.push({ kind: "plain", text: source.slice(index, index + bracketLength) });
        tokens.push({ kind: "tag", text: match[1] });
        index += match[0].length; plain = index;
        while (index < source.length && source[index] !== ">") {
          const character = source[index];
          if (character === '"' || character === "'") {
            const close = source.indexOf(character, index + 1);
            emit("string", index, close < 0 ? source.length : close + 1); continue;
          }
          if (/[^\s=/>"']/.test(character)) {
            const attribute = /[^\s=/>"']+/y;
            attribute.lastIndex = index;
            const found = attribute.exec(source);
            emit("property", index, index + found[0].length); continue;
          }
          index += 1;
        }
        if (index < source.length) index += 1;
        const tag = match[1].toLowerCase();
        if (html && opening && (tag === "script" || tag === "style")) {
          flush(index); plain = index;
          const close = source.toLowerCase().indexOf(`</${tag}`, index);
          const end = close < 0 ? source.length : close;
          const embedded = tokenizeCode(source.slice(index, end), CODE_LANGUAGE_SPECS[tag === "script" ? "javascript" : "css"]);
          tokens.push(...embedded);
          index = end; plain = end;
        }
        continue;
      }
    }
    index += 1;
  }
  flush(source.length);
  return tokens;
}

/// Line-oriented formats: YAML, TOML, INI, Markdown, Dockerfile, Makefile.
function tokenizeLines(source, mode) {
  const tokens = [];
  const lines = source.split("\n");
  let fenced = false;
  let tripleQuote = null;
  const pushValue = (text) => {
    const pattern = /("(?:\\.|[^"\\])*"?|'[^']*'?|\$\{[^}]*\}|\$\([^)]*\)|\$[A-Za-z_@<^?*][\w]*|\b\d[\d_.:\-TZ]*\b|\b(?:true|false|null|yes|no|on|off)\b|#.*$|[&*][A-Za-z_][\w-]*)/g;
    let position = 0;
    for (const match of text.matchAll(pattern)) {
      const value = match[0];
      if (value.startsWith("#") && match.index > 0 && !/\s/.test(text[match.index - 1])) continue;
      if (match.index > position) tokens.push({ kind: "plain", text: text.slice(position, match.index) });
      const kind = value.startsWith("#") ? "comment"
        : value.startsWith('"') || value.startsWith("'") ? "string"
          : value.startsWith("$") ? "variable"
            : value.startsWith("&") || value.startsWith("*") ? "annotation"
              : /^\d/.test(value) ? "number" : "literal";
      tokens.push({ kind, text: value });
      position = match.index + value.length;
    }
    if (position < text.length) tokens.push({ kind: "plain", text: text.slice(position) });
  };
  lines.forEach((line, index) => {
    if (index > 0) tokens.push({ kind: "plain", text: "\n" });
    if (mode === "text" || !line) { if (line) tokens.push({ kind: "plain", text: line }); return; }
    if (mode === "markdown") {
      if (/^\s*(?:```|~~~)/.test(line)) { fenced = !fenced; tokens.push({ kind: "keyword", text: line }); return; }
      if (fenced) { tokens.push({ kind: "string", text: line }); return; }
      if (/^#{1,6}\s/.test(line)) { tokens.push({ kind: "heading", text: line }); return; }
      if (/^\s*>/.test(line)) { tokens.push({ kind: "comment", text: line }); return; }
      const marker = /^(\s*)([-*+]|\d+[.)])(\s+)/.exec(line);
      let rest = line;
      if (marker) {
        tokens.push({ kind: "plain", text: marker[1] }, { kind: "keyword", text: marker[2] }, { kind: "plain", text: marker[3] });
        rest = line.slice(marker[0].length);
      }
      let position = 0;
      for (const match of rest.matchAll(/`[^`]+`|\[[^\]]+\]\([^)]+\)/g)) {
        if (match.index > position) tokens.push({ kind: "plain", text: rest.slice(position, match.index) });
        tokens.push({ kind: match[0].startsWith("`") ? "string" : "property", text: match[0] });
        position = match.index + match[0].length;
      }
      if (position < rest.length) tokens.push({ kind: "plain", text: rest.slice(position) });
      return;
    }
    if (tripleQuote) {
      const close = line.indexOf(tripleQuote);
      if (close < 0) { tokens.push({ kind: "string", text: line }); return; }
      tokens.push({ kind: "string", text: line.slice(0, close + 3) });
      tripleQuote = null;
      pushValue(line.slice(close + 3));
      return;
    }
    if ((mode === "ini" ? /^\s*[#;]/ : /^\s*#/).test(line)) { tokens.push({ kind: "comment", text: line }); return; }
    if (mode === "gitignore") { tokens.push({ kind: line.startsWith("!") ? "keyword" : "plain", text: line }); return; }
    if (mode === "dockerfile") {
      const instruction = /^(\s*)([A-Za-z]+)(\s|$)/.exec(line);
      if (instruction) {
        tokens.push({ kind: "plain", text: instruction[1] }, { kind: "keyword", text: instruction[2] });
        pushValue(line.slice(instruction[1].length + instruction[2].length));
        return;
      }
      pushValue(line); return;
    }
    if (mode === "makefile") {
      const target = /^([A-Za-z0-9_.%/$(){}-][^:=#]*?)(\s*::?)(?!=)/.exec(line);
      const variable = /^(\s*)([A-Za-z_][\w.]*)(\s*(?:[:?+!]?=))/.exec(line);
      if (!line.startsWith("\t") && variable) {
        tokens.push({ kind: "plain", text: variable[1] }, { kind: "property", text: variable[2] }, { kind: "plain", text: variable[3] });
        pushValue(line.slice(variable[0].length)); return;
      }
      if (!line.startsWith("\t") && target) {
        tokens.push({ kind: "function", text: target[1] }, { kind: "plain", text: target[2] });
        pushValue(line.slice(target[0].length)); return;
      }
      pushValue(line); return;
    }
    if ((mode === "toml" || mode === "ini") && /^\s*\[/.test(line)) { tokens.push({ kind: "heading", text: line }); return; }
    const key = mode === "yaml"
      ? /^(\s*(?:-\s+)?)((?:"[^"]*"|'[^']*'|[^\s#'"{}[\],:][^#:]*?))(\s*:)(?=\s|$)/.exec(line)
      : /^(\s*)((?:"[^"]*"|'[^']*'|[A-Za-z0-9_.-]+(?:\s*\.\s*[A-Za-z0-9_-]+)*))(\s*[=:])/.exec(line);
    if (mode === "yaml" && /^(?:---|\.\.\.)\s*$/.test(line)) { tokens.push({ kind: "keyword", text: line }); return; }
    if (key) {
      tokens.push({ kind: "plain", text: key[1] }, { kind: "property", text: key[2] }, { kind: "plain", text: key[3] });
      const rest = line.slice(key[0].length);
      const opener = /("""|''')/.exec(rest);
      if (mode === "toml" && opener && rest.indexOf(opener[1], opener.index + 3) < 0) {
        pushValue(rest.slice(0, opener.index));
        tokens.push({ kind: "string", text: rest.slice(opener.index) });
        tripleQuote = opener[1];
        return;
      }
      pushValue(rest);
      return;
    }
    pushValue(line);
  });
  return tokens;
}

/// Splits whole-file tokens into rendered lines. Each line is an array of
/// `{ kind, text }` segments whose texts concatenate to the source line.
function splitTokenLines(tokens) {
  const lines = [[]];
  for (const token of tokens) {
    const parts = token.text.split("\n");
    parts.forEach((part, index) => {
      if (index > 0) lines.push([]);
      if (part) lines[lines.length - 1].push({ kind: token.kind, text: part });
    });
  }
  return lines;
}

/// Tokenizes a whole source file into highlighted lines.
function tokenizeSource(text, language = "text") {
  const source = String(text ?? "");
  const resolved = CODE_LANGUAGE_ALIASES.get(String(language || "").toLowerCase()) || String(language || "").toLowerCase();
  if (CODE_MARKUP_MODES.has(resolved)) return splitTokenLines(tokenizeMarkup(source, resolved === "html"));
  if (CODE_LINE_MODES.has(resolved)) return splitTokenLines(tokenizeLines(source, resolved));
  const spec = CODE_LANGUAGE_SPECS[resolved] || CODE_LANGUAGE_SPECS.generic;
  return splitTokenLines(tokenizeCode(source, spec));
}

/// Flat highlighted segments, used by Markdown code blocks. Newlines are
/// kept as plain segments so the text round-trips exactly.
function highlightCode(text, language = "generic") {
  const segments = [];
  tokenizeSource(text, language || "generic").forEach((line, index) => {
    if (index > 0) segments.push({ kind: "plain", text: "\n" });
    segments.push(...line);
  });
  return segments;
}

// ---------------------------------------------------------------------------
// Import extraction

function codeImportEntry(lines, line, start, end, spec, symbol = null) {
  if (!spec || spec.length > 512 || end <= start) return;
  const entries = lines.get(line) || [];
  entries.push({ start, end, spec, symbol });
  lines.set(line, entries);
}

/// Finds import statements and the names they bind. `lines` maps a 1-based
/// line to clickable `{ start, end, spec, symbol }` ranges; `names` maps an
/// imported local name to the `{ spec, symbol }` that resolves it.
function sourceImports(content, language) {
  const names = new Map();
  const lines = new Map();
  const text = String(content || "");
  const sourceLines = text.split("\n");
  const bind = (name, spec, symbol) => {
    if (codeSymbolValid(name) && !names.has(name)) {
      names.set(name, { spec, symbol: codeSymbolValid(symbol) ? symbol : null });
    }
  };
  const lang = CODE_LANGUAGE_ALIASES.get(language) || language;
  let goImportGroup = false;
  sourceLines.forEach((line, index) => {
    const number = index + 1;
    if (["java", "kotlin", "scala", "groovy"].includes(lang)) {
      const match = /^(\s*import\s+(?:static\s+)?)([\w.]+(?:\.\*)?)(?:\s+as\s+(\w+))?/.exec(line);
      if (match) {
        const path = match[2];
        const spec = line.trim().replace(/;\s*$/, "");
        codeImportEntry(lines, number, match[1].length, match[1].length + path.length, spec);
        const last = path.split(".").pop();
        if (last !== "*") bind(match[3] || last, spec, last);
      }
    } else if (["typescript", "tsx", "javascript", "jsx"].includes(lang)) {
      for (const match of line.matchAll(/\b(?:from|import|require\s*\(|import\s*\()\s*(['"])([^'"\n]+)\1/g)) {
        const start = match.index + match[0].indexOf(match[1]) + 1;
        codeImportEntry(lines, number, start, start + match[2].length, match[2]);
      }
    } else if (lang === "python") {
      const from = /^(\s*from\s+)([.\w]+)(\s+import\s+)(.*)$/.exec(line);
      if (from) {
        codeImportEntry(lines, number, from[1].length, from[1].length + from[2].length, `from ${from[2]} import *`);
        let clause = from[4];
        if (clause.includes("(") && !clause.includes(")")) {
          for (let next = index + 1; next < sourceLines.length && next < index + 200; next += 1) {
            clause += ` ${sourceLines[next]}`;
            if (sourceLines[next].includes(")")) break;
          }
        }
        for (const part of clause.replace(/[()]/g, " ").split(",")) {
          const [name, alias] = part.trim().split(/\s+as\s+/);
          if (name && name !== "*") bind((alias || name).trim(), `from ${from[2]} import ${name.trim()}`, name.trim());
        }
      }
      const plain = /^(\s*import\s+)([\w.]+)(?:\s+as\s+(\w+))?/.exec(line);
      if (plain) {
        codeImportEntry(lines, number, plain[1].length, plain[1].length + plain[2].length, `import ${plain[2]}`);
        bind(plain[3] || plain[2].split(".")[0], `import ${plain[2]}`, null);
      }
    } else if (lang === "rust") {
      const module = /^(\s*(?:pub(?:\([^)]*\))?\s+)?mod\s+)([A-Za-z_]\w*)\s*;/.exec(line);
      if (module) codeImportEntry(lines, number, module[1].length, module[1].length + module[2].length, `mod ${module[2]}`);
      const use = /^(\s*(?:pub(?:\([^)]*\))?\s+)?use\s+)([^;]+)/.exec(line);
      if (use) {
        let tree = use[2];
        if (tree.includes("{") && !tree.includes("}")) {
          for (let next = index + 1; next < sourceLines.length && next < index + 200; next += 1) {
            tree += ` ${sourceLines[next]}`;
            if (sourceLines[next].includes("}")) break;
          }
        }
        tree = tree.replace(/;.*$/, "").trim();
        const brace = tree.indexOf("::{");
        const prefix = brace < 0 ? tree : tree.slice(0, brace);
        const rawPath = use[2].split(";")[0];
        const braceAt = rawPath.indexOf("::{");
        const visible = braceAt < 0 ? rawPath.trimEnd() : rawPath.slice(0, braceAt);
        codeImportEntry(lines, number, use[1].length, use[1].length + visible.length, brace < 0 ? `use ${tree}` : `use ${prefix}`);
        const members = brace < 0 ? [tree.split("::").pop()] : tree.slice(brace + 3).replace(/}.*$/, "").split(",");
        for (const member of members) {
          const [path, alias] = member.trim().split(/\s+as\s+/);
          if (!path || path === "*" || path === "self") continue;
          const full = brace < 0 ? tree.split(/\s+as\s+/)[0] : `${prefix}::${path.trim()}`;
          const leaf = full.split("::").pop();
          bind((alias || leaf).trim(), `use ${full}`, null);
        }
      }
    } else if (lang === "go") {
      if (/^\s*import\s*\(\s*$/.test(line)) { goImportGroup = true; return; }
      if (goImportGroup && /^\s*\)/.test(line)) { goImportGroup = false; return; }
      const single = /^(\s*import\s+)(?:([A-Za-z_.]\w*)\s+)?("([^"\n]+)")/.exec(line);
      const grouped = /^(\s*)(?:([A-Za-z_.]\w*)\s+)?("([^"\n]+)")\s*$/.exec(line);
      const match = single || (goImportGroup ? grouped : null);
      if (match) {
        const start = line.indexOf(match[3]) + 1;
        codeImportEntry(lines, number, start, start + match[4].length, match[3]);
        const leaf = match[4].split("/").filter((part) => !/^v\d+$/.test(part)).pop()?.replace(/[^A-Za-z0-9_]/g, "_");
        const alias = match[2] && match[2] !== "." && match[2] !== "_" ? match[2] : leaf;
        if (alias) bind(alias, match[3], null);
      }
    } else if (lang === "c" || lang === "cpp") {
      const include = /^(\s*#\s*include\s*)(["<])([^">\n]+)[">]/.exec(line);
      if (include) codeImportEntry(lines, number, include[1].length + 1, include[1].length + 1 + include[3].length, `#include ${include[2]}${include[3]}${include[2] === "<" ? ">" : "\""}`);
    } else if (lang === "ruby") {
      const require = /^(\s*require(?:_relative)?\s*\(?\s*)(['"])([^'"\n]+)\2/.exec(line);
      if (require) codeImportEntry(lines, number, require[1].length + 1, require[1].length + 1 + require[3].length, `${line.trim().startsWith("require_relative") ? "require_relative" : "require"} '${require[3]}'`);
    } else if (lang === "php") {
      const use = /^(\s*use\s+)([\w\\]+)(?:\s+as\s+(\w+))?\s*;/.exec(line);
      if (use) {
        codeImportEntry(lines, number, use[1].length, use[1].length + use[2].length, `use ${use[2]}`);
        const leaf = use[2].split("\\").pop();
        bind(use[3] || leaf, `use ${use[2]}`, leaf);
      }
    } else if (lang === "css" || lang === "scss") {
      const imported = /^(\s*@(?:import|use|forward)\s+(?:url\()?)(['"])([^'"\n]+)\2/.exec(line);
      if (imported) codeImportEntry(lines, number, imported[1].length + 1, imported[1].length + 1 + imported[3].length, imported[3]);
    } else if (lang === "protobuf") {
      const imported = /^(\s*import\s+(?:public\s+|weak\s+)?)(")([^"\n]+)"/.exec(line);
      if (imported) codeImportEntry(lines, number, imported[1].length + 1, imported[1].length + 1 + imported[3].length, imported[3]);
    }
  });
  if (["typescript", "tsx", "javascript", "jsx"].includes(lang)) {
    const clauses = /\b(?:import|export)\s+(?:type\s+)?([\w$*{},\s]+?)\s+from\s+(['"])([^'"\n]+)\2/g;
    for (const match of text.matchAll(clauses)) {
      const clause = match[1];
      const spec = match[3];
      const namespace = /\*\s+as\s+([\w$]+)/.exec(clause);
      if (namespace) bind(namespace[1], spec, null);
      const braces = /\{([^}]*)\}/.exec(clause);
      if (braces) {
        for (const part of braces[1].split(",")) {
          const [name, alias] = part.trim().replace(/^type\s+/, "").split(/\s+as\s+/);
          if (name) bind((alias || name).trim(), spec, name.trim());
        }
      }
      const defaultName = /^\s*([\w$]+)\s*(?:,|$)/.exec(clause.replace(/\{[^}]*\}/, "").replace(/\*\s+as\s+[\w$]+/, ""));
      if (defaultName && defaultName[1] !== "type") bind(defaultName[1], spec, defaultName[1]);
    }
    for (const match of text.matchAll(/\b(?:const|let|var)\s+(\{[^}]*\}|[\w$]+)\s*=\s*require\s*\(\s*(['"])([^'"\n]+)\2\s*\)/g)) {
      if (match[1].startsWith("{")) {
        for (const part of match[1].slice(1, -1).split(",")) {
          const [name, alias] = part.trim().split(/\s*:\s*/);
          if (name) bind((alias || name).trim(), match[3], name.trim());
        }
      } else bind(match[1], match[3], null);
    }
  }
  return { names, lines };
}

// ---------------------------------------------------------------------------
// Same-file definitions

const CODE_DECLARATION_KEYWORDS = codeWords("class interface enum struct trait fn func function def fun type typealias const let var val record object mod module namespace protocol macro_rules message service rpc input scalar union actor");

/// Lines in an already tokenized file that declare `symbol`, found from the
/// tokens around each occurrence. Local variables count here; the owner-side
/// search deliberately ignores them.
function localDefinitionLines(tokenLines, language, symbol) {
  if (!codeSymbolValid(symbol) || !Array.isArray(tokenLines)) return [];
  const typed = ["java", "groovy", "c", "cpp", "csharp"].includes(language);
  const found = [];
  tokenLines.forEach((segments, index) => {
    const significant = [];
    let column = 0;
    for (const segment of segments) {
      if (segment.kind !== "plain" || segment.text.trim()) {
        significant.push({ ...segment, column, trimmed: segment.text.trim() });
      }
      column += segment.text.length;
    }
    const lineText = segments.map((segment) => segment.text).join("");
    significant.forEach((token, position) => {
      if (token.text !== symbol || token.kind === "comment" || token.kind === "string") return;
      const previous = significant[position - 1];
      const next = significant[position + 1];
      const before = lineText.slice(0, token.column).trimEnd();
      const after = lineText.slice(token.column + symbol.length).trimStart();
      if (previous && previous.kind === "keyword" && CODE_DECLARATION_KEYWORDS.has(previous.trimmed)) {
        found.push({ line: index + 1, column: token.column + 1 });
        return;
      }
      if (language === "rust" && /macro_rules!\s*$/.test(before)) { found.push({ line: index + 1, column: token.column + 1 }); return; }
      if (language === "python" && /^\s*$/.test(before) && /^=(?!=)/.test(after)) { found.push({ line: index + 1, column: token.column + 1 }); return; }
      if (language === "go" && (/^func\s*\([^)]*\)\s*$/.test(before.trim()) || (/^\s*$/.test(before) && /^:=/.test(after)))) { found.push({ line: index + 1, column: token.column + 1 }); return; }
      if (typed && previous && ["type", "identifier", "keyword"].includes(previous.kind)
        && !/[.=(,]$/.test(before) && (previous.kind !== "keyword" || /^(?:int|long|short|char|byte|double|float|boolean|bool|void|var|auto|unsigned|signed|string|object|decimal|uint|ulong|ushort|sbyte)$/.test(previous.trimmed))
        && /^(?:[;=,({]|$)/.test(after) && !/^==/.test(after)) {
        found.push({ line: index + 1, column: token.column + 1 });
        return;
      }
      if (["typescript", "tsx", "javascript", "jsx", "java", "kotlin", "csharp", "php"].includes(language)
        && /^\s*(?:(?:public|private|protected|static|async|get|set|readonly|abstract|override|export|default)\s+)*$/.test(before)
        && /^\([^)]*\)\s*(?::[^={;]+)?\{/.test(after)) {
        found.push({ line: index + 1, column: token.column + 1 });
        return;
      }
      if (["typescript", "tsx", "javascript", "jsx"].includes(language) && next?.trimmed === "=>" && /^\s*$/.test(before)) {
        found.push({ line: index + 1, column: token.column + 1 });
      }
    });
  });
  return found;
}

/// Chooses the declaration a reader most likely means: the nearest one above
/// the clicked line (locals shadow outer names), otherwise the first.
function chooseLocalDefinition(definitions, line) {
  if (!Array.isArray(definitions) || !definitions.length) return null;
  const above = definitions.filter((definition) => definition.line <= line);
  return above.length ? above[above.length - 1] : definitions[0];
}

/// The qualifier before a clicked identifier: `ns` in `ns.name` or
/// `module::name`.
function symbolQualifier(lineText, column) {
  const before = String(lineText || "").slice(0, Math.max(0, column - 1));
  const match = /([A-Za-z_$][\w$]*)\s*(?:\.|::|->)\s*$/.exec(before);
  return match ? match[1] : null;
}

// ---------------------------------------------------------------------------
// Owner requests

function paneCodePath(paneId, operation, params = {}) {
  if (!paneId || !CODE_NAV_OPERATIONS.has(operation)) return null;
  const query = new URLSearchParams();
  for (const key of ["symbol", "path", "spec"]) {
    const value = params[key];
    if (value === undefined || value === null || value === "") continue;
    if (key === "symbol" && !codeSymbolValid(value)) return null;
    if (key === "path" && !projectRelativePath(value)) return null;
    if (key === "spec" && (String(value).length > 512 || /[\u0000-\u001f\u007f]/.test(String(value)))) return null;
    query.set(key, String(value));
  }
  if (operation !== "resolve" && !query.has("symbol")) return null;
  if (operation === "resolve" && (!query.has("path") || !query.has("spec"))) return null;
  return `/api/v1/panes/${encodeURIComponent(String(paneId))}/code/${operation}?${query}`;
}

/// Normalizes an owner response: only project-relative paths, positive
/// positions, bounded kinds and previews.
function codeNavResults(data) {
  const results = (Array.isArray(data?.results) ? data.results : []).slice(0, MAX_CODE_NAV_RESULTS)
    .map((result) => {
      const path = projectRelativePath(result?.path);
      const line = Number(result?.line);
      const column = Number(result?.column);
      if (!path || !Number.isInteger(line) || line < 1) return null;
      return {
        path,
        line,
        column: Number.isInteger(column) && column >= 1 ? column : 1,
        kind: typeof result?.kind === "string" ? result.kind.replace(/[^a-z_-]/g, "").slice(0, 24) || "match" : "match",
        preview: typeof result?.preview === "string" ? result.preview.replace(/[\u0000-\u001f\u007f]/g, " ").slice(0, 240) : "",
      };
    })
    .filter(Boolean);
  return { results, truncated: Boolean(data?.truncated) };
}

/// Back/forward stacks for the file viewer.
function pushCodeHistory(history, entry) {
  const back = [...(history?.back || []), entry].slice(-MAX_CODE_NAV_HISTORY);
  return { back, forward: [] };
}

function stepCodeHistory(history, direction, current) {
  const back = [...(history?.back || [])];
  const forward = [...(history?.forward || [])];
  const from = direction === "back" ? back : forward;
  const to = direction === "back" ? forward : back;
  const target = from.pop();
  if (!target) return { history: { back, forward }, target: null };
  if (current) to.push(current);
  return { history: { back: back.slice(-MAX_CODE_NAV_HISTORY), forward: forward.slice(-MAX_CODE_NAV_HISTORY) }, target };
}

function projectRelativePath(value) {
  if (value === "" || value == null) return "";
  const path = String(value);
  if (path.length > 4096 || path.startsWith("/") || path.includes("\\")
    || /[\u0000-\u001f\u007f]/.test(path)) return null;
  const parts = path.split("/");
  return parts.every((part) => part && part !== "." && part !== "..") ? path : null;
}

function fileReaderPreferences(value, mobile = false) {
  const defaults = mobile
    ? { wrap: true, size: "small" }
    : { wrap: false, size: "medium" };
  let stored = value;
  if (typeof value === "string") {
    try { stored = JSON.parse(value); } catch { return defaults; }
  }
  if (!stored || typeof stored !== "object" || Array.isArray(stored)) return defaults;
  return {
    wrap: typeof stored.wrap === "boolean" ? stored.wrap : defaults.wrap,
    size: FILE_READER_SIZES.has(stored.size) ? stored.size : defaults.size,
  };
}

function fileReaderPreferenceJson(preferences) {
  const normalized = fileReaderPreferences(preferences, false);
  return JSON.stringify({ wrap: normalized.wrap, size: normalized.size });
}

function conversationVisibilityPreferences(value) {
  const defaults = { human: true, internal: true };
  let stored = value;
  if (typeof value === "string") {
    try { stored = JSON.parse(value); } catch { return defaults; }
  }
  if (!stored || typeof stored !== "object" || Array.isArray(stored)) return defaults;
  return {
    human: typeof stored.human === "boolean" ? stored.human : defaults.human,
    internal: typeof stored.internal === "boolean" ? stored.internal : defaults.internal,
  };
}

function conversationVisibilityPreferenceJson(preferences) {
  const normalized = conversationVisibilityPreferences(preferences);
  return JSON.stringify({ human: normalized.human, internal: normalized.internal });
}

function loadConversationVisibilityPreferences(readStoredValue) {
  try {
    return conversationVisibilityPreferences(readStoredValue());
  } catch {
    return conversationVisibilityPreferences(null);
  }
}

function saveConversationVisibilityPreferences(writeStoredValue, preferences) {
  try {
    return writeStoredValue(conversationVisibilityPreferenceJson(preferences)) !== false;
  } catch {
    return false;
  }
}

function loadFileReaderPreferences(readStoredValue, mobile = false) {
  try {
    return fileReaderPreferences(readStoredValue(), mobile);
  } catch {
    return fileReaderPreferences(null, mobile);
  }
}

function paneFilesPath(paneId, path = "") {
  const relative = projectRelativePath(path);
  if (!paneId || relative === null) return null;
  return `/api/v1/panes/${encodeURIComponent(String(paneId))}/files?path=${encodeURIComponent(relative)}`;
}

function validContentHash(value) {
  return typeof value === "string" && CONTENT_HASH_PATTERN.test(value);
}

function fileCanEdit(file) {
  return Boolean(file)
    && typeof file.content === "string"
    && !file.binary
    && !file.truncated
    && validContentHash(file.contentHash);
}

function fileEditHasUnsavedWork(files) {
  return Boolean(files?.editing && files.file)
    && (files.saving || files.reloading || files.conflict
      || String(files.editDraft ?? "") !== String(files.file.content ?? ""));
}

function reconcileSavedFileDraft(sentContent, currentDraft, savedFile) {
  const sent = String(sentContent ?? "");
  const draft = String(currentDraft ?? "");
  return {
    file: savedFile,
    editDraft: draft === sent ? String(savedFile?.content ?? "") : draft,
    editing: draft !== sent,
  };
}

/// Two taps on line numbers form a mobile-friendly inclusive range. Once a
/// range exists, a normal tap starts a fresh selection; Shift always extends
/// the original anchor for desktop readers.
function nextFileLineSelection(current, line, extend = false) {
  if (!Number.isInteger(line) || line < 1) return current || null;
  const selection = current && Number.isInteger(current.anchor)
    && Number.isInteger(current.start) && Number.isInteger(current.end)
    ? current : null;
  const anchor = selection && (extend || selection.start === selection.end)
    ? selection.anchor : line;
  return { anchor, start: Math.min(anchor, line), end: Math.max(anchor, line) };
}

function fileReferenceBlock(path, language, content, selection) {
  const relative = projectRelativePath(path);
  if (!relative || typeof content !== "string" || !selection) return null;
  const allLines = content.split("\n");
  const requestedStart = Math.max(1, Math.min(allLines.length, Number(selection.start) || 1));
  const requestedEnd = Math.max(requestedStart, Math.min(allLines.length, Number(selection.end) || requestedStart));
  const end = Math.min(requestedEnd, requestedStart + MAX_FILE_REFERENCE_LINES - 1);
  const chosen = allLines.slice(requestedStart - 1, end);
  let excerpt = chosen.join("\n");
  let truncated = end < requestedEnd;
  const characters = Array.from(excerpt);
  if (characters.length > MAX_FILE_REFERENCE_CHARS) {
    excerpt = characters.slice(0, MAX_FILE_REFERENCE_CHARS).join("");
    truncated = true;
  }
  if (truncated) excerpt = `${excerpt}\n… [selection truncated by atmux]`;
  const longestFence = Math.max(0, ...[...excerpt.matchAll(/`+/g)].map((match) => match[0].length));
  const fence = "`".repeat(Math.max(3, longestFence + 1));
  const labelEnd = end < requestedEnd ? `${end} of ${requestedEnd}` : String(requestedEnd);
  const label = requestedStart === requestedEnd
    ? `${relative}:${requestedStart}`
    : `${relative}:${requestedStart}-${labelEnd}`;
  const safeLanguage = sourceLanguage(relative, language).replace(/[^a-z0-9_+-]/g, "") || "text";
  return `Selected \`${label.replace(/`/g, "\\`")}\`:\n\n${fence}${safeLanguage}\n${excerpt}\n${fence}`;
}

/// Inserts without replacing any part of the current draft. This matters when
/// the composer itself has a selection: referencing source must never destroy
/// text the user already wrote.
function insertComposerReference(draft, cursor, reference) {
  const value = String(draft ?? "");
  const block = String(reference ?? "");
  const at = Number.isInteger(cursor) ? Math.max(0, Math.min(value.length, cursor)) : value.length;
  const before = value.slice(0, at);
  const after = value.slice(at);
  const prefix = before && !before.endsWith("\n\n") ? (before.endsWith("\n") ? "\n" : "\n\n") : "";
  const suffix = after && !after.startsWith("\n\n") ? (after.startsWith("\n") ? "\n" : "\n\n") : "";
  const inserted = `${prefix}${block}${suffix}`;
  return { value: `${before}${inserted}${after}`, cursor: before.length + inserted.length };
}

function paneGitPath(paneId, path = null) {
  if (!paneId) return null;
  const base = `/api/v1/panes/${encodeURIComponent(String(paneId))}/git`;
  if (path === null) return base;
  const relative = projectRelativePath(path);
  return relative ? `${base}?path=${encodeURIComponent(relative)}` : null;
}

function projectEntryKind(entry) {
  const kind = String(entry?.kind || entry?.type || "").toLowerCase();
  if (kind === "directory" || kind === "dir" || entry?.is_dir === true) return "directory";
  return kind === "file" || entry?.is_dir === false ? "file" : null;
}

function sourceLanguage(path, hint = "") {
  const declared = String(hint || "").trim().toLowerCase().replace(/[^a-z0-9_+-]/g, "");
  if (declared) return declared;
  const name = String(path || "").toLowerCase();
  const base = name.split("/").pop() || "";
  if (["dockerfile", "containerfile"].includes(base)) return "dockerfile";
  if (["makefile", "gnumakefile"].includes(base)) return "makefile";
  const extension = base.includes(".") ? base.split(".").pop() : "";
  return ({
    c: "c", h: "c", cc: "cpp", cpp: "cpp", cxx: "cpp", hpp: "cpp",
    cs: "csharp", css: "css", go: "go", html: "html", htm: "html",
    java: "java", js: "javascript", cjs: "javascript", mjs: "javascript",
    json: "json", jsx: "jsx", kt: "kotlin", kts: "kotlin", lua: "lua",
    md: "markdown", py: "python", rb: "ruby", rs: "rust", sh: "shell",
    bash: "shell", sql: "sql", toml: "toml", ts: "typescript", tsx: "tsx",
    xml: "xml", yaml: "yaml", yml: "yaml", diff: "diff", patch: "diff",
  })[extension] || "text";
}

function projectFilePreview(data, path) {
  const raw = typeof data?.content === "string" ? data.content : null;
  const binary = data?.binary === true;
  return {
    path,
    content: !binary && raw !== null ? raw.slice(0, MAX_PROJECT_SOURCE_CHARS) : null,
    binary,
    size: Number(data?.size),
    language: sourceLanguage(path, data?.language),
    contentHash: validContentHash(data?.content_hash) ? data.content_hash : null,
    lineCount: Number.isInteger(data?.line_count) && data.line_count >= 0 ? data.line_count : null,
    truncated: !binary && (Boolean(data?.truncated) || (raw !== null
      && (raw.length > MAX_PROJECT_SOURCE_CHARS
        || raw.split("\n", MAX_PROJECT_SOURCE_LINES + 1).length > MAX_PROJECT_SOURCE_LINES))),
  };
}

function diffLineKind(line) {
  const value = String(line || "");
  if (value.startsWith("@@")) return "hunk";
  if (value.startsWith("+") && !value.startsWith("+++")) return "added";
  if (value.startsWith("-") && !value.startsWith("---")) return "removed";
  if (/^(diff |index |--- |\+\+\+ )/.test(value)) return "meta";
  return "context";
}

function appendInlineMarkdown(parent, text) {
  for (const token of inlineTokens(text)) {
    if (token.type === "text") linkifyInto(parent, token.text);
    else if (token.type === "break") parent.append(document.createElement("br"));
    else if (token.type === "code") {
      const code = document.createElement("code"); code.textContent = token.text; parent.append(code);
    } else {
      const tag = token.type === "strong" ? "strong"
        : token.type === "emphasis" ? "em"
          : token.type === "strike" ? "s" : "a";
      const node = document.createElement(tag);
      if (token.type === "link") {
        const url = safeLinkUrl(token.url, location.href);
        if (!url) {
          node.removeAttribute("href");
          node.className = "unsafe-link";
          node.title = "Blocked non-HTTP link";
        } else {
          node.href = url;
          node.target = "_blank";
          node.rel = "noopener noreferrer";
        }
      }
      for (const child of token.children || []) appendInlineToken(node, child);
      parent.append(node);
    }
  }
}

function appendInlineToken(parent, token) {
  // Never nest an autolink inside an explicit markdown link.
  if (token.type === "text") {
    if (parent.closest?.("a")) parent.append(document.createTextNode(token.text));
    else linkifyInto(parent, token.text);
    return;
  }
  if (token.type === "break") { parent.append(document.createElement("br")); return; }
  if (token.type === "code") {
    const code = document.createElement("code"); code.textContent = token.text; parent.append(code); return;
  }
  const tag = token.type === "strong" ? "strong"
    : token.type === "emphasis" ? "em"
      : token.type === "strike" ? "s" : "span";
  const node = document.createElement(tag);
  for (const child of token.children || []) appendInlineToken(node, child);
  parent.append(node);
}

function markdownFragment(markdown) {
  const fragment = document.createDocumentFragment();
  for (const block of markdownBlocks(markdown)) {
    if (block.type === "code") {
      const details = document.createElement("details");
      details.className = "code-block";
      const lines = block.text ? block.text.split("\n").length : 0;
      details.open = lines <= 12;
      const summary = document.createElement("summary");
      summary.textContent = `${block.language || "code"} · ${lines} line${lines === 1 ? "" : "s"}`;
      const pre = document.createElement("pre");
      const code = document.createElement("code");
      code.className = `language-${String(block.language || "text").replace(/[^a-z0-9_+-]/gi, "")}`;
      for (const segment of highlightCode(block.text, block.language || "generic")) {
        const span = document.createElement("span");
        span.className = segment.kind === "plain" ? "" : `syntax-${segment.kind}`;
        span.textContent = segment.text;
        code.append(span);
      }
      pre.append(code); details.append(summary, pre); fragment.append(details); continue;
    }
    if (block.type === "rule") { fragment.append(document.createElement("hr")); continue; }
    if (block.type === "quote") {
      const quote = document.createElement("blockquote");
      for (const child of block.children) quote.append(markdownFragmentFromBlock(child));
      fragment.append(quote); continue;
    }
    if (block.type === "list") {
      const list = document.createElement(block.ordered ? "ol" : "ul");
      for (const item of block.items) {
        const entry = document.createElement("li"); appendInlineMarkdown(entry, item); list.append(entry);
      }
      fragment.append(list); continue;
    }
    if (block.type === "table") {
      const wrapper = document.createElement("div"); wrapper.className = "table-scroll";
      const table = document.createElement("table");
      block.rows.forEach((row, rowIndex) => {
        const tr = document.createElement("tr");
        for (const cell of row) {
          const node = document.createElement(rowIndex === 0 ? "th" : "td");
          appendInlineMarkdown(node, cell); tr.append(node);
        }
        table.append(tr);
      });
      wrapper.append(table); fragment.append(wrapper); continue;
    }
    fragment.append(markdownFragmentFromBlock(block));
  }
  return fragment;
}

function markdownFragmentFromBlock(block) {
  const node = document.createElement(block.type === "heading" ? `h${block.level}` : "p");
  appendInlineMarkdown(node, block.text || "");
  return node;
}

function reduceTranscript(current, data) {
  const available = Boolean(data?.available);
  const source = data?.source || "agent";
  // The owner explains unusual mapping states (a CLI still at a startup
  // prompt, a session with no messages yet). Bounded plain text only.
  const note = typeof data?.note === "string" && data.note.trim()
    ? data.note.trim().slice(0, MAX_TRANSCRIPT_NOTE_CHARS) : "";
  if (!available) {
    return {
      hash: "",
      transcript: {
        available: false,
        source,
        messages: [],
        truncated: false,
        error: null,
        note,
      },
    };
  }
  return {
    hash: data.content_hash || "",
    transcript: {
      available: true,
      source,
      messages: data.changed && Array.isArray(data.messages)
        ? data.messages
        : current.messages,
      truncated: data.changed ? Boolean(data.truncated) : current.truncated,
      error: null,
      note,
    },
  };
}

const MAX_TRANSCRIPT_NOTE_CHARS = 400;

/// Terminal-cell bounds for fitting a detached tmux window to the raw view.
/// The owner accepts 40-400 columns and 10-200 rows; below 60 columns a
/// full-screen agent becomes unreadable, so a phone keeps at least that.
const RAW_FIT_MIN_COLS = 60;
const RAW_FIT_MAX_COLS = 300;
const RAW_FIT_MIN_ROWS = 16;
const RAW_FIT_MAX_ROWS = 150;

/// How many terminal cells fit the raw view's content box.
function rawPaneGrid({ width, height, charWidth, lineHeight } = {}) {
  if (![width, height, charWidth, lineHeight].every((value) => Number.isFinite(value) && value > 0)) return null;
  const clamp = (value, low, high) => Math.max(low, Math.min(high, value));
  return {
    cols: clamp(Math.floor(width / charWidth), RAW_FIT_MIN_COLS, RAW_FIT_MAX_COLS),
    rows: clamp(Math.floor(height / lineHeight), RAW_FIT_MIN_ROWS, RAW_FIT_MAX_ROWS),
  };
}

/// One line for a collapsed compaction: what happened, how it was started,
/// and how much context it reclaimed when the CLI recorded that.
function compactionSummaryLabel(message) {
  const detail = message?.compaction && typeof message.compaction === "object" ? message.compaction : {};
  const parts = ["Conversation compacted"];
  if (typeof detail.trigger === "string" && /^[a-z0-9_-]{1,32}$/i.test(detail.trigger)) parts.push(detail.trigger);
  const tokens = (value) => Number.isSafeInteger(value) && value >= 0 ? value : null;
  const before = tokens(detail.pre_tokens);
  const after = tokens(detail.post_tokens);
  if (before !== null && after !== null) parts.push(`${formatTokenCount(before)} \u2192 ${formatTokenCount(after)} tokens`);
  else if (before !== null) parts.push(`${formatTokenCount(before)} tokens before`);
  if (typeof message?.markdown === "string" && message.markdown.trim()) parts.push("summary");
  return parts.join(" \u00b7 ");
}

/// One request at a time: activity cannot postpone an already scheduled read
/// or invalidate a slow response. The next idle poll starts after completion,
/// while activity during a read coalesces into one bounded follow-up.
function createTranscriptPoller({
  load,
  onData,
  onError,
  setTimer = setTimeout,
  clearTimer = clearTimeout,
  now = Date.now,
  intervalMs = 2500,
  minRefreshMs = 750,
  timeoutMs = 15_000,
}) {
  let closed = false;
  let timer = null;
  let due = Infinity;
  let controller = null;
  let timeout = null;
  let pending = false;
  let lastStarted = -Infinity;

  function schedule(delay = 300) {
    if (closed) return;
    if (controller) { pending = true; return; }
    const next = Math.max(now() + delay, lastStarted + minRefreshMs);
    if (timer !== null && due <= next) return;
    if (timer !== null) clearTimer(timer);
    due = next;
    timer = setTimer(() => { void refresh(); }, Math.max(0, next - now()));
  }

  async function refresh() {
    timer = null;
    due = Infinity;
    if (closed) return;
    controller = new AbortController();
    const active = controller;
    let timedOut = false;
    lastStarted = now();
    const deadline = new Promise((_, reject) => {
      timeout = setTimer(() => {
        timedOut = true;
        active.abort();
        reject(new Error("Conversation update timed out; retrying automatically."));
      }, timeoutMs);
    });
    try {
      const data = await Promise.race([load(active.signal), deadline]);
      if (!closed) onData(data);
    } catch (error) {
      if (!closed) onError(timedOut
        ? new Error("Conversation update timed out; retrying automatically.") : error);
    } finally {
      clearTimer(timeout);
      timeout = null;
      controller = null;
      if (!closed) {
        const delay = pending ? minRefreshMs : intervalMs;
        pending = false;
        schedule(delay);
      }
    }
  }

  return {
    schedule,
    close() {
      closed = true;
      if (timer !== null) clearTimer(timer);
      if (timeout !== null) clearTimer(timeout);
      timer = null;
      timeout = null;
      controller?.abort();
    },
  };
}

/// A Claude Code subagent writes its prompts and reports back with the user
/// role. Labelling those "You" credits the operator with an agent's words, so
/// the subagent is named ahead of the visibility bucket it shares with the
/// other internal entries.
function transcriptRoleLabel(message) {
  if (message?.role === "subagent") {
    const name = String(message?.agent_name || "").trim();
    return name ? `Subagent · ${name}` : "Subagent";
  }
  const visibility = transcriptVisibilityKind(message);
  return visibility === "human" ? "You" : visibility === "agent" ? "Agent" : "Internal";
}

function transcriptItemKind(item) {
  return item?.kind === "tool" || item?.role === "tool" ? "tool" : "message";
}

/// Conversation visibility is deliberately role-based and fail-closed. Only
/// ordinary assistant prose is Agent text, and only ordinary user prose is
/// Human text. Tool calls plus future system/status/coordination records are
/// Internal, so a new transcript shape cannot leak into the wrong filter.
function transcriptVisibilityKind(item) {
  if (transcriptItemKind(item) === "tool") return "internal";
  const kind = typeof item?.kind === "string" && item.kind ? item.kind : "message";
  if (kind === "message" && item?.role === "assistant") return "agent";
  if (kind === "message" && item?.role === "user") return "human";
  return "internal";
}

function transcriptItemIsVisible(item, preferences) {
  const visibility = transcriptVisibilityKind(item);
  if (visibility === "agent") return true;
  const normalized = conversationVisibilityPreferences(preferences);
  return visibility === "human" ? normalized.human : normalized.internal;
}

function filterTranscriptMessages(messages, preferences) {
  return (Array.isArray(messages) ? messages : [])
    .filter((message) => transcriptItemIsVisible(message, preferences));
}

function normalizedToolName(item) {
  const raw = String(item?.tool_name || "Tool").trim() || "Tool";
  const lower = raw.toLowerCase();
  for (const name of [
    ...COLLAPSIBLE_COORDINATION_TOOLS,
    ...INTERNAL_TOOL_ALIASES.keys(),
  ]) {
    if (lower === name || lower.endsWith(`.${name}`) || lower.endsWith(`/${name}`)
      || lower.endsWith(`:${name}`) || lower.endsWith(`__${name}`)) {
      return INTERNAL_TOOL_ALIASES.get(name) || name;
    }
  }
  return lower;
}

function coordinationResultSignal(item) {
  const output = typeof item?.tool_output === "string" ? item.tool_output.trim() : "";
  if (!output) return "sent";
  if (/(?:\b(?:error|failed|failure|denied|blocked|rejected|exception|unauthori[sz]ed|forbidden|unavailable|invalid|cancelled|canceled)\b|not[_ -]?found)/i.test(output)) return "error";
  if (/\b(?:approval|approve|confirm|permission)\b/i.test(output)) return "approval";
  if (/^(?:ok|sent|queued|delivered|acknowledged|waiting|idle|running|complete(?:d)?|timed?\s*out|timeout|no updates?|no activity)[.!]?$/i.test(output)) {
    return "status";
  }
  try {
    const value = JSON.parse(output);
    if (coordinationStatusJson(value)) return "status";
    if (coordinationStatusJsonHasInvalidPrimitive(value)) return "error";
  } catch { /* Plain status text is handled above. */ }
  return "meaningful";
}

function coordinationStatusJson(value, depth = 0, key = "") {
  if (depth > 4) return false;
  if (/^(?:status|state)$/.test(key)) {
    if (typeof value !== "string") return false;
    return BENIGN_COORDINATION_STATUSES.has(value.trim().toLowerCase().replace(/[.!]$/, ""));
  }
  if (value === null || typeof value === "boolean" || typeof value === "number") return true;
  if (typeof value === "string") {
    const normalized = value.trim().toLowerCase().replace(/[.!]$/, "");
    if (!key) return BENIGN_COORDINATION_STATUSES.has(normalized);
    if (/^(?:id|agent_id|target|task|task_name|name|path|parent|model|reasoning_effort|started_at|finished_at|updated_at)$/.test(key)) {
      return value.length <= 160 && /^[a-z0-9_./:%+~-]+$/i.test(value);
    }
    if (/^(?:completed|running|waiting|idle)$/.test(key)) {
      return BENIGN_COORDINATION_STATUSES.has(normalized) || (value.length <= 160 && /^[a-z0-9_./:%+~-]+$/i.test(value));
    }
    return false;
  }
  if (Array.isArray(value)) return value.length <= 32 && value.every((entry) => coordinationStatusJson(entry, depth + 1, key));
  if (typeof value !== "object") return false;
  const entries = Object.entries(value);
  if (entries.length > 32) return false;
  const safeKeys = /^(?:status|state|id|agent_id|agents|target|task|tasks|task_name|name|path|parent|children|model|reasoning_effort|started_at|finished_at|updated_at|count|total|delivered|queued|acknowledged|timeout|timed_out|completed|running|waiting|idle)$/;
  return entries.every(([childKey, entry]) => safeKeys.test(childKey)
    && coordinationStatusJson(entry, depth + 1, childKey));
}

function coordinationStatusJsonHasInvalidPrimitive(value, depth = 0) {
  if (depth > 4 || value === null || typeof value !== "object") return false;
  if (Array.isArray(value)) return value.some((entry) => coordinationStatusJsonHasInvalidPrimitive(entry, depth + 1));
  return Object.entries(value).some(([key, entry]) => (
    /^(?:status|state)$/.test(key) && typeof entry !== "string"
  ) || coordinationStatusJsonHasInvalidPrimitive(entry, depth + 1));
}

function collapsibleCoordinationTool(item) {
  return internalToolGroupKey(item) === "coordination";
}

function execJsonResultClass(value, depth = 0) {
  if (depth > 5 || value === null || typeof value !== "object") return null;
  if (Array.isArray(value)) {
    const results = value.slice(0, 64).map((entry) => execJsonResultClass(entry, depth + 1));
    if (results.includes("error")) return "error";
    if (results.includes("success")) return "success";
    return results.includes("pending") ? "pending" : null;
  }
  let observed = null;
  for (const [key, entry] of Object.entries(value).slice(0, 64)) {
    if (/^(?:exit_code|exitCode|exit_status)$/.test(key)
      && (typeof entry === "number" || (typeof entry === "string" && /^-?\d+$/.test(entry.trim())))) {
      const code = Number(entry);
      if (!Number.isFinite(code) || code !== 0) return "error";
      observed = "success";
      continue;
    }
    if (/^(?:status|code)$/.test(key)
      && (typeof entry === "number" || (typeof entry === "string" && /^-?\d+$/.test(entry.trim())))) {
      const code = Number(entry);
      if (!Number.isFinite(code) || code !== 0) return "error";
      // A generic zero code is not enough to prove process success.
      continue;
    }
    if ((key === "is_error" && entry === true)
      || ((key === "success" || key === "ok") && entry === false)) return "error";
    if ((key === "success" || key === "ok") && entry === true) observed = "success";
    if (/^(?:status|state)$/.test(key) && typeof entry === "string") {
      const status = entry.trim().toLowerCase().replace(/[.!]$/, "");
      if (/^(?:error|failed|failure|timed out|timeout|cancelled|canceled|rejected)$/.test(status)) return "error";
      if (/^(?:ok|success|succeeded|complete|completed)$/.test(status)) observed = "success";
      else if (/^(?:pending|queued|running|waiting)$/.test(status) && !observed) observed = "pending";
    }
    const nested = execJsonResultClass(entry, depth + 1);
    if (nested === "error") return "error";
    if (nested === "success") observed = "success";
    else if (nested === "pending" && !observed) observed = "pending";
  }
  return observed;
}

function execResultClass(item) {
  if (normalizedToolName(item) !== "exec") return null;
  const output = typeof item?.tool_output === "string" ? item.tool_output.trim() : "";
  if (!output) return null;
  if (/\b(?:timed?\s*out|timeout)\b/i.test(output)
    || coordinationResultSignal(item) === "error") return "error";

  let jsonResult = null;
  try { jsonResult = execJsonResultClass(JSON.parse(output)); } catch { /* Plain tool output. */ }
  if (jsonResult === "error") return "error";

  const exitCodes = [...output.matchAll(/(?:\b(?:process|command|script)\s+exited\s+with\s+(?:code|status)|\bexit(?:ed)?[_ -]+(?:code|status))[\s:=]*(-?\d+)\b/gi)]
    .map((match) => Number(match[1]));
  if (exitCodes.some((code) => !Number.isFinite(code) || code !== 0)) return "error";
  if (exitCodes.length || jsonResult === "success") return "success";
  if (jsonResult === "pending") return "pending";
  if (/^(?:ok|success|succeeded|complete|completed)[.!]?$/i.test(output)) return "success";
  if (/^(?:pending|queued|running|waiting)[.!]?$/i.test(output)) return "pending";
  // Command output alone does not prove the tool call completed successfully.
  return null;
}

function toolResultSignal(item) {
  const execClass = execResultClass(item);
  if (execClass === "error") return "error";
  if (execClass === "success" || execClass === "pending") return "status";
  return coordinationResultSignal(item);
}

function internalToolGroupKey(item) {
  if (transcriptItemKind(item) !== "tool") return null;
  if (typeof item?.tool_name !== "string" || !item.tool_name.trim()) return null;
  const name = normalizedToolName(item);
  if (COLLAPSIBLE_COORDINATION_TOOLS.has(name)) {
    const signal = coordinationResultSignal(item);
    return ["sent", "status"].includes(signal) ? "coordination" : null;
  }
  if (name !== "exec") return null;
  const execClass = execResultClass(item);
  return execClass === "success" || execClass === "pending"
    ? `repeat:exec:${execClass}`
    : null;
}

function coordinationToolCounts(messages) {
  const counts = new Map();
  for (const message of messages) {
    const name = normalizedToolName(message);
    counts.set(name, (counts.get(name) || 0) + 1);
  }
  return [...counts].map(([name, count]) => ({ name, count }));
}

function compactTranscriptItems(messages, maxRun = MAX_COLLAPSED_TOOL_RUN) {
  const items = [];
  const source = Array.isArray(messages) ? messages : [];
  const boundedMax = Math.max(2, Math.min(Number.isInteger(maxRun) ? maxRun : MAX_COLLAPSED_TOOL_RUN, MAX_COLLAPSED_TOOL_RUN));
  for (let index = 0; index < source.length;) {
    if (!collapsibleToolRun(source[index])) {
      items.push({ kind: "item", message: source[index] }); index += 1; continue;
    }
    let end = index;
    while (end < source.length && collapsibleToolRun(source[end])) end += 1;
    let cursor = index;
    while (cursor < end) {
      const remaining = end - cursor;
      const size = remaining === boundedMax + 1 ? boundedMax - 1 : Math.min(boundedMax, remaining);
      if (size < 2) {
        items.push({ kind: "item", message: source[cursor] }); cursor += 1; continue;
      }
      const grouped = source.slice(cursor, cursor + size);
      const firstId = String(grouped[0]?.id || cursor);
      const groupKey = internalToolGroupKey(grouped[0]);
      const kind = groupKey && grouped.every((message) => internalToolGroupKey(message) === groupKey)
        ? "tool-group" : "tool-run";
      items.push({
        kind,
        // Result updates may change the group style, never its expansion key.
        id: `tool-group:${firstId}`,
        messages: grouped,
        counts: coordinationToolCounts(grouped),
      });
      cursor += size;
    }
    index = end;
  }
  return items;
}

/// Mixed tools, meaningful results and errors still belong to the same run.
/// Errors remain visible in the outer summary. Approvals keep their own row.
function collapsibleToolRun(item) {
  return transcriptItemKind(item) === "tool"
    && toolResultSignal(item) !== "approval";
}

function toolDisplayName(item) {
  const raw = String(item?.tool_name || "Tool").trim() || "Tool";
  const segments = raw.split(/__|[./:]/).filter(Boolean);
  return segments.length ? segments[segments.length - 1] : raw;
}

function toolRunTokens(messages) {
  const totals = {};
  for (const [field, key] of [["input_tokens", "input"], ["output_tokens", "output"]]) {
    let sum = null;
    for (const message of messages || []) {
      const value = message?.[field];
      if (!Number.isSafeInteger(value) || value < 0) continue;
      sum = (sum ?? 0) + value;
      if (!Number.isSafeInteger(sum)) { sum = null; break; }
    }
    totals[key] = sum;
  }
  return totals.input === null && totals.output === null ? null : totals;
}

function transcriptTokenSummary(messages, includeTotal = false) {
  const tokens = toolRunTokens(messages);
  if (!tokens) return "tokens —";
  const count = (value) => value === null ? "—" : formatTokenCount(value);
  const parts = [`${count(tokens.input)} in`, `${count(tokens.output)} out`];
  const total = tokens.input === null || tokens.output === null ? null : tokens.input + tokens.output;
  if (includeTotal && Number.isSafeInteger(total)) parts.push(`${formatTokenCount(total)} total tokens`);
  return parts.join(" · ");
}

function transcriptErrorCount(messages) {
  return messages.filter((message) => transcriptItemKind(message) === "tool"
    && toolResultSignal(message) === "error").length;
}

function conversationMetricsSummary(transcript) {
  const messages = transcript?.available && Array.isArray(transcript.messages) ? transcript.messages : [];
  if (!messages.length) return "";
  const tools = messages.filter((message) => transcriptItemKind(message) === "tool").length;
  const errors = transcriptErrorCount(messages);
  return [transcript.truncated ? "Loaded totals (partial)" : "Loaded totals",
    `${messages.length - tools} messages`, `${tools} tools`, transcriptTokenSummary(messages, true),
    errors ? `${errors} errors` : ""].filter(Boolean).join(" · ");
}

function formatTokenCount(value) {
  const count = Number.isFinite(value) && value > 0 ? value : 0;
  if (count < 1000) return String(count);
  const scaled = count < 1_000_000 ? count / 1000 : count / 1_000_000;
  const unit = count < 1_000_000 ? "k" : "M";
  return `${scaled >= 100 ? Math.round(scaled) : scaled.toFixed(1).replace(/\.0$/, "")}${unit}`;
}

function toolRunGroupSummary(group) {
  const messages = group?.messages || [];
  const names = new Set(messages.map(normalizedToolName));
  const name = names.size === 1
    ? (names.has("exec") ? "exec" : toolDisplayName(messages[0])) : "Tools";
  const parts = [`${name} ×${messages.length}`];
  const errors = transcriptErrorCount(messages);
  if (errors) parts.push(`${errors} ${errors === 1 ? "error" : "errors"}`);
  parts.push(transcriptTokenSummary(messages));
  return parts.join(" · ");
}

/// Folds the plain tool cards the coordination pass left behind. The key comes
/// from the first entry so the reading anchor survives a redraw.
function groupRepeatedTools(items, maxRun = MAX_COLLAPSED_TOOL_RUN) {
  const grouped = [];
  const boundedMax = Math.max(MIN_COLLAPSED_TOOL_RUN, Math.min(Number.isInteger(maxRun) ? maxRun : MAX_COLLAPSED_TOOL_RUN, MAX_COLLAPSED_TOOL_RUN));
  const runnable = (entry) => entry?.kind === "item" && collapsibleToolRun(entry.message);
  for (let index = 0; index < items.length;) {
    if (!runnable(items[index])) { grouped.push(items[index]); index += 1; continue; }
    let end = index;
    while (end < items.length && runnable(items[end])) end += 1;
    let cursor = index;
    while (cursor < end) {
      const size = Math.min(boundedMax, end - cursor);
      if (size < MIN_COLLAPSED_TOOL_RUN) {
        grouped.push(items[cursor]); cursor += 1; continue;
      }
      const messages = items.slice(cursor, cursor + size).map((entry) => entry.message);
      grouped.push({ kind: "tool-run", id: `tool-run:${String(messages[0]?.id || cursor)}`, messages });
      cursor += size;
    }
    index = end;
  }
  return grouped;
}

/// Dispatches between the folded runs of plain tool cards and the internal
/// coordination groups; both render through the same collapsed row.
function coordinationGroupSummary(group) {
  if (group?.kind === "tool-run") return toolRunGroupSummary(group);
  return toolGroupSummary(group);
}

function toolGroupSummary(group) {
  const calls = group?.messages?.length || 0;
  const counts = group?.counts || [];
  const labels = counts.map(({ name, count }) => `${name} ×${count}`).join(" · ");
  const label = counts.length === 1 ? `${counts[0].name} ×${calls}` : `${calls} internal calls · ${labels}`;
  return `${label} · ${transcriptTokenSummary(group?.messages || [])}`;
}

function dictationDelivery(paneId, prefix, finalText) {
  const spoken = String(finalText || "").trim();
  if (!paneId || !spoken) return null;
  return {
    paneId,
    message: [String(prefix || "").trim(), spoken].filter(Boolean).join(" "),
  };
}

function dictationPrefix(inputText, composerSending, inFlightText, inFlightTarget, currentTarget) {
  const current = String(inputText || "").trim();
  const sameTarget = Boolean(inFlightTarget && currentTarget && inFlightTarget === currentTarget);
  return composerSending && sameTarget && current === String(inFlightText || "").trim() ? "" : current;
}

function composerSubmissionMatches(selectedPaneId, targetPaneId, inputText, submittedText) {
  return Boolean(targetPaneId)
    && selectedPaneId === targetPaneId
    && String(inputText) === String(submittedText);
}

function composerSubmissionCanRestore(selectedPaneId, inputText, revision, submission) {
  return Boolean(submission)
    && submission.clearedRevision !== null
    && selectedPaneId === submission.paneId
    && String(inputText) === ""
    && revision === submission.clearedRevision;
}

function dictationEndAction(holding, releaseRequested, failed) {
  return holding && !releaseRequested && !failed ? "restart" : "finish";
}

function dictationErrorPolicy(error) {
  if (error === "no-speech") return "retry";
  if (error === "aborted") return "normal";
  return "fail";
}

function dictationRestartDelay(attempt) {
  const bounded = Math.max(0, Math.min(Number.isInteger(attempt) ? attempt : 0, 3));
  return 250 * (2 ** bounded);
}

function sessionDeletePath(id) {
  return `/api/v1/sessions/${encodeURIComponent(String(id))}`;
}

const SESSION_NAME_PATTERN = /^[A-Za-z0-9_-]{1,100}$/;
const RESERVED_SERVICE_SESSION = "atmux-web";
const MAX_SESSION_DESCRIPTION_CHARS = 120;

/// Builds the PATCH body for a rail rename/description edit from the session
/// captured when the dialog opened, sending only the fields that changed. It
/// mirrors the owner's checks so a mistake is shown in the dialog at once.
function sessionEditRequest(session, nameValue, descriptionValue) {
  if (typeof session?.id !== "string" || !session.id || !PANE_INSTANCE_PATTERN.test(String(session.instance_id || ""))) {
    return { error: "This session can’t be edited until its machine reports its current agent pane." };
  }
  const name = String(nameValue ?? "").trim();
  const description = String(descriptionValue ?? "").trim();
  // A session created outside atmux may carry a name atmux would not choose;
  // only a new name has to follow the launch rules.
  const renamed = name !== session.name;
  if (renamed && !SESSION_NAME_PATTERN.test(name)) return { error: "Use 1–100 letters, numbers, - or _ for the name." };
  if (renamed && name === RESERVED_SERVICE_SESSION) return { error: "That name is reserved for the atmux web service." };
  if ([...description].length > MAX_SESSION_DESCRIPTION_CHARS) {
    return { error: `Keep the description to ${MAX_SESSION_DESCRIPTION_CHARS} characters.` };
  }
  if (/[\u0000-\u001f\u007f-\u009f]/.test(description)) return { error: "The description must be a single line." };
  const body = { instance_id: session.instance_id };
  if (renamed) body.name = name;
  if (description !== (session.description || "") || session.description_source === "auto") body.description = description;
  if (!("name" in body) && !("description" in body)) return { unchanged: true };
  return { id: session.id, body };
}

function modelPickerState(session, capabilities, online, switchingPaneId, composerSending = false) {
  const recognized = session?.agent === "claude" || session?.agent === "codex";
  const matches = capabilities?.pane_id === session?.id;
  const models = matches && Array.isArray(capabilities.model_options) ? capabilities.model_options : [];
  const efforts = matches && Array.isArray(capabilities.effort_options) ? capabilities.effort_options : [];
  const current = matches && typeof capabilities.current === "string" ? capabilities.current : "";
  const effort = matches && typeof capabilities.effort === "string" ? capabilities.effort : "";
  const currentMode = matches && typeof capabilities.current_mode === "string" ? capabilities.current_mode : "";
  const fastSupported = matches && capabilities.fast_supported === true;
  const fast = matches && typeof capabilities.fast === "boolean" ? capabilities.fast : null;
  const busy = Boolean(switchingPaneId);
  // Model, effort, and fast are applied one at a time, so each control is
  // enabled on its own evidence and a shared in-flight switch blocks them all.
  const blocked = !online || busy || composerSending;
  return {
    visible: recognized,
    loading: recognized && !matches,
    current,
    effort,
    currentMode,
    models,
    efforts,
    fast,
    fastSupported,
    disabled: blocked || !models.some((model) => model.switchable),
    effortDisabled: blocked || !efforts.some((choice) => choice.switchable),
    fastDisabled: blocked || !fastSupported,
    status: !recognized ? ""
      : !online ? "Machine offline"
        : busy ? (switchingPaneId === session?.id ? "Switching…" : "Another model switch is in progress")
          : !matches ? "Checking models…"
            : capabilities.note || (current ? `Current: ${[current, effort, fast ? "fast" : ""].filter(Boolean).join(" · ")}` : "Current model unavailable"),
  };
}

/// The choices one picker offers, with the running value shown first and
/// unselectable when this profile does not configure it.
function pickerOptions(choices, current) {
  const options = [...choices];
  if (current && !options.some((choice) => choice.id === current)) {
    options.unshift({ id: "", label: `${current} (current; not configured)`, switchable: false });
  }
  return options;
}

function agentRestartState(session, capabilities, online, resumingPaneId, composerSending = false) {
  const isRestartableAgent = session?.agent === "claude" || session?.agent === "codex";
  const matches = capabilities?.pane_id === session?.id;
  const available = matches && capabilities?.resume_available === true
    && /^restart-v1-[a-f0-9]{64}$/.test(capabilities?.restart_token || "");
  const note = matches && typeof capabilities?.resume_note === "string" ? capabilities.resume_note : "";
  const restarting = Boolean(resumingPaneId);
  return {
    visible: isRestartableAgent,
    available,
    disabled: !online || restarting || composerSending || !available,
    status: !isRestartableAgent ? ""
      : !online ? "Machine offline"
        : restarting ? (resumingPaneId === session?.id ? "Restarting agent…" : "Another agent restart is in progress")
          : !matches ? "Checking restart…"
            : note || (available ? "Ready to restart this session" : "Session restart is unavailable"),
  };
}

function followsLiveTail(element, tolerance = 16) {
  return element.scrollHeight - element.scrollTop - element.clientHeight <= tolerance;
}

function agentRestartRequest(session, capabilities) {
  if (!session?.id || !/^pane-v1-[a-f0-9]{64}$/.test(session.instance_id || "")) return null;
  if (capabilities?.pane_id !== session.id
      || !/^restart-v1-[a-f0-9]{64}$/.test(capabilities?.restart_token || "")) return null;
  return { id: session.id, instance_id: session.instance_id, restart_token: capabilities.restart_token };
}

function paneOutputDownload(session, lines) {
  if (!session?.id || !Array.isArray(lines) || !lines.length) return null;
  const name = String(session.name || "agent").replace(/[^a-zA-Z0-9._-]/g, "-").slice(0, 100);
  return { filename: `atmux-${name}-output.txt`, content: lines.join("\n") + "\n" };
}

function paneOutputBinding(session) {
  return session?.id ? { id: session.id, instance_id: session.instance_id || null } : null;
}

function paneOutputMatchesSession(binding, session) {
  return Boolean(binding && session?.id && binding.id === session.id
    && binding.instance_id === (session.instance_id || null));
}

/// Decides how a transcript redraw treats the reader. A pane with no laid-out
/// box — hidden view mode, unselected agent view, mobile list still showing —
/// reports no scroll height, so its geometry says nothing and the reader's
/// remembered intent carries over until the pane is measurable again.
function stickyBottomState(element, following, visible, tolerance = STICKY_BOTTOM_TOLERANCE) {
  const measurable = Boolean(visible) && element.clientHeight > 0;
  if (!measurable) {
    return { measurable, follow: Boolean(following), deferBottom: Boolean(following), showJump: false };
  }
  const follow = Boolean(following) && followsLiveTail(element, tolerance);
  return { measurable, follow, deferBottom: false, showJump: !follow };
}

function transcriptAnchorMembers(value) {
  if (typeof value !== "string" || !value
    || value.length > MAX_TRANSCRIPT_ANCHOR_JSON_CHARS) return [];
  try {
    const members = JSON.parse(value);
    if (!Array.isArray(members) || !members.length
      || members.length > MAX_COLLAPSED_TOOL_RUN
      || members.some((member) => typeof member !== "string" || !member
        || member.length > MAX_TRANSCRIPT_ANCHOR_MEMBER_CHARS)) return [];
    return members;
  } catch {
    return [];
  }
}

function transcriptAnchorItems(container) {
  return [...(container?.children || [])]
    .filter((node) => Boolean(node?.dataset?.transcriptId));
}

/// Captures the first visible semantic transcript item, not just a pixel
/// offset. Bounded logs can discard cards above a reader while an agent emits.
function transcriptReadingAnchor(container, retain = null) {
  const bounds = container.getBoundingClientRect();
  for (const item of transcriptAnchorItems(container)) {
    const id = item.dataset.transcriptId;
    const box = item.getBoundingClientRect();
    if (id && box.bottom > bounds.top && (!retain || retain(item))) {
      const members = transcriptAnchorMembers(item.dataset.transcriptMembers);
      const memberId = members[0] || id;
      return { id, memberId, offset: box.top - bounds.top };
    }
  }
  return null;
}

/// Restores the reader to the same transcript card after a wholesale redraw.
/// Falling back to the prior offset is still preferable to forcing the tail
/// when the bounded transcript has evicted that card.
function restoreTranscriptReadingAnchor(container, anchor, fallbackOffset) {
  if (!anchor) {
    container.scrollTop = fallbackOffset;
    return;
  }
  // Only outer Conversation rows participate. Expanded/collapsed tool-group
  // descendants carry their own ids but are not independent scroll anchors.
  const items = transcriptAnchorItems(container);
  let item = items.find((node) => node.dataset.transcriptId === anchor.id);
  if (!item && anchor.memberId) {
    item = items.find((node) => node.dataset.transcriptId === anchor.memberId)
      || items.find((node) => transcriptAnchorMembers(node.dataset.transcriptMembers)
        .includes(anchor.memberId));
  }
  if (!item) {
    container.scrollTop = fallbackOffset;
    return;
  }
  const bounds = container.getBoundingClientRect();
  const offset = item.getBoundingClientRect().top - bounds.top;
  container.scrollTop += offset - anchor.offset;
}

/// Ignores only the queued scroll notification caused by a render-time scroll
/// restoration. A later reader scroll always recalculates live-following.
function scrollMatchesExpectedPosition(element, expected) {
  return Number.isFinite(expected) && Math.abs(element.scrollTop - expected) <= 1;
}

function memoryValue(used, total) {
  if (!Number.isFinite(total) || total <= 0) return "—";
  return `${Number.isFinite(used) && used >= 0 ? formatBytes(used) : "—"} / ${formatBytes(total)}`;
}

function formatUptime(seconds) {
  if (!Number.isFinite(seconds) || seconds < 0) return "Unavailable";
  // Owners publish at minute granularity; flooring also keeps mixed-version
  // payloads stable at the precision shown in the UI.
  const totalMinutes = Math.floor(seconds / 60);
  if (totalMinutes < 1) return "<1m";
  const days = Math.floor(totalMinutes / (24 * 60));
  const hours = Math.floor((totalMinutes % (24 * 60)) / 60);
  const minutes = totalMinutes % 60;
  const parts = [];
  if (days) parts.push(`${days}d`);
  if (hours) parts.push(`${hours}h`);
  if (minutes) parts.push(`${minutes}m`);
  return parts.join(" ");
}

function systemMetricLines(metrics = {}) {
  const displayText = (value) => typeof value === "string" && value.trim() ? value.trim() : "Unavailable";
  return [
    `Uptime · ${formatUptime(metrics.uptime_seconds)}`,
    `Kernel · ${displayText(metrics.kernel_version)}`,
    `OS · ${displayText(metrics.os_version)}`,
  ];
}

function formatBytes(bytes) {
  if (!Number.isFinite(bytes) || bytes < 0) return "—";
  const units = ["B", "KiB", "MiB", "GiB", "TiB"];
  let value = bytes; let unit = 0;
  while (value >= 1024 && unit < units.length - 1) { value /= 1024; unit += 1; }
  return `${value >= 10 || unit === 0 ? value.toFixed(0) : value.toFixed(1)} ${units[unit]}`;
}

function gpuSummary(gpu = {}) {
  const parts = [gpu.name || "GPU"];
  if (gpu.utilization_percent != null) parts.push(`${gpu.utilization_percent}%`);
  if (gpu.memory_total_bytes != null) parts.push(memoryValue(gpu.memory_used_bytes, gpu.memory_total_bytes));
  if (gpu.temperature_celsius != null) parts.push(`${gpu.temperature_celsius}°C`);
  return parts.join(" · ");
}

function gpuDetailLines(gpu = {}) {
  const lines = [];
  const identity = [gpu.vendor, gpu.pci_bus_id || gpu.id].filter(Boolean);
  if (identity.length) lines.push(`Identity · ${identity.join(" · ")}`);
  const memory = [];
  if (gpu.memory_total_bytes != null) memory.push(`VRAM ${memoryValue(gpu.memory_used_bytes, gpu.memory_total_bytes)}`);
  if (gpu.memory_shared === true) memory.push("shared memory");
  if (gpu.memory_pressure_percent != null) memory.push(`pressure ${gpu.memory_pressure_percent}%`);
  if (memory.length) lines.push(`Memory · ${memory.join(" · ")}`);
  const powerThermal = [];
  if (gpu.power_draw_watts != null) powerThermal.push(`power ${formatDecimal(gpu.power_draw_watts)} W`);
  if (gpu.power_limit_watts != null) powerThermal.push(`limit ${formatDecimal(gpu.power_limit_watts)} W`);
  if (gpu.temperature_celsius != null) powerThermal.push(`${formatDecimal(gpu.temperature_celsius)}°C`);
  if (gpu.thermal_state) powerThermal.push(`thermal ${gpu.thermal_state}`);
  if (powerThermal.length) lines.push(`Power / thermal · ${powerThermal.join(" · ")}`);
  const clocks = [];
  if (gpu.graphics_clock_mhz != null) clocks.push(`graphics ${gpu.graphics_clock_mhz} MHz`);
  if (gpu.memory_clock_mhz != null) clocks.push(`memory ${gpu.memory_clock_mhz} MHz`);
  if (gpu.video_clock_mhz != null) clocks.push(`video ${gpu.video_clock_mhz} MHz`);
  if (clocks.length) lines.push(`Clocks · ${clocks.join(" · ")}`);
  const fanPerformance = [];
  if (gpu.fan_percent != null) fanPerformance.push(`fan ${gpu.fan_percent}%`);
  if (gpu.fan_speed_rpm != null) fanPerformance.push(`${gpu.fan_speed_rpm} RPM`);
  if (gpu.performance_state) fanPerformance.push(`state ${gpu.performance_state}`);
  if (fanPerformance.length) lines.push(`Cooling / performance · ${fanPerformance.join(" · ")}`);
  const software = [];
  if (gpu.driver_version) software.push(`driver ${gpu.driver_version}`);
  if (gpu.runtime_version) software.push(`runtime ${gpu.runtime_version}`);
  if (gpu.compute_capability) software.push(`compute ${gpu.compute_capability}`);
  if (gpu.core_count != null) software.push(`${gpu.core_count} cores`);
  if (software.length) lines.push(`Driver / compute · ${software.join(" · ")}`);
  const unavailable = Array.isArray(gpu.unavailable) ? gpu.unavailable.filter(Boolean) : [];
  if (unavailable.length) lines.push(`Unavailable · ${unavailable.join(", ")}`);
  return lines.length ? lines : ["No optional counters are available"];
}

function gpuDiagnosticLines(diagnostics) {
  if (!Array.isArray(diagnostics)) return [];
  return diagnostics.map((diagnostic) => [diagnostic?.source, diagnostic?.message].filter(Boolean).join(" · ")).filter(Boolean);
}

function formatDecimal(value) {
  return Number.isInteger(value) ? String(value) : Number(value).toFixed(1);
}

const ATMUX_HISTORY_VIEW = "atmuxView";

/// A proxy or network hop can drop a long-lived stream at any time, and the
/// browser reconnects within a second or two. Only a drop that outlasts this
/// grace period is worth a banner; before then the status pill says enough.
const OVERVIEW_DROP_GRACE_MS = 8000;

function overviewConnectionPresentation(connection, elapsedMs = Infinity) {
  if ((connection === "reconnecting" || connection === "stale")
    && Number.isFinite(elapsedMs) && elapsedMs >= 0 && elapsedMs < OVERVIEW_DROP_GRACE_MS) {
    return { label: "Reconnecting…", note: "", retry: false };
  }
  const states = {
    live: { label: "Live", note: "", retry: false },
    connecting: { label: "Connecting…", note: "Connecting to live updates…", retry: false },
    reconnecting: { label: "Reconnecting…", note: "Live updates disconnected. Showing the last received state.", retry: true },
    stale: { label: "Updates delayed", note: "Live updates are delayed. Showing the last received state.", retry: true },
    paused: { label: "Paused", note: "", retry: false },
  };
  return states[connection] || states.stale;
}

// An open socket is not proof that we have an authoritative overview. Each
// connection must deliver its initial snapshot, including native SSE retries.
// Idle streams have no visible heartbeat, so only the initial snapshot times
// out; unchanged fleets must not be mistaken for disconnected ones.
function createOverviewStream({
  createSource,
  onConnection,
  onOverview,
  onProtocolError,
  setTimer = setTimeout,
  clearTimer = clearTimeout,
}) {
  const source = createSource();
  let closed = false;
  let hasSnapshot = false;
  let snapshotTimer = null;
  const cancelSnapshotTimer = () => {
    if (snapshotTimer !== null) clearTimer(snapshotTimer);
    snapshotTimer = null;
  };
  const waitForSnapshot = () => {
    cancelSnapshotTimer();
    snapshotTimer = setTimer(() => {
      snapshotTimer = null;
      if (!closed && !hasSnapshot) onConnection("stale");
    }, 15_000);
  };
  onConnection("connecting");
  waitForSnapshot();
  source.onopen = () => {
    if (closed) return;
    hasSnapshot = false;
    onConnection("connecting");
    waitForSnapshot();
  };
  const receive = (event, snapshot) => {
    if (closed) return;
    if (!snapshot && !hasSnapshot) {
      onConnection("stale");
      return;
    }
    const accepted = onOverview(event);
    // A revision gap may have replaced this connection during onOverview.
    if (closed) return;
    if (!accepted) {
      onConnection("stale");
      return;
    }
    hasSnapshot = true;
    cancelSnapshotTimer();
    onConnection("live");
  };
  source.addEventListener("sessions.snapshot", (event) => receive(event, true));
  source.addEventListener("sessions.patch", (event) => receive(event, false));
  source.addEventListener("protocol.error", (event) => {
    if (closed) return;
    hasSnapshot = false;
    cancelSnapshotTimer();
    onProtocolError(event);
    onConnection("stale");
  });
  source.onerror = () => {
    if (closed) return;
    hasSnapshot = false;
    cancelSnapshotTimer();
    onConnection("reconnecting");
  };
  return {
    close() {
      closed = true;
      cancelSnapshotTimer();
      source.close();
    },
  };
}

function selectedAgentUrl(urlValue, sessionId) {
  if (typeof sessionId !== "string" || !sessionId) return null;
  const url = agentMenuUrl(urlValue);
  url.searchParams.set("session", sessionId);
  return url.toString();
}

async function copySelectedAgentLink(urlValue, sessionId, clipboard) {
  const url = selectedAgentUrl(urlValue, sessionId);
  if (!url) throw new Error("Select an agent before copying its link.");
  if (typeof clipboard?.writeText !== "function") {
    throw new Error("Clipboard access is unavailable. Copy this page’s address from the browser.");
  }
  try {
    await clipboard.writeText(url);
  } catch {
    throw new Error("Could not copy the link. Allow clipboard access or copy this page’s address from the browser.");
  }
  return url;
}

function agentSearchShortcut(event, { dialogOpen = false, searchVisible = true } = {}) {
  if (!event || event.defaultPrevented || event.isComposing || event.repeat
      || event.ctrlKey || event.metaKey || event.altKey || dialogOpen) return null;
  const target = event.target;
  if (event.key === "Escape" && target?.id === "filter") {
    return target.value ? "clear" : "blur";
  }
  if (event.key !== "/" || !searchVisible) return null;
  if (target?.isContentEditable
      || target?.closest?.("input, textarea, select, [role='textbox'], [contenteditable]:not([contenteditable='false']), #conversation, #pane")) return null;
  return "focus";
}

// Shared A3 history contract; A4 can consume sessionHistoryResumeRequest.
function sessionHistoryQuery(filters = {}, cursor = null) {
  const query = new URLSearchParams({ limit: "100" });
  for (const key of ["state", "machine", "project", "text"]) {
    const value = String(filters[key] || "").trim().slice(0, 2048);
    if (value) query.set(key, value);
  }
  if (cursor) query.set("cursor", String(cursor));
  return `/api/v1/session-history?${query}`;
}
function sessionHistoryRow(record) {
  return {
    sessionKey: String(record?.session_key || ""),
    name: String(record?.name || "Untitled session"),
    description: String(record?.description || ""),
    machine: String(record?.machine || ""),
    project: String(record?.project?.remote || record?.project?.root || record?.cwd || ""),
    lastActiveMs: Number(record?.last_active_ms || 0),
    state: ["running", "exited", "closed", "archived"].includes(record?.state) ? record.state : "unknown",
  };
}
function sessionHistoryResumeRequest(record) {
  return { session_key: String(record?.session_key || record?.sessionKey || ""), machine: String(record?.machine || "") };
}

function appRoute(urlValue) {
  const url = urlValue instanceof URL ? urlValue : new URL(String(urlValue), "https://atmux.invalid/");
  const session = url.searchParams.get("session");
  if (session) return { view: "session", id: session };
  const machine = url.searchParams.get("machine");
  if (machine) return { view: "machine", id: machine };
  if (url.searchParams.get("view") === "sessions") return { view: "sessions", id: null };
  if (url.searchParams.get("view") === "usage") return { view: "usage", id: null };
  return { view: "menu", id: null };
}

function agentMenuUrl(urlValue) {
  const url = new URL(String(urlValue));
  url.searchParams.delete("session");
  url.searchParams.delete("machine");
  url.searchParams.delete("view");
  return url;
}

function appHistoryState(route) {
  return { [ATMUX_HISTORY_VIEW]: route.view, atmuxId: route.id };
}

function savedSessionPreview(value) {
  const preview = String(value || "Previous conversation")
    .replace(/[\u0000-\u001f\u007f]+/g, " ")
    .replace(/\s+/g, " ")
    .trim()
    .slice(0, 160);
  return preview || "Previous conversation";
}

function savedSessionConfirmation({ machineId, machineLabel, profileLabel, directory, harness, preview }) {
  return [
    "Resume this saved conversation?",
    "",
    `Machine: ${machineLabel} (${machineId})`,
    `Profile: ${profileLabel}`,
    `Folder: ${directory}`,
    `Agent: ${harness}`,
    `Preview: ${savedSessionPreview(preview)}`,
  ].join("\n");
}

// Stable-key resume controls are also reusable by A3's Sessions view.
function resumeMachineOptions(machines) {
  return (Array.isArray(machines) ? machines : []).filter((machine) => machine.online
    && ["local", "remote"].includes(machine.kind) && /^[a-z0-9][a-z0-9_-]*$/.test(machine.id));
}
function sessionResumeIntent(session, machine, move = false) {
  if (!session || !/^[0-9a-f]{8}-[0-9a-f]{4}-7[0-9a-f]{3}-[89ab][0-9a-f]{3}-[0-9a-f]{12}$/.test(session.session_key || "")
    || !/^[a-z0-9][a-z0-9_-]*$/.test(machine || "")) return null;
  return { session_key: session.session_key, machine, move: Boolean(move) };
}
function generatedSessionName(title) {
  const name = String(title || "").normalize("NFKD").replace(/[\u0300-\u036f]/g, "")
    .toLowerCase().replace(/[^a-z0-9_-]+/g, "-").replace(/^-+|-+$/g, "").slice(0, 100);
  return SESSION_NAME_PATTERN.test(name) && name !== RESERVED_SERVICE_SESSION ? name : "";
}

function inlineRenameAction(event, { selected = false, dialogOpen = false } = {}) {
  if (event.key === "Escape") return "cancel";
  if (event.key === "Enter" && !event.isComposing) return "save";
  if (event.key === "F2" && !event.isComposing && selected && !dialogOpen && !event.ctrlKey && !event.metaKey && !event.altKey
    && !event.target?.closest?.("input, textarea, select, [contenteditable]")) return "open";
  return null;
}

/// The same editor is used by header and rail, with a captured pane generation.
function createInlineRenameEditor({ document, host, anchor, session, save, suggest, close }) {
  const snapshot = { id: session.id, instance_id: session.instance_id, name: session.name, description: session.description || "" };
  const form = document.createElement("form"); form.className = "inline-rename";
  form.setAttribute("aria-label", `Rename ${snapshot.name}`);
  const input = document.createElement("input"); input.className = "inline-rename-name";
  input.value = snapshot.name; input.maxLength = 100; input.autocomplete = "off";
  input.setAttribute("aria-label", "Session name"); input.spellcheck = false;
  const note = document.createElement("span"); note.className = "inline-rename-note"; note.setAttribute("role", "alert");
  let closed = false; let saving = false; let suggesting = false;
  const finish = () => { if (closed) return; closed = true; form.remove(); anchor.style.visibility = ""; close?.(); };
  const submit = async () => {
    if (closed || saving) return;
    const edit = sessionEditRequest(snapshot, input.value, snapshot.description);
    if (edit.error) { note.textContent = edit.error; return; }
    if (edit.unchanged) { finish(); return; }
    saving = true; input.disabled = true; saveButton.disabled = true; suggestButton.disabled = true;
    try { await save(edit, snapshot); finish(); }
    catch (error) { if (!closed) note.textContent = error.message; }
    finally { saving = false; input.disabled = false; saveButton.disabled = false; suggestButton.disabled = false; }
  };
  const button = (label, action) => {
    const node = document.createElement("button"); node.type = "button"; node.textContent = label;
    node.addEventListener("click", (event) => { event.preventDefault(); event.stopPropagation(); action(); }); return node;
  };
  const suggestButton = button("Suggest", async () => {
    if (closed || saving || suggesting) return;
    suggesting = true; suggestButton.disabled = true;
    try {
      const title = generatedSessionName(await suggest(snapshot));
      if (!closed) { if (title) { input.value = title; note.textContent = ""; input.focus(); input.select(); }
        else note.textContent = "No generated title yet. Try again after a summary is available."; }
    } catch (error) { if (!closed) note.textContent = error.message; }
    finally { suggesting = false; suggestButton.disabled = saving; }
  });
  const saveButton = button("Save", () => { void submit(); });
  const cancelButton = button("Cancel", () => { if (!saving) finish(); });
  input.addEventListener("keydown", (event) => {
    const action = inlineRenameAction(event); if (!action) return;
    event.preventDefault(); event.stopPropagation();
    if (action === "save") void submit(); else if (!saving) finish();
  });
  form.addEventListener("submit", (event) => { event.preventDefault(); void submit(); });
  form.addEventListener("click", (event) => event.stopPropagation());
  form.append(input, suggestButton, saveButton, cancelButton, note); host.append(form);
  anchor.style.visibility = "hidden"; input.focus(); input.select();
  return { id: snapshot.id, instance_id: snapshot.instance_id, close: finish, input };
}

function bindInlineRenameGesture(node, open, timers = globalThis) {
  let timer = null; let origin = null; let held = false;
  const cancel = () => { if (timer !== null) timers.clearTimeout(timer); timer = null; };
  node.addEventListener("dblclick", (event) => { event.preventDefault(); event.stopPropagation(); open(); });
  node.addEventListener("pointerdown", (event) => {
    if (event.pointerType !== "touch") return;
    cancel(); held = false; origin = { x: event.clientX, y: event.clientY };
    timer = timers.setTimeout(() => { timer = null; held = true; open(); }, 600);
  });
  node.addEventListener("pointermove", (event) => {
    if (origin && Math.hypot(event.clientX - origin.x, event.clientY - origin.y) > 10) cancel();
  });
  for (const name of ["pointerup", "pointercancel", "pointerleave"]) node.addEventListener(name, cancel);
  node.addEventListener("click", (event) => { if (held) { event.preventDefault(); event.stopPropagation(); held = false; } });
  node.addEventListener("contextmenu", (event) => { if (held) event.preventDefault(); });
}

if (typeof module !== "undefined" && module.exports) {
  module.exports = {
    resumeMachineOptions, sessionResumeIntent,
    generatedSessionName, inlineRenameAction, createInlineRenameEditor, bindInlineRenameGesture,
    MAX_MESSAGE_BYTES,
    MAX_IMAGE_ATTACHMENTS,
    MAX_IMAGE_BYTES,
    MAX_TOTAL_IMAGE_BYTES,
    MAX_LAUNCH_DIRECTORY_CANDIDATES,
    MAX_LAUNCH_DIRECTORY_SUGGESTIONS,
    LAUNCH_DIRECTORY_SEARCH_DEBOUNCE_MS,
    MAX_FILE_REFERENCE_CHARS,
    MAX_FILE_REFERENCE_LINES,
    attachmentDeliveryTarget,
    attachmentSelectionMatches,
    agentMenuUrl,
    appRoute,
    sessionHistoryQuery,
    sessionHistoryRow,
    sessionHistoryResumeRequest,
    paneOutputBinding,
    paneOutputMatchesSession,
    overviewConnectionPresentation,
    createOverviewStream,
    selectedAgentUrl,
    copySelectedAgentLink,
    agentSearchShortcut,
    remainingAttachmentsAfterDelivery,
    arrayBufferToBase64,
    applyPanePatch,
    classifyOverviewUpdate,
    compareSessions,
    composerEnterAction,
    composerDraftCanClear,
    composerDraftEntries,
    composerDraftIdentity,
    composerDraftInstanceId,
    composerDraftMachine,
    composerDraftJson,
    composerDraftTombstones,
    mergeComposerDraftState,
    pruneComposerDraftEntries,
    staleComposerDraftKeys,
    sessionMatchesComposerIdentity,
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
    duplicateSummaryState,
    filterDirectories,
    defaultMemoryLimitLabel,
    formatMemoryLimit,
    memoryLimitChoices,
    parseMemoryLimitSelection,
    formatRelativeTime,
    fleetUpdatePollDelay,
    recoveryMachines,
    recoveryPollDelay,
    recoveryRowState,
    updateConfirmCopy,
    groupSessionsByMachine,
    favoriteSessionKey,
    navigationPreferences,
    navigationView,
    machineCanCheck,
    machineCanRollback,
    machineCanUpdate,
    machineUpdateInFlight,
    machineUpdatePill,
    softwareCardModel,
    updatableMachines,
    updateProgressLabel,
    updateRestartWarning,
    harnessesForProfiles,
    isMachineControllable,
    isManualDirectory,
    rememberedLaunchDirectories,
    rememberLaunchDirectory,
    availableLaunchDirectories,
    launchDirectoryBrowsePath,
    validLaunchChildName,
    repositoryDestinationName,
    launchMachines,
    imageFilesFromTransfer,
    highlightCode,
    tokenizeSource,
    codeLanguage,
    codeLanguageNavigable,
    codeSymbolValid,
    sourceImports,
    localDefinitionLines,
    chooseLocalDefinition,
    symbolQualifier,
    paneCodePath,
    codeNavResults,
    pushCodeHistory,
    stepCodeHistory,
    inlineTokens,
    machineStatusLabel,
    isLaunchCapableMachine,
    gpuSummary,
    gpuDetailLines,
    gpuDiagnosticLines,
    formatUptime,
    systemMetricLines,
    markdownBlocks,
    messageFitsByteLimit,
    messageHistoryDirection,
    moveMessageHistory,
    paneTypingText,
    paneSpecialKeyDelivery,
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
    profilesForHarness,
    projectLabel,
    projectPreference,
    preferredLaunchMachineId,
    reconcileSessions,
    reduceOverview,
    reduceTranscript,
    createTranscriptPoller,
    sessionDeletePath,
    sessionEditRequest,
    modelPickerState,
    pickerOptions,
    agentRestartState,
    agentRestartRequest,
    paneOutputDownload,
    followsLiveTail,
    stickyBottomState,
    STICKY_BOTTOM_TOLERANCE,
    sessionMachineId,
    sessionFolderLabel,
    sessionProfileLabel,
    sourceLanguage,
    savedSessionConfirmation,
    savedSessionPreview,
    selectionTouchesPane,
    transcriptAnchorMembers,
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
    compactionSummaryLabel,
    rawPaneGrid,
    coordinationGroupSummary,
    toolGroupSummary,
    collapsibleToolRun,
    groupRepeatedTools,
    toolDisplayName,
    toolRunGroupSummary,
    toolRunTokens,
    transcriptTokenSummary,
    conversationMetricsSummary,
    formatTokenCount,
    linkifyTokens,
    trimmedAutolink,
    transcriptRoleLabel,
    diffLineKind,
    safeLinkUrl,
    sortSessions,
    presentSessionStatuses,
    WORKING_TO_WAITING_HOLD_MS,
    suggestedSessionName,
    utf8ByteLength,
    validateImageSelection,
    validContentHash,
    installMobileViewportRecovery,
  };
}

if (typeof document !== "undefined") initialize();

// WebKit can leave its reveal scroll behind after dismissing the keyboard or
// restoring a tab, including an offset with document.scrollTop already at zero.
// Observe the visual viewport only to recover that settled state. It must never
// supply the app's height or compete with the browser revealing a focused field.
function installMobileViewportRecovery({ window, document, isMobile, syncViewport }) {
  let settleTimer = null;
  const viewport = window.visualViewport;
  const listeners = [];
  const listen = (target, event, callback) => {
    if (!target?.addEventListener) return;
    target.addEventListener(event, callback, { passive: true });
    listeners.push(() => target.removeEventListener(event, callback));
  };
  const cancel = () => {
    window.clearTimeout(settleTimer);
    settleTimer = null;
  };
  const editableFocused = () => {
    const focused = document.activeElement;
    return focused?.isContentEditable
      || focused?.matches?.("input, textarea, select, [contenteditable]:not([contenteditable='false'])");
  };
  const recover = () => {
    settleTimer = null;
    if (document.hidden || !isMobile() || editableFocused()) return;
    syncViewport();
    if (!viewport) return;
    // A reduced visual viewport still belongs to the keyboard (or pinch zoom).
    // Wait for the full layout height instead of guessing keyboard dimensions.
    if (!Number.isFinite(window.innerHeight) || window.innerHeight <= 0
        || !Number.isFinite(viewport.height) || !Number.isFinite(viewport.scale)
        || Math.abs(viewport.scale - 1) > 0.01
        || viewport.height < window.innerHeight - 2) return;
    const root = document.scrollingElement || document.documentElement;
    const shifted = Math.abs(window.scrollY || 0) > 1
      || Math.abs(root.scrollTop || 0) > 1
      || Math.abs(viewport.offsetTop || 0) > 1
      || document.body.getBoundingClientRect().top < -1;
    if (!shifted) return;
    // Only the outer document is pinned. Rail, transcript, file reader, and
    // dialog scroll positions remain owned by their respective panels.
    root.scrollTop = 0;
    window.scrollTo({ top: 0, left: 0, behavior: "instant" });
  };
  const schedule = () => {
    cancel();
    if (document.hidden) return;
    // Keyboard/toolbar events can arrive before the paint animation finishes.
    // Coalesce them and re-check focus/scale/height when the viewport settles.
    settleTimer = window.setTimeout(recover, 350);
  };
  listen(viewport, "resize", schedule);
  listen(viewport, "scroll", schedule);
  listen(window, "resize", schedule);
  listen(window, "orientationchange", schedule);
  listen(window, "pageshow", schedule);
  listen(window, "pagehide", cancel);
  listen(document, "focusout", schedule);
  listen(document, "focusin", cancel);
  listen(document, "visibilitychange", schedule);
  schedule();
  return () => { cancel(); listeners.forEach((remove) => remove()); };
}

function initialize() {
  const pageUrl = new URL(location.href);
  const initialRoute = appRoute(pageUrl);
  if (initialRoute.view !== "menu" && history.state?.[ATMUX_HISTORY_VIEW] !== initialRoute.view) {
    const menuUrl = agentMenuUrl(pageUrl);
    history.replaceState(appHistoryState({ view: "menu", id: null }), "", menuUrl);
    history.pushState(appHistoryState(initialRoute), "", pageUrl);
  } else {
    history.replaceState(appHistoryState(initialRoute), "", pageUrl);
  }
  const readLocalStorage = (key) => {
    try { return localStorage.getItem(key); } catch { return null; }
  };
  const writeLocalStorage = (key, value) => {
    try {
      localStorage.setItem(key, value);
      return true;
    } catch {
      return false;
    }
  };
  const storedPulseAccount = pulseAccountId(readLocalStorage("atmux.pulse-account"));
  const storedLaunchDirectories = rememberedLaunchDirectories(
    readLocalStorage(LAUNCH_DIRECTORY_STORAGE_KEY),
  );
  const storedFileReaderPreferences = loadFileReaderPreferences(
    () => readLocalStorage(FILE_READER_STORAGE_KEY),
    mobileViewportActive(),
  );
  const storedConversationVisibility = loadConversationVisibilityPreferences(
    () => readLocalStorage(CONVERSATION_VISIBILITY_STORAGE_KEY),
  );
  const storedComposerDraftValue = readLocalStorage(COMPOSER_DRAFT_STORAGE_KEY);
  const storedComposerDraftState = mergeComposerDraftState(
    new Map(),
    new Map(),
    storedComposerDraftValue,
  );
  const storedComposerDrafts = storedComposerDraftState.drafts;
  const storedNavigation = navigationPreferences(readLocalStorage(NAVIGATION_STORAGE_KEY));
  const requestedPulseAccount = pulseAccountId(pageUrl.searchParams.get("pulseAccount"));
  const state = {
    revision: 0,
    agentEventStates: new Map(),
    agentEventCursor: null,
    agentEventController: null,
    agentEventTimer: null,
    agentEventAvailable: true,
    historyOpen: initialRoute.view === "sessions",
    historyRows: [],
    historyCursor: null,
    historyGeneration: 0,
    historyController: null,
    historyTimer: null,
    sessions: new Map(),
    machines: [],
    selected: initialRoute.view === "session" ? initialRoute.id : null,
    selectedMachine: initialRoute.view === "machine" ? initialRoute.id : null,
    paneLines: [],
    paneOutputBinding: null,
    paneRevision: 0,
    overviewSource: null,
    paneSource: null,
    overviewConnection: "connecting",
    overviewConnectionSince: 0,
    overviewGraceTimer: null,
    statusPresentations: new Map(),
    statusTimer: null,
    filter: "",
    statusFilter: "",
    harnessFilter: "",
    collapsedMachines: new Set(storedNavigation.collapsed),
    favoriteSessions: new Set(storedNavigation.favorites),
    health: null,
    pendingSelectionName: null,
    launchOptions: null,
    launchNamePristine: true,
    rememberedLaunchDirectories: storedLaunchDirectories,
    launchBrowseGeneration: 0,
    launchBrowseMutation: false,
    launchDialogGeneration: 0,
    launchFlow: null,
    launchSummarySourceId: null,
    launchSessionsGeneration: 0,
    launchSessionsKey: "",
    launchSessionsController: null,
    launchDirectorySearchTimer: null,
    launchDirectoryCandidates: null,
    launchDirectoryActiveIndex: -1,
    launchDirectorySuggestionsDismissed: false,
    launchDirectoryPointerGesture: null,
    launchDirectorySuppressClick: null,
    paneError: null,
    panePointerDown: false,
    pendingPaneRender: false,
    paneFollowing: true,
    paneReadingScrollTop: 0,
    paneExpectedScrollTop: null,
    transcript: { available: false, source: "agent", messages: [], truncated: false, error: null },
    transcriptHash: "",
    transcriptPoll: null,
    transcriptRequest: 0,
    transcriptPointerDown: false,
    pendingTranscriptRender: false,
    pendingTranscriptFilterChange: false,
    transcriptFollowing: true,
    transcriptExpectedScrollTop: null,
    transcriptReadingScrollTop: 0,
    transcriptPendingBottom: true,
    transcriptUnseen: false,
    transcriptDrawnHash: "",
    conversationVisibility: storedConversationVisibility,
    viewMode: "conversation",
    projectView: null,
    filesRequest: 0,
    fileSaveRequest: 0,
    codeNavRequest: 0,
    codeNavController: null,
    gitRequest: 0,
    filesController: null,
    fileSaveController: null,
    gitController: null,
    fileReaderPreferences: storedFileReaderPreferences,
    messageHistory: new Map(),
    messageHistoryNavigation: null,
    composerDrafts: storedComposerDrafts,
    composerDraftIdentity: null,
    composerDraftSequence: Math.max(
      0,
      ...[...storedComposerDrafts.values()].map((draft) => draft.version),
    ),
    composerDraftTimestamp: Math.max(
      Date.now(),
      ...[...storedComposerDrafts.values()].map((draft) => draft.updatedAt),
      ...[...storedComposerDraftState.tombstones.values()].map((item) => item.deletedAt),
    ),
    composerDraftStorageTimer: null,
    composerDraftTombstones: storedComposerDraftState.tombstones,
    optimisticComposerClears: new Map(),
    composerSending: false,
    specialKeySending: null,
    specialKeyQueue: [],
    specialKeyStatuses: new Map(),
    inFlightComposerText: null,
    inFlightComposerIdentity: null,
    composerRevision: 0,
    queuedComposerMessages: [],
    attachments: [],
    attachmentPaneId: null,
    attachmentInstanceKey: null,
    pendingKillId: null,
    pendingSessionEdit: null,
    pendingResumeId: null,
    registryEnabled: false,
    pendingResumeOn: null,
    resumingOn: false,
    paneModels: null,
    paneModelsRequest: 0,
    modelSwitchingPaneId: null,
    duplicatingPaneId: null,
    resumingPaneId: null,
    /// Machine id -> the node's own Quick Resume document, as the coordinator read it.
    fleetRecovery: new Map(),
    recoveryPoll: null,
    rawFitTimer: null,
    rawFitKey: null,
    /// Machines with a Quick Resume start in flight from this browser.
    recoveryBusy: new Set(),
    /// Machine id -> the node's own update document, as the coordinator read it.
    fleetUpdates: new Map(),
    fleetUpdatePoll: null,
    /// Machines with a verb in flight from this browser, so its buttons stay
    /// disabled until the node answers.
    updateBusy: new Set(),
    pendingUpdate: null,
    railCollapsed: readLocalStorage("atmux.rail-collapsed") === "true",
    pulseOpen: initialRoute.view === "usage",
    pulseAccount: requestedPulseAccount || storedPulseAccount,
    pulseAccounts: [],
    pulseAccountsLoaded: false,
    pulseAccountsLoading: false,
    pulseAccountsError: null,
    pulseTab: "dashboard",
    pulseData: {},
    pulseErrors: {},
    pulseLoading: false,
    pulseFailures: 0,
    pulseGeneration: 0,
    pulseTimer: null,
    pulseSource: null,
    pulseSourceAccount: null,
    pulseStreamGeneration: 0,
    pulseStreamAwaitingInitial: false,
    pulseEventRevision: null,
    pulseEventFailures: 0,
    pulseReconnectTimer: null,
    pulseInvalidationTimer: null,
    pulseInvalidationQueued: false,
    pulseLastLoadedAt: null,
    pulseMutation: false,
    pulseMutationId: 0,
    pulseIssuedToken: null,
    pulseReport: { days: 30, granularity: "daily", drill: "model" },
  };

  const sessionNodes = new Map();
  const machineNodes = new Map();
  const $ = (id) => document.getElementById(id);
  const sessionList = $("sessions");
  const pane = $("pane");
  const conversation = $("conversation");
  const filesPanel = $("files-panel");
  const gitPanel = $("git-panel");

  function mobileViewportActive() {
    return window.matchMedia("(max-width: 720px)").matches;
  }

  function revealFocusedLaunchMemoryControl() {
    const focused = document.activeElement;
    if (!$("launch-dialog").open
        || !focused?.matches("#launch-memory, #launch-memory-custom")) return;
    requestAnimationFrame(() => {
      if (document.activeElement === focused) {
        focused.scrollIntoView({ block: "nearest", inline: "nearest" });
      }
    });
  }

  // Measure the layout viewport, never the visual viewport.
  //
  // On mobile `body` is `position: fixed`, so it is anchored to the layout
  // viewport. WebKit does not resize the layout viewport for the software
  // keyboard -- `interactive-widget` is a Chromium key that iOS ignores. It
  // shrinks the *visual* viewport and, when the focused field would sit under
  // the keyboard, additionally offsets it by `visualViewport.offsetTop` to
  // reveal that field. Feeding `visualViewport.height` back into the app box
  // shrank the app against the layout viewport's top edge while WebKit had
  // already scrolled the visual viewport down by roughly the keyboard height,
  // computed against the taller pre-shrink layout. The two compounded: the only
  // part of the app still inside the visible band was its bottom row, so the
  // composer appeared pinned to the top of the screen with dead space beneath
  // it and the transcript scrolled out of view.
  //
  // Measuring the layout viewport keeps the app box and WebKit's reveal scroll
  // in one coordinate space, so the native reveal is the only thing that moves
  // and it lands where WebKit intends. Android Chrome shrinks `innerHeight`
  // itself for the keyboard (`interactive-widget=resizes-content`), which keeps
  // the composer above the keyboard with no visual viewport offset -- so this
  // single measurement is correct on both platforms.
  function syncMobileViewport() {
    if (!mobileViewportActive()) {
      document.documentElement.style.removeProperty("--app-height");
      return;
    }
    const height = window.innerHeight;
    if (Number.isFinite(height) && height > 0) {
      if (document.documentElement.style.getPropertyValue("--app-height") !== `${Math.floor(height)}px`) {
        document.documentElement.style.setProperty("--app-height", `${Math.floor(height)}px`);
      }
      revealFocusedLaunchMemoryControl();
    }
  }

  window.addEventListener("resize", syncMobileViewport, { passive: true });
  window.addEventListener("orientationchange", syncMobileViewport, { passive: true });
  syncMobileViewport();
  installMobileViewportRecovery({
    window, document, isMobile: mobileViewportActive, syncViewport: syncMobileViewport,
  });

  if (state.pulseOpen) {
    state.selected = null;
    state.selectedMachine = null;
  }

  function setRailCollapsed(collapsed) {
    state.railCollapsed = Boolean(collapsed);
    document.body.classList.toggle("rail-collapsed", state.railCollapsed);
    const toggle = $("rail-toggle");
    toggle.textContent = state.railCollapsed ? "›" : "‹";
    toggle.setAttribute("aria-expanded", String(!state.railCollapsed));
    toggle.setAttribute("aria-label", state.railCollapsed ? "Expand agent list" : "Collapse agent list");
    toggle.title = state.railCollapsed ? "Expand agent list" : "Collapse agent list";
    writeLocalStorage("atmux.rail-collapsed", String(state.railCollapsed));
  }

  setRailCollapsed(state.railCollapsed);

  function parseEvent(event) {
    try { return JSON.parse(event.data); }
    catch { toast("Received an invalid server event"); return null; }
  }

  function applyOverview(data) {
    const previousSessions = state.sessions;
    const result = reduceOverview({ revision: state.revision, sessions: state.sessions }, data);
    if (result.resync) {
      // This patch does not continue the revision we hold, so the session list
      // may already be wrong. Reconnect for an authoritative snapshot instead
      // of merging into a gap.
      connectOverview();
      return false;
    }
    mergeComposerDraftState(
      state.composerDrafts,
      state.composerDraftTombstones,
      readLocalStorage(COMPOSER_DRAFT_STORAGE_KEY),
      Date.now(),
      protectedComposerDraftKeys(),
    );
    syncComposerDraftTimestamp();
    const snapshotMachines = Array.isArray(data.sessions)
      ? new Set((Array.isArray(data.machines) ? data.machines : [])
        .filter((machine) => machine?.online === true)
        .map((machine) => machine.id))
      : null;
    let draftsChanged = false;
    for (const [id, previous] of previousSessions) {
      const current = result.sessions.get(id);
      if (!current && snapshotMachines
          && !snapshotMachines.has(sessionMachineId(previous))) continue;
      const previousIdentity = composerDraftIdentity(previous);
      const currentIdentity = composerDraftIdentity(current);
      if (!currentIdentity || currentIdentity.key !== previousIdentity?.key) {
        draftsChanged = forgetComposerDraft(previousIdentity, true, false) || draftsChanged;
      }
    }
    if (Array.isArray(data.sessions)) {
      for (const key of staleComposerDraftKeys(state.composerDrafts, result.sessions, data.machines)) {
        draftsChanged = forgetComposerDraft({ key, persistent: true }, true, false) || draftsChanged;
      }
    }
    if (draftsChanged) saveComposerDraftStorage(true);
    state.sessions = result.sessions;
    state.revision = result.revision;
    if (Array.isArray(data.machines) && data.machines.length) state.machines = data.machines;
    bindComposerDraftToSelection();
    const selected = state.sessions.get(state.selected);
    if (selected && !paneOutputMatchesSession(paneOutputBinding(previousSessions.get(state.selected)), selected)) {
      // A reused pane ID is a new output owner. Retire the old stream before
      // any of its queued callbacks can populate or export the replacement.
      connectPane(false);
    }
    setHealth(data.health);
    reconcileSelection();
    render();
    return true;
  }

  function machineOf(session) {
    if (!session) return null;
    const localMachineId = state.machines.find((machine) => machine.kind === "local")?.id || "local";
    const id = sessionMachineId(session, localMachineId);
    return state.machines.find((machine) => machine.id === id) || null;
  }

  function connectOverview() {
    state.overviewSource?.close();
    state.overviewSource = createOverviewStream({
      createSource: () => new EventSource("/api/v1/events"),
      onConnection(connection) {
        if (connection !== state.overviewConnection) {
          state.overviewConnectionSince = Date.now();
          if (state.overviewGraceTimer !== null) clearTimeout(state.overviewGraceTimer);
          state.overviewGraceTimer = null;
          if (connection === "reconnecting" || connection === "stale") {
            // Repaint once the grace period ends, in case nothing else does.
            state.overviewGraceTimer = setTimeout(() => {
              state.overviewGraceTimer = null;
              renderOverviewConnection();
            }, OVERVIEW_DROP_GRACE_MS + 50);
          }
        }
        state.overviewConnection = connection;
        renderCounts();
      },
      onOverview(event) {
        const data = parseEvent(event);
        return data ? applyOverview(data) : false;
      },
      onProtocolError: (event) => setHealth(event.data || "stream protocol error"),
    });
  }

  function resetProjectView() {
    state.codeNavController?.abort();
    state.codeNavController = null;
    state.codeNavRequest += 1;
    state.filesController?.abort();
    state.fileSaveController?.abort();
    state.gitController?.abort();
    state.filesController = null;
    state.fileSaveController = null;
    state.gitController = null;
    state.filesRequest += 1;
    state.fileSaveRequest += 1;
    state.gitRequest += 1;
    state.projectView = null;
    // Pane-scoped project data must disappear synchronously on selection
    // changes so source from one owner can never flash beneath another.
    filesPanel.hidden = true;
    gitPanel.hidden = true;
    $("files-breadcrumbs").replaceChildren();
    $("files-list").replaceChildren();
    $("file-viewer").replaceChildren();
    $("git-summary").replaceChildren();
    $("git-changes").replaceChildren();
    $("git-diff").replaceChildren();
  }

  function connectPane(resetProject = true) {
    state.rawFitKey = null;
    state.paneSource?.close();
    stopTranscriptPolling();
    if (resetProject) resetProjectView();
    state.paneSource = null;
    state.paneLines = [];
    state.paneOutputBinding = null;
    state.paneRevision = 0;
    state.paneError = null;
    state.panePointerDown = false;
    state.pendingPaneRender = false;
    state.paneFollowing = true;
    state.paneReadingScrollTop = 0;
    state.paneExpectedScrollTop = null;
    state.paneModels = null;
    state.transcript = { available: false, source: "agent", messages: [], truncated: false, error: null };
    state.transcriptHash = "";
    state.agentSummary = null; state.summaryOpen = false;
    state.transcriptRequest += 1;
    state.transcriptPointerDown = false;
    state.pendingTranscriptRender = false;
    state.pendingTranscriptFilterChange = false;
    state.transcriptFollowing = true;
    state.transcriptExpectedScrollTop = null;
    // Opening or switching agents always starts pinned at the newest message.
    state.transcriptReadingScrollTop = 0;
    state.transcriptPendingBottom = true;
    state.transcriptUnseen = false;
    state.transcriptDrawnHash = "";
    renderTranscriptJump();
    pane.textContent = "";
    conversation.replaceChildren();
    renderConversationMetrics();
    const selected = state.sessions.get(state.selected);
    if (!selected || document.hidden) return;
    const binding = paneOutputBinding(selected);
    void refreshModels(state.selected);
    // The branch belongs in the agent header, so discover it in the
    // background without making the reader open the Git tab first.
    void loadGitSummary();
    $("stream-state").textContent = "Connecting…";
    startTranscriptPolling();
    const source = new EventSource(`/api/v1/panes/${encodeURIComponent(state.selected)}/events`);
    state.paneSource = source;
    const current = () => state.paneSource === source
      && paneOutputMatchesSession(binding, state.sessions.get(state.selected));
    source.addEventListener("pane.snapshot", (event) => {
      if (!current()) return;
      const data = parseEvent(event); if (!data) return;
      state.paneLines = contentToLines(data.content);
      state.paneOutputBinding = binding;
      state.paneRevision = data.revision;
      state.paneError = null;
      drawPane(true);
      scheduleTranscript(100);
      $("stream-state").textContent = "Live";
      render();
    });
    source.addEventListener("pane.patch", (event) => {
      if (!current()) return;
      const data = parseEvent(event); if (!data) return;
      if (!paneOutputMatchesSession(state.paneOutputBinding, selected)) {
        connectPane(false);
        return;
      }
      const result = applyPanePatch(state.paneLines, state.paneRevision, data);
      if (!result.applied) {
        $("stream-state").textContent = "Resyncing…";
        connectPane(false);
        return;
      }
      state.paneLines = result.lines;
      state.paneRevision = result.revision;
      state.paneError = null;
      drawPane(false);
      scheduleTranscript(350);
      $("stream-state").textContent = "Live";
    });
    source.addEventListener("pane.removed", () => {
      if (!current()) return;
      forgetComposerDraft(selectedComposerDraftIdentity(), true);
      selectSession(null, "replace");
    });
    // A failure on the owning machine belongs to this pane, not to the local
    // tmux monitor, so it never touches the global health alert.
    source.addEventListener("pane.error", (event) => {
      if (!current()) return;
      const data = parseEvent(event); if (!data) return;
      state.paneError = data;
      $("stream-state").textContent = paneErrorLabel(data.kind);
      render();
    });
    source.addEventListener("protocol.error", (event) => {
      if (!current()) return;
      state.paneError = { error: event.data || "stream protocol error", kind: "protocol" };
      $("stream-state").textContent = paneErrorLabel("protocol");
      render();
    });
    source.onerror = () => { if (current()) $("stream-state").textContent = "Reconnecting…"; };
  }

  async function refreshModels(paneId) {
    if (!paneId) return;
    const generation = ++state.paneModelsRequest;
    try {
      const capabilities = await request(`/api/v1/panes/${encodeURIComponent(paneId)}/models`);
      if (state.selected !== paneId || generation !== state.paneModelsRequest) return;
      state.paneModels = capabilities;
    } catch (error) {
      if (state.selected !== paneId || generation !== state.paneModelsRequest) return;
      state.paneModels = {
        pane_id: paneId,
        harness: state.sessions.get(paneId)?.agent || "agent",
        current: null,
        models: [],
        model_options: [],
        effort_options: [],
        note: error.message,
      };
    }
    render();
  }

  /// The three controls the picker shows, as one comparable value. A switch
  /// uses it to tell a pane that has caught up from one still reporting the
  /// state it had before the change.
  function paneModelSignature(models) {
    if (!models) return "";
    return [models.current, models.effort, models.fast].map((value) => value ?? "").join("|");
  }

  /// Takes a capability snapshot as the picker's current truth, unless the user
  /// has since selected another pane. Claims the read generation so a `/models`
  /// request already in flight cannot overwrite it with older observations.
  function adoptPaneModels(paneId, capabilities) {
    if (!capabilities || !paneId || state.selected !== paneId) return false;
    state.paneModelsRequest += 1;
    state.paneModels = capabilities;
    return true;
  }

  /// Some harnesses print their confirmation a beat after answering the switch.
  /// Re-read the pane a few times over about three seconds, stopping as soon as
  /// it reports controls other than the ones it had before.
  async function settlePaneModels(paneId, before) {
    for (let attempt = 0; attempt < 3; attempt += 1) {
      await new Promise((resolve) => { setTimeout(resolve, 1000); });
      if (state.selected !== paneId) return;
      await refreshModels(paneId);
      if (paneModelSignature(state.paneModels) !== before) return;
    }
  }

  function scheduleTranscript(delay = 300) {
    state.transcriptPoll?.schedule(delay);
  }

  function stopTranscriptPolling() {
    state.transcriptPoll?.close();
    state.transcriptPoll = null;
    state.transcriptRequest += 1;
  }

  function startTranscriptPolling() {
    const paneId = state.selected;
    const selected = state.sessions.get(paneId);
    if (state.transcriptPoll || !selected || document.hidden || state.viewMode !== "conversation") return;
    const binding = paneOutputBinding(selected);
    const current = () => state.selected === paneId
      && paneOutputMatchesSession(binding, state.sessions.get(paneId));
    state.transcriptPoll = createTranscriptPoller({
      load(signal) {
        const suffix = state.transcriptHash ? `?known_hash=${encodeURIComponent(state.transcriptHash)}` : "";
        return Promise.all([
          request(`/api/v1/panes/${encodeURIComponent(paneId)}/transcript${suffix}`, { signal }),
          request(`/api/v1/panes/${encodeURIComponent(paneId)}/summary`, { signal }).catch(() => undefined),
        ]).then(([transcript, summary]) => ({ ...transcript, summary }));
      },
      onData(data) {
        if (!current()) return;
        state.transcriptRequest += 1;
        const next = reduceTranscript(state.transcript, data);
        const summary = data.summary === undefined ? state.agentSummary : data.summary?.enabled ? data.summary : null;
        const summaryChanged = JSON.stringify(state.agentSummary) !== JSON.stringify(summary);
        state.agentSummary = summary;
        const shouldDraw = summaryChanged || next.transcript.messages !== state.transcript.messages
          || next.transcript.available !== state.transcript.available
          || next.transcript.source !== state.transcript.source
          || next.transcript.truncated !== state.transcript.truncated
          || Boolean(state.transcript.error);
        state.transcriptHash = next.hash;
        state.transcript = next.transcript;
        renderConversationMetrics();
        if (shouldDraw) drawConversation();
        renderViewMode();
      },
      onError(error) {
        if (!current()) return;
        state.transcript.error = error.message;
        drawConversation();
      },
    });
    scheduleTranscript(0);
  }

  function renderToolCard(message, expandedTools, grouped = false) {
    const details = document.createElement("details");
    details.className = "tool-card";
    if (grouped) details.classList.add("tool-card-group-item");
    details.dataset.transcriptId = String(message.id || "");
    details.dataset.transcriptVisibility = "internal";
    details.open = expandedTools.has(details.dataset.transcriptId);
    const summary = document.createElement("summary");
    const name = String(message.tool_name || "Tool");
    const resultSignal = toolResultSignal(message);
    const suffix = resultSignal === "error" ? "error"
      : resultSignal === "approval" ? "approval required"
        : message.tool_output ? "result" : "";
    const label = document.createElement("span");
    label.className = "tool-label";
    label.textContent = [name, suffix].filter(Boolean).join(" · ");
    summary.append(label, document.createTextNode(" "), renderEntryMetrics(message));
    details.classList.toggle("has-errors", resultSignal === "error");
    details.append(summary);
    const body = document.createElement("div");
    body.className = "tool-body";
    for (const [label, value] of [["Input", message.tool_input], ["Result", message.tool_output]]) {
      if (!value) continue;
      const section = document.createElement("section");
      const heading = document.createElement("span"); heading.textContent = label;
      const pre = document.createElement("pre"); linkifyInto(pre, value);
      section.append(heading, pre); body.append(section);
    }
    if (body.childNodes.length) details.append(body);
    return details;
  }

  /// A compaction replaces most of the context with a long summary. Show it
  /// as one collapsed row; the summary opens on demand.
  function renderCompactionCard(message, expandedTools) {
    const details = document.createElement("details");
    details.className = "compaction-card";
    details.dataset.transcriptId = String(message.id || "");
    details.dataset.transcriptVisibility = transcriptVisibilityKind(message);
    details.open = expandedTools.has(details.dataset.transcriptId);
    const summary = document.createElement("summary");
    summary.textContent = compactionSummaryLabel(message);
    details.append(summary);
    if (typeof message.markdown === "string" && message.markdown.trim()) {
      const body = document.createElement("div");
      body.className = "markdown-body compaction-body";
      body.append(markdownFragment(message.markdown));
      details.append(body);
    } else {
      details.classList.add("compaction-empty");
    }
    return details;
  }

  function renderToolGroup(group, expandedTools) {
    const details = document.createElement("details");
    details.className = "tool-card tool-call-group";
    details.classList.toggle("has-errors", transcriptErrorCount(group.messages) > 0);
    if (group.kind === "tool-run") details.classList.add("tool-run-group");
    details.dataset.transcriptId = group.id;
    details.dataset.transcriptMembers = JSON.stringify(
      group.messages.map((message) => String(message?.id || "")).filter(Boolean),
    );
    details.dataset.transcriptVisibility = "internal";
    details.open = expandedTools.has(group.id);
    const summary = document.createElement("summary");
    summary.className = "tool-call-group-summary";
    summary.textContent = coordinationGroupSummary(group);
    summary.setAttribute(
      "aria-label",
      `${summary.textContent}; ${group.messages.length} calls and results`,
    );
    // Mobile browsers may scroll an expanding <details> to keep its newly
    // exposed body visible. The reader chose this visible summary, so retain
    // their exact Conversation offset while revealing the original calls.
    summary.addEventListener("click", () => {
      const paneId = state.selected;
      const transcriptGeneration = state.transcriptRequest;
      const scrollTop = conversation.scrollTop;
      requestAnimationFrame(() => {
        if (state.selected === paneId
          && state.transcriptRequest === transcriptGeneration
          && state.viewMode === "conversation"
          && details.isConnected
          && details.closest("#conversation") === conversation) {
          conversation.scrollTop = scrollTop;
          state.transcriptFollowing = false;
          state.transcriptExpectedScrollTop = scrollTop;
        }
      });
    });
    const body = document.createElement("div");
    body.className = "tool-call-group-items";
    for (const message of group.messages) body.append(renderToolCard(message, expandedTools, true));
    details.append(summary, body);
    return details;
  }

  function conversationMeasurable() {
    return !conversation.hidden && conversation.clientHeight > 0;
  }

  function renderEntryMetrics(message) {
    const metrics = document.createElement("span");
    metrics.className = "entry-metrics";
    metrics.textContent = transcriptTokenSummary([message]);
    metrics.title = "Recorded model-request tokens, counted once on one entry per request. Input includes cached tokens. — means no separate usage was recorded for this entry.";
    return metrics;
  }

  function renderConversationMetrics() {
    const metrics = $("conversation-metrics");
    metrics.textContent = conversationMetricsSummary(state.transcript);
    metrics.hidden = state.viewMode !== "conversation" || !metrics.textContent;
  }

  /// Landing on the newest message has to survive late layout. The agent view
  /// is still hidden while the first transcript arrives, and markdown, code
  /// blocks and web fonts keep growing the log after the synchronous write, so
  /// a single scrollTop assignment used to leave the reader at the very top.
  function scrollConversationToBottom() {
    if (!conversationMeasurable()) {
      state.transcriptPendingBottom = true;
      return;
    }
    state.transcriptPendingBottom = false;
    state.transcriptUnseen = false;
    const settle = () => {
      if (!conversationMeasurable() || !state.transcriptFollowing) return;
      conversation.scrollTop = conversation.scrollHeight;
      state.transcriptReadingScrollTop = conversation.scrollTop;
      state.transcriptExpectedScrollTop = conversation.scrollTop;
    };
    settle();
    requestAnimationFrame(settle);
    renderTranscriptJump();
  }

  /// The pill only claims there is something new: a reader who scrolled up and
  /// received nothing since is left undisturbed.
  function renderTranscriptJump() {
    const jump = $("conversation-jump");
    if (!jump) return;
    jump.hidden = !state.transcriptUnseen
      || !conversationMeasurable()
      || state.transcriptFollowing;
  }

  // A pane that was hidden or zero-height when the transcript rendered gains a
  // box later; that is the moment the deferred jump to the tail can happen.
  if (typeof ResizeObserver === "function") {
    new ResizeObserver(() => {
      if (state.transcriptPendingBottom) scrollConversationToBottom();
      else renderTranscriptJump();
    }).observe(conversation);
  }

  function drawConversation(filterChanged = false) {
    if (state.transcriptPointerDown || selectionTouchesPane(conversation, window.getSelection())) {
      state.pendingTranscriptRender = true;
      state.pendingTranscriptFilterChange ||= filterChanged;
      return;
    }
    const sticky = stickyBottomState(conversation, state.transcriptFollowing, !conversation.hidden);
    const shouldFollow = sticky.follow;
    const readingOffset = sticky.measurable
      ? conversation.scrollTop
      : state.transcriptReadingScrollTop;
    const retainAfterFilter = filterChanged
      ? (node) => node.dataset.transcriptVisibility === "agent"
        || (node.dataset.transcriptVisibility === "human" && state.conversationVisibility.human)
        || (node.dataset.transcriptVisibility === "internal" && state.conversationVisibility.internal)
      : null;
    const readingAnchor = shouldFollow || !sticky.measurable
      ? null
      : transcriptReadingAnchor(conversation, retainAfterFilter);
    const expandedTools = new Set(
      [...conversation.querySelectorAll("details.tool-card[open], details.compaction-card[open]")]
        .map((node) => node.dataset.transcriptId)
        .filter(Boolean),
    );
    const nodes = [];
    if (state.agentSummary?.digest) {
      const summary = document.createElement("details"); summary.className = "conversation-summary";
      summary.open = Boolean(state.summaryOpen);
      summary.addEventListener("toggle", () => { if (summary.isConnected) state.summaryOpen = summary.open; });
      const label = document.createElement("summary"); label.textContent = `Summary${state.agentSummary.stale ? " · refreshing" : ""}`;
      const title = document.createElement("p"); title.className = "summary-title"; title.textContent = state.agentSummary.title;
      const body = document.createElement("p"); body.textContent = state.agentSummary.digest;
      summary.append(label, title, body); nodes.push(summary);
    }
    if (state.transcript.available && state.transcript.note) {
      const notice = document.createElement("p");
      notice.className = "transcript-notice transcript-owner-note";
      notice.setAttribute("role", "status");
      notice.textContent = state.transcript.note;
      nodes.push(notice);
    }
    if (state.transcript.truncated && state.conversationVisibility.internal) {
      const notice = document.createElement("p");
      notice.className = "transcript-notice";
      notice.textContent = "Showing the newest bounded part of this session log.";
      nodes.push(notice);
    }
    const sourceMessages = state.transcript.available && Array.isArray(state.transcript.messages)
      ? state.transcript.messages : [];
    const visibleMessages = filterTranscriptMessages(sourceMessages, state.conversationVisibility);
    const transcriptItems = compactTranscriptItems(visibleMessages);
    let renderedMessages = 0;
    for (const item of transcriptItems) {
      if (item.kind === "tool-group" || item.kind === "tool-run") {
        nodes.push(renderToolGroup(item, expandedTools));
        renderedMessages += item.messages.length;
        continue;
      }
      const message = item.message;
      if (!message) continue;
      if (transcriptItemKind(message) === "tool") {
        nodes.push(renderToolCard(message, expandedTools));
        renderedMessages += 1;
        continue;
      }
      if (message.kind === "compaction") {
        nodes.push(renderCompactionCard(message, expandedTools));
        renderedMessages += 1;
        continue;
      }
      const visibility = transcriptVisibilityKind(message);
      if (visibility === "internal" && typeof message.markdown !== "string") continue;
      const article = document.createElement("article");
      article.className = `message-card ${String(message.role || "internal")} ${visibility}`;
      article.dataset.transcriptId = String(message.id || "");
      article.dataset.transcriptVisibility = visibility;
      const label = document.createElement("header");
      label.textContent = transcriptRoleLabel(message);
      label.append(document.createTextNode(" "), renderEntryMetrics(message));
      const body = document.createElement("div");
      body.className = "markdown-body";
      body.append(markdownFragment(message.markdown));
      article.append(label, body); nodes.push(article);
      renderedMessages += 1;
    }
    const hasOnlyNotice = nodes.length > 0 && nodes.every((node) => node.classList.contains("transcript-notice"));
    if (!renderedMessages && (!nodes.length || hasOnlyNotice)) {
      const empty = document.createElement("div");
      empty.className = "conversation-empty";
      const hiddenMessages = Math.max(0, sourceMessages.length - visibleMessages.length);
      empty.textContent = hiddenMessages > 0
        ? "No agent messages to show. Change Conversation visibility or choose Show all."
        : (state.transcript.error && state.conversationVisibility.internal)
          ? `Conversation log unavailable: ${state.transcript.error}. Raw pane remains available.`
        : (state.transcript.available
          ? `Waiting for ${state.transcript.source} conversation messages…`
          : (state.transcript.note || "No agent session log is mapped yet. Raw pane remains available."));
      nodes.push(empty);
    } else if (state.transcript.error) {
      // A failed refresh must never look like a quiet agent.
      const notice = document.createElement("p");
      notice.className = "transcript-notice";
      notice.textContent = `Conversation log update failed: ${state.transcript.error}`;
      nodes.unshift(notice);
    }
    const changed = state.transcriptDrawnHash !== state.transcriptHash;
    state.transcriptDrawnHash = state.transcriptHash;
    conversation.replaceChildren(...nodes);
    state.pendingTranscriptRender = false;
    state.transcriptFollowing = shouldFollow;
    state.pendingTranscriptFilterChange = false;
    // Stream updates replace transcript cards wholesale. Following is an
    // explicit reader choice, not merely a position that happens to be near
    // the tail. When reading, anchor the same transcript item in the viewport
    // and offer the jump pill rather than dragging the reader to the tail.
    if (!sticky.measurable) {
      state.transcriptReadingScrollTop = readingOffset;
      state.transcriptExpectedScrollTop = null;
      state.transcriptPendingBottom = state.transcriptPendingBottom || shouldFollow;
    } else if (shouldFollow) {
      scrollConversationToBottom();
    } else {
      restoreTranscriptReadingAnchor(conversation, readingAnchor, readingOffset);
      state.transcriptReadingScrollTop = conversation.scrollTop;
      state.transcriptExpectedScrollTop = conversation.scrollTop;
      if (changed) state.transcriptUnseen = true;
    }
    renderTranscriptJump();
  }

  function flushPendingTranscriptRender() {
    if (!state.pendingTranscriptRender
      || state.transcriptPointerDown
      || selectionTouchesPane(conversation, window.getSelection())) return;
    drawConversation(state.pendingTranscriptFilterChange);
  }

  function emptyProjectView(paneId) {
    return {
      paneId,
      files: {
        path: "", breadcrumbs: [{ name: "Project", path: "" }], listing: null,
        file: null, loading: false, error: null,
        editing: false, editDraft: "", saving: false, reloading: false, saveError: null,
        conflict: false, selection: null,
        listScrolls: new Map(), viewerScrolls: new Map(),
        history: { back: [], forward: [] }, symbol: null, symbolPanel: null,
        symbolCount: 0, navModel: null, pendingReveal: null,
      },
      git: {
        summary: null, diff: null, loading: false, diffLoading: false, error: null,
        changesScroll: 0, diffScrolls: new Map(),
      },
    };
  }

  function selectedProjectView() {
    if (!state.selected) return null;
    if (!state.projectView || state.projectView.paneId !== state.selected) {
      state.projectView = emptyProjectView(state.selected);
    }
    return state.projectView;
  }

  function discardFileEdit(files) {
    if (!files) return;
    // A conflict or in-flight mutation means the preview/hash may no longer
    // describe disk. Dropping that preview forces the next Files visit to GET
    // a fresh owner-issued base instead of making stale Edit available.
    const invalidatePreview = files.conflict || files.saving || files.reloading;
    state.fileSaveController?.abort();
    state.fileSaveController = null;
    state.fileSaveRequest += 1;
    files.editing = false;
    files.editDraft = "";
    files.saving = false;
    files.reloading = false;
    files.saveError = null;
    files.conflict = false;
    files.selection = null;
    if (invalidatePreview) files.file = null;
  }

  function confirmDiscardFileEdit(files = state.projectView?.files) {
    if (!fileEditHasUnsavedWork(files)) return true;
    if (!window.confirm("Discard your unsaved file edits? This cannot be undone.")) return false;
    discardFileEdit(files);
    return true;
  }

  function projectErrorMessage(error, subject) {
    if (error?.status === 503) return `The selected machine is offline. ${subject} will be available when it reconnects.`;
    if (error?.status === 502) return `The owner machine could not load ${subject.toLowerCase()}.`;
    if (error?.status === 404) return `${subject} no longer exists.`;
    if (error?.status === 400) return `The selected path is not safe to open.`;
    return `${subject} is unavailable.`;
  }

  function projectStateNode(message, error = false) {
    const node = document.createElement("p");
    node.className = `project-state${error ? " error" : ""}`;
    node.setAttribute("role", error ? "alert" : "status");
    node.textContent = message;
    return node;
  }

  function applyFileReaderPreferences(viewer) {
    const preferences = state.fileReaderPreferences;
    viewer.classList.toggle("file-wrap", preferences.wrap);
    for (const size of FILE_READER_SIZES) {
      viewer.classList.toggle(`file-size-${size}`, preferences.size === size);
    }
    const editor = viewer.querySelector(".file-editor");
    if (editor) editor.wrap = preferences.wrap ? "soft" : "off";
  }

  function updateFileReaderPreference(change) {
    state.fileReaderPreferences = fileReaderPreferences({
      ...state.fileReaderPreferences,
      ...change,
    });
    writeLocalStorage(
      FILE_READER_STORAGE_KEY,
      fileReaderPreferenceJson(state.fileReaderPreferences),
    );
    applyFileReaderPreferences($("file-viewer"));
  }

  function fileReaderControls() {
    const controls = document.createElement("div");
    controls.className = "file-display-controls";
    controls.setAttribute("role", "group");
    controls.setAttribute("aria-label", "File display");

    const wrap = document.createElement("button");
    wrap.type = "button";
    wrap.className = "subtle file-wrap-toggle";
    wrap.textContent = "Wrap";
    wrap.title = "Wrap long file lines";
    wrap.setAttribute("aria-pressed", String(state.fileReaderPreferences.wrap));
    wrap.addEventListener("click", () => {
      const enabled = !state.fileReaderPreferences.wrap;
      updateFileReaderPreference({ wrap: enabled });
      wrap.setAttribute("aria-pressed", String(enabled));
    });

    const size = document.createElement("select");
    size.className = "file-text-size";
    size.title = "File text size";
    size.setAttribute("aria-label", "File text size");
    for (const [value, label] of [["small", "Small"], ["medium", "Medium"], ["large", "Large"]]) {
      const option = document.createElement("option");
      option.value = value;
      option.textContent = label;
      size.append(option);
    }
    size.value = state.fileReaderPreferences.size;
    size.addEventListener("change", () => updateFileReaderPreference({ size: size.value }));
    controls.append(wrap, size);
    return controls;
  }

  function appendSource(parent, content, language, diff = false, onSelectLine = null, selection = null, navigation = null) {
    const source = String(content || "").slice(0, MAX_PROJECT_SOURCE_CHARS);
    const code = document.createElement("pre");
    code.className = "code-source";
    const lines = source.split("\n").slice(0, MAX_PROJECT_SOURCE_LINES);
    const tokenLines = diff ? null : (navigation?.tokenLines || tokenizeSource(lines.join("\n"), language));
    const interactive = Boolean(navigation?.interactive);
    for (let index = 0; index < lines.length; index += 1) {
      const row = document.createElement("span");
      const diffKind = diff ? diffLineKind(lines[index]) : null;
      row.className = `code-line${diffKind && diffKind !== "context" ? ` diff-line-${diffKind}` : ""}`;
      row.dataset.line = String(index + 1);
      const number = document.createElement(onSelectLine ? "button" : "span");
      number.className = "code-line-number";
      number.textContent = String(index + 1);
      if (onSelectLine) {
        const line = index + 1;
        number.type = "button";
        number.title = `Select line ${line}`;
        number.setAttribute("aria-label", `Select line ${line}`);
        number.addEventListener("click", (event) => onSelectLine(line, event.shiftKey));
        if (selection && line >= selection.start && line <= selection.end) {
          row.classList.add("selected");
          number.setAttribute("aria-pressed", "true");
        } else number.setAttribute("aria-pressed", "false");
      }
      row.append(number);
      const lineContent = document.createElement("span");
      lineContent.className = "code-line-content";
      if (diff) {
        lineContent.append(document.createTextNode(lines[index]));
      } else {
        const importRanges = interactive ? navigation.imports?.lines.get(index + 1) || [] : [];
        let column = 0;
        for (const segment of tokenLines[index] || []) {
          const token = document.createElement("span");
          if (segment.kind !== "plain") token.className = `syntax-${segment.kind}`;
          token.textContent = segment.text;
          if (interactive) {
            const end = column + segment.text.length;
            const range = importRanges.find((candidate) => column < candidate.end && end > candidate.start);
            if (range && segment.text.trim()) {
              token.classList.add("code-import");
              token.dataset.importSpec = range.spec;
              if (range.symbol) token.dataset.importSymbol = range.symbol;
              token.title = "Open imported file";
            } else if (["identifier", "function", "type", "constant"].includes(segment.kind)
              && codeSymbolValid(segment.text)) {
              token.classList.add("code-symbol");
              token.dataset.symbol = segment.text;
              token.dataset.column = String(column + 1);
            }
          }
          column += segment.text.length;
          lineContent.append(token);
        }
      }
      row.append(lineContent);
      code.append(row);
    }
    parent.append(code);
    return code;
  }

  /// Tokens, imports, and language for the open file, computed once per
  /// file revision so selection and panel updates never re-tokenize.
  function fileNavigationModel(files) {
    const file = files?.file;
    if (!file || typeof file.content !== "string") return null;
    const language = codeLanguage(file.path, file.language, file.content);
    const cached = files.navModel;
    if (cached && cached.content === file.content && cached.language === language && cached.path === file.path) return cached;
    const source = file.content.slice(0, MAX_PROJECT_SOURCE_CHARS).split("\n").slice(0, MAX_PROJECT_SOURCE_LINES).join("\n");
    const tokenLines = tokenizeSource(source, language);
    const interactive = codeLanguageNavigable(language);
    const model = {
      path: file.path,
      content: file.content,
      language,
      tokenLines,
      interactive,
      imports: interactive ? sourceImports(source, language) : { names: new Map(), lines: new Map() },
    };
    files.navModel = model;
    return model;
  }

  function codeLineText(model, line) {
    return (model?.tokenLines?.[line - 1] || []).map((segment) => segment.text).join("");
  }

  function highlightSymbolOccurrences(symbol) {
    const viewer = $("file-viewer");
    for (const node of viewer.querySelectorAll(".code-symbol.symbol-match, .code-symbol.symbol-origin")) {
      node.classList.remove("symbol-match", "symbol-origin");
    }
    if (!symbol) return 0;
    const escaped = typeof CSS !== "undefined" && CSS.escape ? CSS.escape(symbol.name) : symbol.name.replace(/["\\]/g, "\\$&");
    const matches = viewer.querySelectorAll(`.code-symbol[data-symbol="${escaped}"]`);
    for (const node of matches) {
      node.classList.add("symbol-match");
      const row = node.closest(".code-line");
      if (Number(row?.dataset.line) === symbol.line && Number(node.dataset.column) === symbol.column) {
        node.classList.add("symbol-origin");
      }
    }
    return matches.length;
  }

  function selectCodeSymbol(view, symbol) {
    const files = view?.files;
    const model = fileNavigationModel(files);
    if (!files?.file || !model || !codeSymbolValid(symbol?.name)) return;
    files.symbol = {
      name: symbol.name,
      line: symbol.line,
      column: symbol.column,
      qualifier: symbolQualifier(codeLineText(model, symbol.line), symbol.column),
    };
    files.symbolPanel = { status: "", results: null, truncated: false, operation: null };
    files.symbolCount = highlightSymbolOccurrences(files.symbol);
    renderSymbolPanel(view);
  }

  function clearCodeSymbol(view) {
    const files = view?.files;
    if (!files) return;
    files.symbol = null;
    files.symbolPanel = null;
    state.codeNavController?.abort();
    state.codeNavController = null;
    state.codeNavRequest += 1;
    highlightSymbolOccurrences(null);
    $("file-viewer").querySelector(".code-symbol-panel")?.remove();
  }

  function renderSymbolPanel(view) {
    const viewer = $("file-viewer");
    const files = view?.files;
    const existing = viewer.querySelector(".code-symbol-panel");
    if (!files?.symbol || !files.file || files.editing) { existing?.remove(); return; }
    const panel = document.createElement("div");
    panel.className = "code-symbol-panel";
    panel.setAttribute("role", "region");
    panel.setAttribute("aria-label", `Navigation for ${files.symbol.name}`);
    const head = document.createElement("div");
    head.className = "code-symbol-head";
    const name = document.createElement("code");
    name.className = "code-symbol-name";
    const named = codeSymbolValid(files.symbol.name);
    name.textContent = !named ? String(files.symbol.importSpec || "import")
      : files.symbol.qualifier ? `${files.symbol.qualifier}.${files.symbol.name}` : files.symbol.name;
    const count = document.createElement("span");
    count.className = "code-symbol-count";
    count.textContent = named ? `${files.symbolCount || 0} in this file` : "import";
    const definition = document.createElement("button");
    definition.type = "button"; definition.className = "subtle code-go-definition";
    definition.textContent = "Go to definition";
    definition.title = "Go to definition (F12, or Ctrl/⌘-click a name)";
    definition.addEventListener("click", () => { void goToDefinition(view); });
    const references = document.createElement("button");
    references.type = "button"; references.className = "subtle code-find-references";
    references.textContent = "Find references";
    references.title = "Find references (Shift+F12)";
    references.addEventListener("click", () => { void findReferences(view); });
    const close = document.createElement("button");
    close.type = "button"; close.className = "icon-button code-symbol-close";
    close.textContent = "×"; close.setAttribute("aria-label", "Close symbol navigation");
    close.addEventListener("click", () => clearCodeSymbol(view));
    head.append(name, count);
    if (named) head.append(definition, references);
    head.append(close);
    panel.append(head);
    const panelState = files.symbolPanel || {};
    if (panelState.status) {
      const status = document.createElement("p");
      status.className = `code-symbol-status${panelState.error ? " error" : ""}`;
      status.setAttribute("role", "status");
      status.textContent = panelState.status;
      panel.append(status);
    }
    if (Array.isArray(panelState.results) && panelState.results.length) {
      const list = document.createElement("ul");
      list.className = "code-symbol-results";
      for (const result of panelState.results) {
        const item = document.createElement("li");
        const button = document.createElement("button");
        button.type = "button";
        button.className = "code-symbol-result";
        const location = document.createElement("span");
        location.className = "code-symbol-location";
        location.textContent = `${result.path}:${result.line}`;
        const kind = document.createElement("span");
        kind.className = "code-symbol-kind";
        kind.textContent = result.kind;
        const preview = document.createElement("span");
        preview.className = "code-symbol-preview";
        preview.textContent = result.preview;
        button.title = `${result.path}:${result.line}`;
        button.append(location, kind, preview);
        button.addEventListener("click", () => navigateToCodeLocation(view, result));
        item.append(button);
        list.append(item);
      }
      panel.append(list);
    }
    if (existing) existing.replaceWith(panel); else viewer.append(panel);
  }

  function setSymbolPanel(view, change) {
    const files = view?.files;
    if (!files?.symbol) return;
    files.symbolPanel = { ...(files.symbolPanel || {}), ...change };
    if (state.projectView === view && state.viewMode === "files") renderSymbolPanel(view);
  }

  async function codeNavRequest(view, operation, params) {
    const paneId = state.selected;
    const endpoint = paneCodePath(paneId, operation, params);
    if (!view || !endpoint) return null;
    const machine = machineOf(state.sessions.get(paneId));
    if (!isMachineControllable(machine)) {
      setSymbolPanel(view, { status: "The selected machine is offline.", error: true, results: null });
      return null;
    }
    state.codeNavController?.abort();
    const controller = new AbortController();
    state.codeNavController = controller;
    const generation = ++state.codeNavRequest;
    setSymbolPanel(view, { status: "Searching the project…", error: false, results: null });
    try {
      const data = await request(endpoint, { signal: controller.signal });
      if (generation !== state.codeNavRequest || state.selected !== paneId || state.projectView !== view) return null;
      return codeNavResults(data);
    } catch (error) {
      if (error?.name === "AbortError") return null;
      if (generation === state.codeNavRequest && state.projectView === view) {
        setSymbolPanel(view, { status: projectErrorMessage(error, "Source navigation"), error: true, results: null });
      }
      return null;
    } finally {
      if (state.codeNavController === controller) state.codeNavController = null;
    }
  }

  /// Opens one result, or lists several so the reader can choose.
  function presentCodeResults(view, found, operation, emptyMessage) {
    if (!found) return false;
    const current = view.files.symbol;
    const results = found.results.filter((result) => operation !== "definitions" || !current
      || !(result.path === view.files.file?.path && result.line === current.line && result.column === current.column));
    if (!results.length) {
      setSymbolPanel(view, { status: emptyMessage, error: false, results: null, operation });
      return false;
    }
    if (results.length === 1 && operation !== "references") {
      setSymbolPanel(view, { status: "", results: null, operation });
      navigateToCodeLocation(view, results[0]);
      return true;
    }
    const label = operation === "references"
      ? `${results.length}${found.truncated ? "+" : ""} reference${results.length === 1 ? "" : "s"}`
      : `${results.length}${found.truncated ? "+" : ""} candidate definitions`;
    setSymbolPanel(view, {
      status: found.truncated ? `${label}. The search stopped at its safety limit.` : label,
      error: false, results, truncated: found.truncated, operation,
    });
    return true;
  }

  async function resolveImport(view, spec, symbol = null, options = {}) {
    const files = view?.files;
    if (!files?.file || !spec) return false;
    if (!files.symbol && !options.quiet) {
      // An import click has no selected name; its panel reports progress.
      files.symbol = { name: null, importSpec: spec, line: 0, column: 0, qualifier: null };
      files.symbolCount = 0;
      files.symbolPanel = { status: "", results: null };
    }
    const found = await codeNavRequest(view, "resolve", {
      path: files.file.path,
      spec,
      symbol: codeSymbolValid(symbol) ? symbol : null,
    });
    if (!found) return false;
    if (options.quiet && !found.results.length) return false;
    return presentCodeResults(view, found, "resolve", `Could not resolve ${spec} inside this project.`);
  }

  async function goToDefinition(view) {
    const files = view?.files;
    const symbol = files?.symbol;
    const model = fileNavigationModel(files);
    if (!symbol || !model || !codeSymbolValid(symbol.name)) return;
    const imports = model.imports;
    const importedQualifier = symbol.qualifier && imports.names.get(symbol.qualifier);
    if (importedQualifier && await resolveImport(view, importedQualifier.spec, symbol.name, { quiet: true })) return;
    const imported = !symbol.qualifier && imports.names.get(symbol.name);
    if (imported && await resolveImport(view, imported.spec, imported.symbol || symbol.name, { quiet: true })) return;
    const locals = localDefinitionLines(model.tokenLines, model.language, symbol.name);
    const selfDeclared = locals.some((definition) => definition.line === symbol.line && definition.column === symbol.column);
    const localQualifier = !symbol.qualifier || ["this", "self", "Self", "super", "cls"].includes(symbol.qualifier);
    if (!selfDeclared && localQualifier) {
      const local = chooseLocalDefinition(locals, symbol.line);
      if (local) {
        navigateToCodeLocation(view, { path: files.file.path, line: local.line, column: local.column });
        return;
      }
    }
    if (selfDeclared) {
      // Already on the declaration: an editor shows its usages instead.
      await findReferences(view);
      return;
    }
    const found = await codeNavRequest(view, "definitions", { symbol: symbol.name, path: files.file.path });
    presentCodeResults(view, found, "definitions", `No definition of ${symbol.name} found in this project.`);
  }

  async function findReferences(view) {
    const files = view?.files;
    const symbol = files?.symbol;
    if (!symbol || !files.file || !codeSymbolValid(symbol.name)) return;
    const found = await codeNavRequest(view, "references", { symbol: symbol.name, path: files.file.path });
    presentCodeResults(view, found, "references", `No references to ${symbol.name} found.`);
  }

  function codeViewerPosition(files) {
    const viewer = $("file-viewer");
    return files?.file ? { path: files.file.path, top: viewer.scrollTop, left: viewer.scrollLeft } : null;
  }

  function navigateToCodeLocation(view, location) {
    const files = view?.files;
    if (!files?.file || !location?.path) return;
    if (!confirmDiscardFileEdit(files)) return;
    const current = codeViewerPosition(files);
    if (current) files.history = pushCodeHistory(files.history, current);
    if (location.path === files.file.path) {
      updateCodeHistoryButtons(files);
      revealCodeLine(view, location.line, location.column);
      return;
    }
    void loadProjectFile(location.path, { reveal: { line: location.line, column: location.column } });
  }

  function stepCodeNavigation(view, direction) {
    const files = view?.files;
    if (!files || !confirmDiscardFileEdit(files)) return;
    const { history, target } = stepCodeHistory(files.history, direction, codeViewerPosition(files));
    if (!target) return;
    files.history = history;
    if (target.path === files.file?.path) {
      const viewer = $("file-viewer");
      viewer.scrollTop = target.top || 0;
      viewer.scrollLeft = target.left || 0;
      updateCodeHistoryButtons(files);
      return;
    }
    void loadProjectFile(target.path, { scroll: { top: target.top || 0, left: target.left || 0 } });
  }

  function updateCodeHistoryButtons(files) {
    const viewer = $("file-viewer");
    const back = viewer.querySelector(".code-history-back");
    const forward = viewer.querySelector(".code-history-forward");
    if (back) back.disabled = !files?.history?.back?.length;
    if (forward) forward.disabled = !files?.history?.forward?.length;
  }

  function codeHistoryControls(view) {
    const files = view.files;
    const group = document.createElement("div");
    group.className = "code-history-controls";
    group.setAttribute("role", "group");
    group.setAttribute("aria-label", "File navigation history");
    for (const [direction, label, text] of [["back", "Back (Alt+←)", "←"], ["forward", "Forward (Alt+→)", "→"]]) {
      const button = document.createElement("button");
      button.type = "button";
      button.className = `subtle code-history-${direction}`;
      button.textContent = text;
      button.title = label;
      button.setAttribute("aria-label", label);
      button.disabled = !files.history?.[direction]?.length;
      button.addEventListener("click", () => stepCodeNavigation(view, direction));
      group.append(button);
    }
    return group;
  }

  /// Scrolls a line into the reader's upper third, flashes it, and selects
  /// the name at the target column so its occurrences light up.
  function revealCodeLine(view, line, column = null) {
    const viewer = $("file-viewer");
    const row = viewer.querySelector(`.code-line[data-line="${Number(line) || 1}"]`);
    if (!row) return;
    const delta = row.getBoundingClientRect().top - viewer.getBoundingClientRect().top;
    viewer.scrollTop = Math.max(0, viewer.scrollTop + delta - viewer.clientHeight / 3);
    const target = column ? row.querySelector(`.code-symbol[data-column="${Number(column)}"]`) : null;
    if (target && !state.fileReaderPreferences.wrap) {
      const offset = target.getBoundingClientRect().left - viewer.getBoundingClientRect().left;
      if (offset < 0 || offset > viewer.clientWidth - 40) viewer.scrollLeft = Math.max(0, viewer.scrollLeft + offset - 80);
    }
    row.classList.remove("code-line-flash");
    void row.offsetWidth;
    row.classList.add("code-line-flash");
    setTimeout(() => row.classList.remove("code-line-flash"), 1800);
    if (target) selectCodeSymbol(view, { name: target.dataset.symbol, line: Number(line), column: Number(column) });
  }

  function handleSourceClick(view, event) {
    const files = view?.files;
    if (!files?.file || files.editing) return;
    const element = event.target instanceof Element ? event.target : null;
    const target = element?.closest("[data-symbol], [data-import-spec]");
    if (!target) return;
    if (String(window.getSelection?.() || "").trim()) return;
    const row = target.closest(".code-line");
    const line = Number(row?.dataset.line) || 1;
    if (target.dataset.importSpec) {
      event.preventDefault();
      clearCodeSymbol(view);
      void resolveImport(view, target.dataset.importSpec, target.dataset.importSymbol || null);
      return;
    }
    selectCodeSymbol(view, { name: target.dataset.symbol, line, column: Number(target.dataset.column) || 1 });
    if (event.metaKey || event.ctrlKey) {
      event.preventDefault();
      void goToDefinition(view);
    }
  }

  function handleFileViewerKeydown(event) {
    const view = state.projectView;
    if (!view || state.viewMode !== "files" || !view.files.file || view.files.editing) return;
    if (event.target instanceof Element && event.target.closest("textarea, input, select")) return;
    if (event.altKey && (event.key === "ArrowLeft" || event.key === "ArrowRight")) {
      event.preventDefault();
      stepCodeNavigation(view, event.key === "ArrowLeft" ? "back" : "forward");
    } else if (event.key === "F12" && view.files.symbol) {
      event.preventDefault();
      if (event.shiftKey) void findReferences(view); else void goToDefinition(view);
    } else if (event.key === "Escape" && view.files.symbol) {
      event.preventDefault();
      clearCodeSymbol(view);
    }
  }

  function setNavigationModifier(active) {
    $("file-viewer").classList.toggle("nav-modifier", Boolean(active));
  }

  function updateFileSelection(view, line, extend) {
    const files = view?.files;
    if (!files?.file || files.editing) return;
    files.selection = nextFileLineSelection(files.selection, line, extend);
    const viewer = $("file-viewer");
    for (const row of viewer.querySelectorAll(".code-line")) {
      const button = row.querySelector("button.code-line-number");
      const number = Number(button?.textContent);
      const selected = Number.isInteger(number)
        && number >= files.selection.start && number <= files.selection.end;
      row.classList.toggle("selected", selected);
      button?.setAttribute("aria-pressed", String(selected));
    }
    const reference = viewer.querySelector(".file-reference");
    if (reference) {
      reference.disabled = false;
      reference.textContent = files.selection.start === files.selection.end
        ? `Reference line ${files.selection.start}`
        : `Reference lines ${files.selection.start}–${files.selection.end}`;
    }
  }

  function clearFileSelection(files) {
    files.selection = null;
    for (const row of $("file-viewer").querySelectorAll(".code-line.selected")) {
      row.classList.remove("selected");
      row.querySelector("button.code-line-number")?.setAttribute("aria-pressed", "false");
    }
    const reference = $("file-viewer").querySelector(".file-reference");
    if (reference) { reference.disabled = true; reference.textContent = "Reference selection"; }
  }

  function referenceSelectedFile(view) {
    const files = view?.files;
    const file = files?.file;
    if (!file || !files.selection || typeof file.content !== "string") return;
    const reference = fileReferenceBlock(file.path, file.language, file.content, files.selection);
    if (!reference) return;
    const input = $("message");
    const insertion = insertComposerReference(input.value, input.selectionEnd, reference);
    if (!messageFitsByteLimit(insertion.value)) {
      toast("The selected code would exceed the 64 KiB message limit");
      return;
    }
    const viewer = $("file-viewer");
    const position = { top: viewer.scrollTop, left: viewer.scrollLeft };
    replaceComposerValue(insertion.value);
    input.focus({ preventScroll: true });
    input.setSelectionRange(insertion.cursor, insertion.cursor);
    // Focusing the persistent composer must not knock the source reader away
    // from the chunk they just referenced.
    viewer.scrollTop = position.top;
    viewer.scrollLeft = position.left;
    requestAnimationFrame(() => {
      if (state.selected === view.paneId && state.viewMode === "files") {
        viewer.scrollTop = position.top;
        viewer.scrollLeft = position.left;
      }
    });
  }

  function updateFileEditControls(files) {
    const viewer = $("file-viewer");
    const save = viewer.querySelector(".file-save");
    const status = viewer.querySelector(".file-edit-status");
    if (!save || !status || !files.file) return;
    const dirty = files.editDraft !== files.file.content;
    const oversized = utf8ByteLength(files.editDraft) > MAX_PROJECT_SOURCE_CHARS;
    save.disabled = files.saving || files.reloading || files.conflict || !dirty || oversized;
    save.textContent = files.saving ? "Saving…" : "Save";
    status.className = `file-edit-status${files.conflict || files.saveError || oversized ? " error" : dirty ? " dirty" : ""}`;
    status.textContent = files.conflict
      ? (files.saveError
        ? `Conflict: your draft is preserved. Reload failed: ${files.saveError}`
        : "Conflict: this file changed on disk. Your draft is preserved. Reload latest before saving again.")
      : files.saveError || (oversized ? "Draft exceeds the 256 KiB editing limit." : dirty ? "Unsaved changes" : "No changes");
  }

  function rememberProjectScroll() {
    const view = state.projectView;
    if (!view || view.paneId !== state.selected) return;
    if (state.viewMode === "files" && !filesPanel.hidden) {
      view.files.listScrolls.set(view.files.path, $("files-list").scrollTop);
      if (view.files.file?.path) {
        view.files.viewerScrolls.set(view.files.file.path, {
          top: $("file-viewer").scrollTop,
          left: $("file-viewer").scrollLeft,
        });
      }
    }
    if (state.viewMode === "git" && !gitPanel.hidden) {
      view.git.changesScroll = $("git-changes").scrollTop;
      if (view.git.diff?.path) {
        view.git.diffScrolls.set(view.git.diff.path, {
          top: $("git-diff").scrollTop,
          left: $("git-diff").scrollLeft,
        });
      }
    }
  }

  function restoreProjectScroll(mode, paneId, path) {
    requestAnimationFrame(() => {
      if (state.selected !== paneId || state.viewMode !== mode) return;
      const view = state.projectView;
      if (!view || view.paneId !== paneId) return;
      if (mode === "files") {
        $("files-list").scrollTop = view.files.listScrolls.get(view.files.path) || 0;
        const position = view.files.viewerScrolls.get(path) || {};
        $("file-viewer").scrollTop = position.top || 0;
        $("file-viewer").scrollLeft = position.left || 0;
      } else {
        $("git-changes").scrollTop = view.git.changesScroll || 0;
        const position = view.git.diffScrolls.get(path) || {};
        $("git-diff").scrollTop = position.top || 0;
        $("git-diff").scrollLeft = position.left || 0;
      }
    });
  }

  function renderBreadcrumbs(files) {
    const buttons = files.breadcrumbs.map((crumb, index) => {
      const button = document.createElement("button");
      button.type = "button";
      button.textContent = crumb.name;
      button.title = crumb.path || "Project root";
      button.disabled = files.loading || index === files.breadcrumbs.length - 1;
      button.addEventListener("click", () => {
        if (!confirmDiscardFileEdit(files)) return;
        rememberProjectScroll();
        void loadFilesDirectory(crumb.path, files.breadcrumbs.slice(0, index + 1));
      });
      return button;
    });
    $("files-breadcrumbs").replaceChildren(...buttons);
  }

  function renderFiles() {
    const view = selectedProjectView();
    if (!view) return;
    const files = view.files;
    renderBreadcrumbs(files);
    const list = $("files-list");
    if (files.loading && !files.listing) list.replaceChildren(projectStateNode("Loading project files…"));
    else if (files.error && !files.listing) list.replaceChildren(projectStateNode(files.error, true));
    else {
      const entries = files.listing?.entries || [];
      list.replaceChildren(...(entries.length ? entries.map((entry) => {
        const button = document.createElement("button");
        button.type = "button";
        button.className = `project-entry${files.file?.path === entry.path ? " selected" : ""}`;
        button.title = entry.path;
        const icon = document.createElement("span");
        icon.className = "project-entry-icon";
        icon.textContent = entry.kind === "directory" ? "▸" : "·";
        const name = document.createElement("span");
        name.className = "project-entry-name";
        name.textContent = entry.name;
        const meta = document.createElement("span");
        meta.className = "project-entry-meta";
        meta.textContent = entry.kind === "directory" ? "folder" : formatBytes(entry.size);
        button.append(icon, name, meta);
        button.addEventListener("click", () => {
          if (!confirmDiscardFileEdit(files)) return;
          rememberProjectScroll();
          if (entry.kind === "directory") {
            const crumbs = [...files.breadcrumbs, { name: entry.name, path: entry.path }];
            void loadFilesDirectory(entry.path, crumbs);
          } else {
            const current = codeViewerPosition(files);
            if (current && current.path !== entry.path) files.history = pushCodeHistory(files.history, current);
            void loadProjectFile(entry.path);
          }
        });
        return button;
      }) : [projectStateNode(files.error || "This directory is empty.", Boolean(files.error))]));
    }

    const viewer = $("file-viewer");
    applyFileReaderPreferences(viewer);
    $("files-panel").classList.toggle("has-file", Boolean(files.file));
    if (files.loading && files.file === null && files.listing) {
      viewer.replaceChildren(projectStateNode("Loading file…"));
    } else if (files.error && files.file === null && files.listing) {
      viewer.replaceChildren(projectStateNode(files.error, true));
    } else if (!files.file) {
      viewer.replaceChildren(projectStateNode("Choose a file to inspect its source."));
    } else {
      const head = document.createElement("header");
      head.className = "code-viewer-head";
      const back = document.createElement("button");
      back.type = "button";
      back.className = "mobile-only subtle project-viewer-back";
      back.textContent = "← Files";
      back.addEventListener("click", () => {
        if (!confirmDiscardFileEdit(files)) return;
        rememberProjectScroll();
        files.file = null;
        renderFiles();
        restoreProjectScroll("files", view.paneId, "");
      });
      const path = document.createElement("span"); path.className = "code-viewer-path"; path.textContent = files.file.path;
      const meta = document.createElement("span");
      meta.textContent = [files.file.language, formatBytes(files.file.size), files.file.truncated ? "truncated" : ""].filter(Boolean).join(" · ");
      const actions = document.createElement("div"); actions.className = "file-viewer-actions";
      if (typeof files.file.content === "string" && !files.editing) actions.append(codeHistoryControls(view));
      if (typeof files.file.content === "string") actions.append(fileReaderControls());
      if (typeof files.file.content === "string" && !files.editing) {
        const reference = document.createElement("button");
        reference.type = "button"; reference.className = "subtle file-reference";
        reference.disabled = !files.selection;
        reference.textContent = files.selection
          ? (files.selection.start === files.selection.end
            ? `Reference line ${files.selection.start}`
            : `Reference lines ${files.selection.start}–${files.selection.end}`)
          : "Reference selection";
        reference.addEventListener("click", () => referenceSelectedFile(view));
        actions.append(reference);
        const clear = document.createElement("button");
        clear.type = "button"; clear.className = "subtle file-selection-clear"; clear.textContent = "Clear";
        clear.hidden = !files.selection;
        clear.addEventListener("click", () => { clearFileSelection(files); clear.hidden = true; });
        actions.append(clear);
      }
      if (fileCanEdit(files.file)) {
        if (files.editing) {
          if (files.conflict) {
            const reload = document.createElement("button");
            reload.type = "button"; reload.className = "subtle file-reload";
            reload.textContent = files.reloading ? "Reloading…" : "Reload latest";
            reload.disabled = files.saving || files.reloading;
            reload.addEventListener("click", () => { void reloadConflictedFile(); });
            actions.append(reload);
          }
          const cancel = document.createElement("button");
          cancel.type = "button"; cancel.className = "subtle file-cancel"; cancel.textContent = "Cancel";
          cancel.disabled = files.saving || files.reloading;
          cancel.addEventListener("click", () => {
            if (files.conflict) { void reloadConflictedFile(); return; }
            if (!confirmDiscardFileEdit(files)) return;
            discardFileEdit(files);
            renderFiles();
          });
          const save = document.createElement("button");
          save.type = "button"; save.className = "primary file-save"; save.textContent = files.saving ? "Saving…" : "Save";
          save.addEventListener("click", () => { void saveProjectFile(); });
          actions.append(cancel, save);
        } else {
          const edit = document.createElement("button");
          edit.type = "button"; edit.className = "subtle file-edit"; edit.textContent = "Edit";
          edit.addEventListener("click", () => {
            files.editing = true; files.editDraft = files.file.content;
            files.saveError = null; files.conflict = false; files.selection = null;
            files.symbol = null; files.symbolPanel = null;
            renderFiles();
            $("file-viewer").querySelector(".file-editor")?.focus({ preventScroll: true });
          });
          actions.append(edit);
        }
      }
      head.append(back, path, meta, actions);
      viewer.replaceChildren(head);
      if (typeof files.file.content !== "string") {
        viewer.append(projectStateNode("This binary or unsupported file cannot be previewed."));
      } else if (files.editing) {
        const status = document.createElement("p"); status.className = "file-edit-status"; status.setAttribute("role", "status");
        const editor = document.createElement("textarea");
        editor.className = "file-editor"; editor.value = files.editDraft;
        editor.setAttribute("aria-label", `Edit ${files.file.path}`);
        editor.spellcheck = false;
        editor.wrap = state.fileReaderPreferences.wrap ? "soft" : "off";
        editor.disabled = files.reloading;
        editor.addEventListener("input", () => {
          files.editDraft = editor.value;
          if (!files.conflict) files.saveError = null;
          updateFileEditControls(files);
        });
        viewer.append(status, editor);
        updateFileEditControls(files);
      } else {
        const model = fileNavigationModel(files);
        const code = appendSource(
          viewer,
          files.file.content,
          model?.language || files.file.language,
          false,
          (line, extend) => updateFileSelection(view, line, extend),
          files.selection,
          model,
        );
        code.addEventListener("click", (event) => handleSourceClick(view, event));
        viewer.classList.toggle("code-navigable", Boolean(model?.interactive));
        if (files.file.truncated) viewer.append(projectStateNode("Preview truncated at the safe display limit."));
        if (files.symbol) {
          files.symbolCount = highlightSymbolOccurrences(files.symbol);
          renderSymbolPanel(view);
        }
      }
    }
    restoreProjectScroll("files", view.paneId, files.file?.path || "");
    const reveal = files.pendingReveal;
    files.pendingReveal = null;
    if (reveal && reveal.path === files.file?.path && !files.editing) {
      // Runs after restoreProjectScroll's frame callback, so the target
      // line wins over a remembered position.
      requestAnimationFrame(() => {
        if (state.projectView === view && state.viewMode === "files" && files.file?.path === reveal.path) {
          revealCodeLine(view, reveal.line, reveal.column);
        }
      });
    }
  }

  async function loadFilesDirectory(path = "", breadcrumbs = null) {
    const view = selectedProjectView();
    const paneId = state.selected;
    const endpoint = paneFilesPath(paneId, path);
    if (!view || !endpoint || state.viewMode !== "files" || filesPanel.hidden) return;
    const machine = machineOf(state.sessions.get(paneId));
    if (!isMachineControllable(machine)) {
      view.files.error = "The selected machine is offline. Files will be available when it reconnects.";
      renderFiles(); return;
    }
    state.filesController?.abort();
    const controller = new AbortController();
    state.filesController = controller;
    const generation = ++state.filesRequest;
    view.files.loading = true; view.files.error = null; view.files.file = null; view.files.listing = null;
    if (breadcrumbs) view.files.breadcrumbs = breadcrumbs;
    renderFiles();
    try {
      const data = await request(endpoint, { signal: controller.signal });
      if (generation !== state.filesRequest || state.selected !== paneId
        || state.viewMode !== "files" || filesPanel.hidden) return;
      const kind = String(data?.kind || data?.type || "").toLowerCase();
      if (kind !== "directory") throw new Error("The owner returned an invalid directory response");
      const responsePath = projectRelativePath(data.path);
      if (responsePath === null || responsePath !== projectRelativePath(path)) throw new Error("The owner returned a mismatched directory path");
      const entries = (Array.isArray(data.entries) ? data.entries : []).slice(0, MAX_PROJECT_ENTRIES)
        .map((entry) => {
          const entryPath = projectRelativePath(entry?.path);
          const entryKind = projectEntryKind(entry);
          const name = typeof entry?.name === "string" ? entry.name : "";
          if (!entryPath || !entryKind || !name || name.length > 512 || /[\u0000-\u001f\u007f/]/.test(name)) return null;
          const expectedPath = responsePath ? `${responsePath}/${name}` : name;
          if (entryPath !== expectedPath) return null;
          return { name, path: entryPath, kind: entryKind, size: Number(entry.size) };
        }).filter(Boolean)
        .sort((left, right) => (left.kind === right.kind ? left.name.localeCompare(right.name) : left.kind === "directory" ? -1 : 1));
      view.files.path = responsePath;
      view.files.listing = { entries, truncated: Boolean(data.truncated) || (data.entries?.length || 0) > MAX_PROJECT_ENTRIES };
      if (view.files.listing.truncated) view.files.error = "Some entries are omitted from this large directory.";
    } catch (error) {
      if (error?.name === "AbortError") return;
      view.files.error = projectErrorMessage(error, "Directory");
      view.files.listing = null;
    } finally {
      if (generation === state.filesRequest && state.selected === paneId) {
        view.files.loading = false;
        if (state.viewMode === "files" && !filesPanel.hidden) renderFiles();
      }
    }
  }

  async function loadProjectFile(path, options = {}) {
    const view = selectedProjectView();
    const paneId = state.selected;
    const endpoint = paneFilesPath(paneId, path);
    if (!view || !endpoint || state.viewMode !== "files" || filesPanel.hidden) return;
    state.filesController?.abort();
    const controller = new AbortController();
    state.filesController = controller;
    const generation = ++state.filesRequest;
    view.files.loading = true; view.files.error = null; view.files.file = null;
    view.files.editing = false; view.files.editDraft = ""; view.files.saveError = null;
    view.files.saving = false; view.files.reloading = false;
    view.files.conflict = false; view.files.selection = null;
    view.files.symbol = null; view.files.symbolPanel = null;
    view.files.navModel = null; view.files.pendingReveal = null;
    state.codeNavController?.abort();
    state.codeNavController = null;
    state.codeNavRequest += 1;
    renderFiles();
    try {
      const data = await request(endpoint, { signal: controller.signal });
      if (generation !== state.filesRequest || state.selected !== paneId
        || state.viewMode !== "files" || filesPanel.hidden) return;
      const responsePath = projectRelativePath(data?.path);
      if (String(data?.kind || data?.type || "").toLowerCase() !== "file" || responsePath !== path) {
        throw new Error("The owner returned an invalid file response");
      }
      view.files.file = projectFilePreview(data, responsePath);
      if (options.scroll) view.files.viewerScrolls.set(responsePath, options.scroll);
      if (options.reveal) {
        view.files.pendingReveal = { path: responsePath, line: options.reveal.line, column: options.reveal.column };
      }
    } catch (error) {
      if (error?.name === "AbortError") return;
      view.files.error = projectErrorMessage(error, "File");
    } finally {
      if (generation === state.filesRequest && state.selected === paneId) {
        view.files.loading = false;
        if (state.viewMode === "files" && !filesPanel.hidden) renderFiles();
      }
    }
  }

  async function saveProjectFile() {
    const view = selectedProjectView();
    const files = view?.files;
    const file = files?.file;
    const paneId = state.selected;
    const endpoint = paneFilesPath(paneId, file?.path);
    if (!view || !files.editing || !fileCanEdit(file) || !endpoint || files.saving) return;
    const content = files.editDraft;
    if (utf8ByteLength(content) > MAX_PROJECT_SOURCE_CHARS) {
      files.saveError = "Draft exceeds the 256 KiB editing limit."; updateFileEditControls(files); return;
    }
    state.fileSaveController?.abort();
    const controller = new AbortController(); state.fileSaveController = controller;
    const generation = ++state.fileSaveRequest;
    const snapshot = { paneId, path: file.path, expectedHash: file.contentHash, content };
    files.saving = true; files.saveError = null; files.conflict = false;
    updateFileEditControls(files);
    try {
      const data = await request(endpoint, {
        method: "PUT", signal: controller.signal,
        body: JSON.stringify({ path: snapshot.path, content: snapshot.content, expected_hash: snapshot.expectedHash }),
      });
      if (generation !== state.fileSaveRequest || state.selected !== snapshot.paneId
        || state.projectView !== view || files.file?.path !== snapshot.path) return;
      const responsePath = projectRelativePath(data?.path);
      if (String(data?.kind || data?.type || "").toLowerCase() !== "file"
        || responsePath !== snapshot.path || !validContentHash(data?.content_hash)) {
        throw new Error("The owner returned an invalid saved-file response");
      }
      const saved = projectFilePreview(data, responsePath);
      if (!fileCanEdit(saved)) throw new Error("The owner did not return the saved UTF-8 file");
      const reconciled = reconcileSavedFileDraft(snapshot.content, files.editDraft, saved);
      files.file = reconciled.file;
      files.editDraft = reconciled.editDraft;
      files.editing = reconciled.editing;
      files.selection = null; files.saveError = null; files.conflict = false;
      toast(reconciled.editing ? `Saved ${saved.path}; newer edits remain unsaved` : `Saved ${saved.path}`);
    } catch (error) {
      if (error?.name === "AbortError") return;
      if (generation !== state.fileSaveRequest || state.selected !== snapshot.paneId
        || state.projectView !== view || files.file?.path !== snapshot.path) return;
      if (error?.status === 409) {
        files.conflict = true;
        files.saveError = null;
      } else {
        files.saveError = error?.message || "The file could not be saved.";
      }
    } finally {
      if (generation === state.fileSaveRequest && state.selected === snapshot.paneId
        && state.projectView === view) {
        files.saving = false;
        if (state.viewMode === "files" && !filesPanel.hidden) renderFiles();
      }
    }
  }

  async function reloadConflictedFile() {
    const view = selectedProjectView();
    const files = view?.files;
    const file = files?.file;
    const paneId = state.selected;
    const endpoint = paneFilesPath(paneId, file?.path);
    if (!view || !files?.editing || !files.conflict || !file || !endpoint || files.reloading) return;
    if (!window.confirm("Discard this draft and reload the latest file from disk?")) return;
    state.fileSaveController?.abort();
    const controller = new AbortController(); state.fileSaveController = controller;
    const generation = ++state.fileSaveRequest;
    const snapshot = { paneId, path: file.path };
    files.reloading = true; files.saveError = null;
    renderFiles();
    try {
      const data = await request(endpoint, { signal: controller.signal });
      if (generation !== state.fileSaveRequest || state.selected !== snapshot.paneId
        || state.projectView !== view || files.file?.path !== snapshot.path) return;
      const responsePath = projectRelativePath(data?.path);
      if (String(data?.kind || data?.type || "").toLowerCase() !== "file" || responsePath !== snapshot.path) {
        throw new Error("The owner returned an invalid reloaded-file response");
      }
      files.file = projectFilePreview(data, responsePath);
      files.editing = false; files.editDraft = ""; files.saving = false;
      files.saveError = null; files.conflict = false; files.selection = null;
      toast(`Reloaded ${responsePath}`);
    } catch (error) {
      if (error?.name === "AbortError") return;
      if (generation !== state.fileSaveRequest || state.selected !== snapshot.paneId
        || state.projectView !== view || files.file?.path !== snapshot.path) return;
      files.saveError = error?.message || "The latest file could not be reloaded.";
      files.conflict = true;
    } finally {
      if (generation === state.fileSaveRequest && state.selected === snapshot.paneId
        && state.projectView === view) {
        files.reloading = false;
        if (state.viewMode === "files" && !filesPanel.hidden) renderFiles();
      }
    }
  }

  function renderGit() {
    const view = selectedProjectView();
    if (!view) return;
    const git = view.git;
    const summary = $("git-summary");
    const changes = $("git-changes");
    const diff = $("git-diff");
    if (git.loading && !git.summary) {
      summary.replaceChildren(projectStateNode("Loading Git status…"));
      changes.replaceChildren(); diff.replaceChildren(); return;
    }
    if (git.error && !git.summary) {
      summary.replaceChildren(projectStateNode(git.error, true));
      changes.replaceChildren(); diff.replaceChildren(); return;
    }
    if (!git.summary?.available) {
      summary.replaceChildren(projectStateNode("This project is not a Git repository."));
      changes.replaceChildren(); diff.replaceChildren(); return;
    }
    const branch = document.createElement("span");
    branch.className = "git-branch";
    branch.textContent = git.summary.detached ? `Detached at ${git.summary.branch || "HEAD"}` : (git.summary.branch || "Unknown branch");
    const status = document.createElement("span");
    status.className = `git-chip${git.summary.clean ? " clean" : ""}`;
    status.textContent = git.summary.clean ? "Clean" : `${git.summary.changes.length} changed`;
    summary.replaceChildren(branch, status);
    if (git.summary.truncated) {
      const warning = document.createElement("span"); warning.className = "git-chip"; warning.textContent = "List truncated"; summary.append(warning);
    }
    changes.replaceChildren(...(git.summary.changes.length ? git.summary.changes.map((change) => {
      const button = document.createElement("button");
      button.type = "button";
      button.className = `git-change${git.diff?.path === change.path ? " selected" : ""}`;
      button.title = change.oldPath ? `${change.oldPath} → ${change.path}` : change.path;
      const badge = document.createElement("span"); badge.className = "git-status"; badge.textContent = change.status;
      const path = document.createElement("span"); path.className = "git-change-path";
      path.textContent = change.oldPath ? `${change.oldPath} → ${change.path}` : change.path;
      button.append(badge, path);
      button.addEventListener("click", () => { rememberProjectScroll(); void loadGitDiff(change.path); });
      return button;
    }) : [projectStateNode(git.summary.clean ? "Working tree clean." : "No changed paths were returned.")]));
    $("git-panel").classList.toggle("has-diff", Boolean(git.diff));
    if (git.diffLoading) diff.replaceChildren(projectStateNode("Loading diff…"));
    else if (git.error && !git.diff) diff.replaceChildren(projectStateNode(git.error, true));
    else if (!git.diff) diff.replaceChildren(projectStateNode(git.summary.clean ? "No changes to inspect." : "Choose a changed file to inspect its diff."));
    else {
      const head = document.createElement("header"); head.className = "code-viewer-head";
      const back = document.createElement("button"); back.type = "button"; back.className = "mobile-only subtle project-viewer-back"; back.textContent = "← Changes";
      back.addEventListener("click", () => { rememberProjectScroll(); git.diff = null; renderGit(); restoreProjectScroll("git", view.paneId, ""); });
      const path = document.createElement("span"); path.textContent = git.diff.path;
      const meta = document.createElement("span"); meta.textContent = git.diff.truncated ? "diff · truncated" : "diff";
      head.append(back, path, meta);
      diff.replaceChildren(head);
      appendSource(diff, git.diff.diff, "diff", true);
      if (git.diff.truncated) diff.append(projectStateNode("Diff preview truncated at the safe display limit."));
    }
    restoreProjectScroll("git", view.paneId, git.diff?.path || "");
  }

  async function loadGitSummary() {
    const view = selectedProjectView();
    const paneId = state.selected;
    const endpoint = paneGitPath(paneId);
    if (!view || !endpoint) return;
    const machine = machineOf(state.sessions.get(paneId));
    if (!isMachineControllable(machine)) {
      view.git.error = "The selected machine is offline. Git status will be available when it reconnects.";
      renderGit(); return;
    }
    state.gitController?.abort();
    const controller = new AbortController(); state.gitController = controller;
    const generation = ++state.gitRequest;
    view.git.loading = true; view.git.error = null;
    if (state.viewMode === "git" && !gitPanel.hidden) renderGit();
    renderAgentBranch();
    try {
      const data = await request(endpoint, { signal: controller.signal });
      if (generation !== state.gitRequest || state.selected !== paneId
        || state.projectView !== view || view.paneId !== paneId) return;
      const rawChanges = Array.isArray(data?.changes) ? data.changes : [];
      const changes = rawChanges.slice(0, MAX_PROJECT_ENTRIES).map((change) => {
        const path = projectRelativePath(change?.path);
        const oldPath = change?.old_path == null ? null : projectRelativePath(change.old_path);
        const status = typeof change?.status === "string" ? change.status.trim().slice(0, 8) : "?";
        return path && status && (change?.old_path == null || oldPath) ? { path, oldPath, status } : null;
      }).filter(Boolean);
      view.git.summary = {
        available: data?.available === true,
        branch: typeof data?.branch === "string" ? data.branch.slice(0, 512) : null,
        detached: Boolean(data?.detached), clean: Boolean(data?.clean), changes,
        truncated: Boolean(data?.truncated) || rawChanges.length > MAX_PROJECT_ENTRIES,
      };
      view.git.diff = null;
    } catch (error) {
      if (error?.name === "AbortError") return;
      view.git.error = projectErrorMessage(error, "Git status");
      view.git.summary = null;
    } finally {
      if (generation === state.gitRequest && state.selected === paneId) {
        view.git.loading = false;
        renderAgentBranch();
        if (state.viewMode === "git" && !gitPanel.hidden) renderGit();
      }
    }
  }

  async function loadGitDiff(path) {
    const view = selectedProjectView();
    const paneId = state.selected;
    if (!view?.git.summary?.changes.some((change) => change.path === path)) return;
    const endpoint = paneGitPath(paneId, path);
    if (!view || !endpoint || state.viewMode !== "git" || gitPanel.hidden) return;
    state.gitController?.abort();
    const controller = new AbortController(); state.gitController = controller;
    const generation = ++state.gitRequest;
    view.git.diffLoading = true; view.git.error = null; view.git.diff = null;
    renderGit();
    try {
      const data = await request(endpoint, { signal: controller.signal });
      if (generation !== state.gitRequest || state.selected !== paneId || state.viewMode !== "git" || gitPanel.hidden) return;
      const responsePath = projectRelativePath(data?.path);
      if (responsePath !== path || typeof data?.diff !== "string") throw new Error("The owner returned an invalid Git diff");
      view.git.diff = {
        path: responsePath, diff: data.diff.slice(0, MAX_PROJECT_SOURCE_CHARS),
        truncated: Boolean(data.truncated) || data.diff.length > MAX_PROJECT_SOURCE_CHARS
          || data.diff.split("\n", MAX_PROJECT_SOURCE_LINES + 1).length > MAX_PROJECT_SOURCE_LINES,
      };
    } catch (error) {
      if (error?.name === "AbortError") return;
      view.git.error = projectErrorMessage(error, "Git diff");
    } finally {
      if (generation === state.gitRequest && state.selected === paneId) {
        view.git.diffLoading = false;
        if (state.viewMode === "git" && !gitPanel.hidden) renderGit();
      }
    }
  }

  function setViewMode(mode) {
    const next = ["conversation", "raw", "files", "git"].includes(mode) ? mode : "conversation";
    if (next === state.viewMode) return;
    if (state.viewMode === "files" && !confirmDiscardFileEdit()) return false;
    rememberProjectScroll();
    if (state.viewMode === "files") {
      state.filesController?.abort(); state.filesController = null; state.filesRequest += 1;
      if (state.projectView?.paneId === state.selected) state.projectView.files.loading = false;
    }
    if (state.viewMode === "git") {
      state.gitController?.abort(); state.gitController = null; state.gitRequest += 1;
      if (state.projectView?.paneId === state.selected) {
        state.projectView.git.loading = false;
        state.projectView.git.diffLoading = false;
      }
    }
    state.viewMode = next;
    if (next === "conversation") startTranscriptPolling();
    else stopTranscriptPolling();
    renderViewMode();
    return true;
  }

  function renderConversationFilters() {
    const preferences = conversationVisibilityPreferences(state.conversationVisibility);
    const hiddenCount = Number(!preferences.human) + Number(!preferences.internal);
    const open = $("conversation-filters-open");
    const indicator = $("conversation-filters-indicator");
    $("conversation-show-human").checked = preferences.human;
    $("conversation-show-internal").checked = preferences.internal;
    $("conversation-filters-reset").disabled = hiddenCount === 0;
    open.classList.toggle("active", hiddenCount > 0);
    open.setAttribute("aria-label", hiddenCount
      ? `Conversation visibility: ${hiddenCount} message ${hiddenCount === 1 ? "type" : "types"} hidden`
      : "Conversation visibility: showing all message types");
    indicator.textContent = hiddenCount ? `${hiddenCount} off` : "All";
  }

  function setConversationVisibility(next) {
    state.conversationVisibility = conversationVisibilityPreferences(next);
    saveConversationVisibilityPreferences(
      (value) => writeLocalStorage(CONVERSATION_VISIBILITY_STORAGE_KEY, value),
      state.conversationVisibility,
    );
    renderConversationFilters();
    drawConversation(true);
  }

  function renderViewMode() {
    const raw = state.viewMode === "raw";
    const files = state.viewMode === "files";
    const git = state.viewMode === "git";
    const revealRaw = raw && pane.hidden;
    if (!raw && !pane.hidden) {
      state.paneReadingScrollTop = pane.scrollTop;
      state.paneExpectedScrollTop = null;
    }
    const revealFiles = files && filesPanel.hidden;
    const revealGit = git && gitPanel.hidden;
    const conversationMode = state.viewMode === "conversation";
    const revealConversation = conversationMode && conversation.hidden;
    if (!conversationMode && conversationMeasurable()) {
      state.transcriptReadingScrollTop = conversation.scrollTop;
      state.transcriptExpectedScrollTop = null;
    }
    pane.hidden = !raw;
    conversation.hidden = !conversationMode;
    renderConversationMetrics();
    $("conversation-filters-open").hidden = !conversationMode;
    filesPanel.hidden = !files;
    gitPanel.hidden = !git;
    // Snapshots normally arrive while Conversation is visible, when the
    // hidden raw pane has no measurable scroll height. Reveal it first, then
    // restore the live tail only while the reader is still following. A raw
    // pane that the reader left mid-scroll keeps its exact position.
    if (revealRaw) {
      pane.scrollTop = state.paneFollowing
        ? pane.scrollHeight
        : state.paneReadingScrollTop;
      state.paneReadingScrollTop = pane.scrollTop;
      state.paneExpectedScrollTop = pane.scrollTop;
    }
    if (raw) scheduleRawFit(revealRaw ? 0 : 350);
    // Conversation is hidden while Files, Git or the raw pane are open, and a
    // hidden element reports no scroll height, so every redraw it missed left
    // it parked at the top. Re-apply the reader's place once it is measurable.
    if (revealConversation) {
      if (state.transcriptFollowing) scrollConversationToBottom();
      else if (conversationMeasurable()) {
        conversation.scrollTop = state.transcriptReadingScrollTop;
        state.transcriptExpectedScrollTop = conversation.scrollTop;
      }
    }
    renderTranscriptJump();
    const labels = { conversation: "Conversation", raw: "Live pane", files: "Project files", git: "Git status" };
    $("pane-heading").textContent = labels[state.viewMode];
    for (const mode of ["conversation", "raw", "files", "git"]) {
      const button = $(`${mode}-view`);
      const selected = state.viewMode === mode;
      button.classList.toggle("selected", selected);
      button.setAttribute("aria-selected", String(selected));
      button.setAttribute("aria-pressed", String(selected));
      button.tabIndex = selected ? 0 : -1;
    }
    renderConversationFilters();
    if (revealFiles) {
      const view = selectedProjectView();
      if (view?.files.listing) renderFiles();
      else void loadFilesDirectory("", [{ name: "Project", path: "" }]);
    }
    if (revealGit) {
      const view = selectedProjectView();
      if (view?.git.summary || view?.git.loading || view?.git.error) renderGit();
      else void loadGitSummary();
    }
  }

  /// A detached tmux window is 80x24 unless something sizes it, so a
  /// full-screen agent draws only 24 short rows. While Raw pane is open, ask
  /// the owner to fit the window to this view. The owner skips windows a
  /// terminal is attached to, and the next attached terminal takes over.
  function scheduleRawFit(delay = 350) {
    if (state.rawFitTimer !== null) clearTimeout(state.rawFitTimer);
    state.rawFitTimer = setTimeout(() => {
      state.rawFitTimer = null;
      void fitRawPane();
    }, delay);
  }

  function measureRawPaneGrid() {
    const style = getComputedStyle(pane);
    const probe = document.createElement("span");
    probe.textContent = "M".repeat(100);
    probe.style.cssText = "position:absolute;left:-10000px;top:0;visibility:hidden;white-space:pre";
    probe.style.font = style.font;
    document.body.append(probe);
    const charWidth = probe.getBoundingClientRect().width / 100;
    probe.remove();
    const fontSize = parseFloat(style.fontSize);
    const lineHeight = parseFloat(style.lineHeight) || fontSize * 1.45;
    const horizontal = (parseFloat(style.paddingLeft) || 0) + (parseFloat(style.paddingRight) || 0);
    const vertical = (parseFloat(style.paddingTop) || 0) + (parseFloat(style.paddingBottom) || 0);
    return rawPaneGrid({
      width: pane.clientWidth - horizontal,
      height: pane.clientHeight - vertical,
      charWidth,
      lineHeight,
    });
  }

  async function fitRawPane() {
    if (state.viewMode !== "raw" || pane.hidden || document.hidden) return;
    const paneId = state.selected;
    const session = state.sessions.get(paneId);
    if (!session || !isMachineControllable(machineOf(session))) return;
    const grid = measureRawPaneGrid();
    if (!grid) return;
    const key = `${paneId}:${grid.cols}x${grid.rows}`;
    if (state.rawFitKey === key) return;
    state.rawFitKey = key;
    try {
      await request(`/api/v1/panes/${encodeURIComponent(paneId)}/size`, {
        method: "POST",
        body: JSON.stringify({ cols: grid.cols, rows: grid.rows }),
      });
    } catch {
      // An older owner, a vanished pane or a transient failure: Raw pane
      // still shows what tmux has, and the next resize or visit retries.
      if (state.rawFitKey === key) state.rawFitKey = null;
    }
  }

  if (typeof ResizeObserver === "function") {
    new ResizeObserver(() => {
      if (state.viewMode === "raw" && !pane.hidden) scheduleRawFit();
    }).observe(pane);
  }

  function drawPane(initial) {
    if (!initial && (state.panePointerDown || selectionTouchesPane(pane, window.getSelection()))) {
      state.pendingPaneRender = true;
      return;
    }
    const paneVisible = !pane.hidden;
    const shouldFollow = initial || (state.paneFollowing
      && (!paneVisible || followsLiveTail(pane, LIVE_TAIL_TOLERANCE)));
    // A hidden element's DOM scrollTop may be clamped to zero. Reader intent
    // therefore lives in state and remains authoritative while Conversation
    // is visible and raw output continues to stream in the background.
    const readingOffset = paneVisible ? pane.scrollTop : state.paneReadingScrollTop;
    pane.textContent = state.paneLines.join("\n");
    state.pendingPaneRender = false;
    // Raw-pane streaming obeys the same explicit reader intent as
    // Conversation view: new output follows only while the reader remains
    // deliberately at the real tail.
    if (paneVisible) {
      pane.scrollTop = shouldFollow ? pane.scrollHeight : readingOffset;
      state.paneReadingScrollTop = pane.scrollTop;
      state.paneExpectedScrollTop = pane.scrollTop;
    } else {
      state.paneReadingScrollTop = readingOffset;
      state.paneExpectedScrollTop = null;
    }
    state.paneFollowing = shouldFollow;
  }

  function flushPendingPaneRender() {
    if (!state.pendingPaneRender
      || state.panePointerDown
      || selectionTouchesPane(pane, window.getSelection())) return;
    drawPane(false);
  }

  function setHealth(message) {
    const normalized = typeof message === "string" && message.trim() ? message.trim() : null;
    if (state.health === normalized) return;
    state.health = normalized;
    const alert = $("health-message");
    alert.textContent = normalized ? `tmux monitor: ${normalized}` : "";
    alert.hidden = !normalized;
    renderOverviewConnection();
  }

  function reconcileSelection() {
    if (state.pendingSelectionName) {
      const { name, machine } = state.pendingSelectionName;
      const launched = [...state.sessions.values()].find((session) =>
        session.name === name && (!machine || sessionMachineId(session) === machine));
      if (launched) {
        state.pendingSelectionName = null;
        selectSession(launched.id);
        return;
      }
    }
    if (state.selected && !state.sessions.has(state.selected)) selectSession(null, "replace");
    if (state.selectedMachine && !state.machines.some((machine) => machine.id === state.selectedMachine)) {
      selectMachine(null, "replace");
    }
  }

  function updateSelectionHistory(url, mode, changed) {
    if (mode === "none") return;
    const route = appRoute(url);
    const current = appRoute(location.href);
    // Keep one menu entry beneath the active detail. Replacing detail-to-detail
    // navigation means the browser Back gesture always returns to Agents instead
    // of walking through older agent/machine/usage screens.
    const shouldPush = mode === "push" && changed && current.view === "menu" && route.view !== "menu";
    if (shouldPush) history.pushState(appHistoryState(route), "", url);
    else history.replaceState(appHistoryState(route), "", url);
  }

  function invalidateLaunchDialog(close = true) {
    state.launchDialogGeneration += 1;
    state.launchFlow = null;
    state.launchSummarySourceId = null;
    $("launch-summary").hidden = true;
    $("launch-summary-resume").checked = false;
    cancelLaunchDirectorySearch();
    hideLaunchDirectorySuggestions(true);
    clearLaunchSessions();
    const dialog = $("launch-dialog");
    if (close && dialog.open) {
      dialog.dataset.launchGeneration = "";
      dialog.close();
    }
  }

  function selectSession(id, historyMode = "push") {
    if (state.inlineRename && state.inlineRename.id !== id) state.inlineRename.close();
    const changed = state.selected !== id || state.selectedMachine !== null || state.pulseOpen || state.historyOpen;
    const paneChanged = state.selected !== id;
    if (changed && !confirmDiscardFileEdit()) return false;
    if (changed) {
      persistBoundComposerDraft(true);
      invalidateLaunchDialog();
    }
    state.selected = id;
    state.selectedMachine = null;
    state.pulseOpen = false;
    closeSessionHistory();
    bindComposerDraftToSelection();
    stopPulseRefresh();
    stopPulseEvents();
    document.body.classList.toggle("has-selection", Boolean(id));
    const url = new URL(location.href);
    url.searchParams.delete("machine");
    url.searchParams.delete("view");
    if (id) url.searchParams.set("session", id); else url.searchParams.delete("session");
    updateSelectionHistory(url, historyMode, changed);
    if (paneChanged) {
      state.messageHistoryNavigation = null;
      connectPane();
    }
    render();
    return true;
  }

  function selectMachine(id, historyMode = "push") {
    const changed = state.selected !== null || state.selectedMachine !== id || state.pulseOpen || state.historyOpen;
    if (changed && !confirmDiscardFileEdit()) return false;
    if (changed) {
      persistBoundComposerDraft(true);
      invalidateLaunchDialog();
    }
    state.selected = null;
    state.selectedMachine = id;
    state.pulseOpen = false;
    closeSessionHistory();
    bindComposerDraftToSelection();
    resetProjectView();
    stopPulseRefresh();
    stopPulseEvents();
    document.body.classList.toggle("has-selection", Boolean(id));
    const url = new URL(location.href);
    url.searchParams.delete("session");
    url.searchParams.delete("view");
    if (id) url.searchParams.set("machine", id); else url.searchParams.delete("machine");
    updateSelectionHistory(url, historyMode, changed);
    state.paneSource?.close();
    state.paneSource = null;
    stopTranscriptPolling();
    render();
    return true;
  }

  function selectPulse(open, historyMode = "push") {
    const nextOpen = Boolean(open);
    const changed = state.selected !== null || state.selectedMachine !== null || state.pulseOpen !== nextOpen || state.historyOpen;
    if (changed && !confirmDiscardFileEdit()) return false;
    if (changed) {
      persistBoundComposerDraft(true);
      invalidateLaunchDialog();
    }
    closeSessionHistory();
    state.pulseOpen = nextOpen;
    state.selected = null;
    state.selectedMachine = null;
    bindComposerDraftToSelection();
    resetProjectView();
    const url = new URL(location.href);
    url.searchParams.delete("session");
    url.searchParams.delete("machine");
    if (state.pulseOpen) url.searchParams.set("view", "usage");
    else url.searchParams.delete("view");
    updateSelectionHistory(url, historyMode, changed);
    state.paneSource?.close();
    state.paneSource = null;
    stopTranscriptPolling();
    if (state.pulseOpen) {
      void loadPulseAccounts();
      if (state.pulseAccount && state.pulseAccountsLoaded) {
        connectPulseEvents();
        if (!Object.keys(state.pulseData).length) void refreshPulse(true);
        else schedulePulseRefresh();
      }
    } else {
      stopPulseRefresh();
      stopPulseEvents();
    }
    render();
    return true;
  }

  function closeSessionHistory() {
    state.historyOpen = false;
    state.historyGeneration += 1;
    state.historyController?.abort();
    clearTimeout(state.historyTimer);
  }

  function selectSessionHistory(open, historyMode = "push") {
    const changed = state.historyOpen !== Boolean(open) || state.selected !== null || state.selectedMachine !== null || state.pulseOpen;
    if (!selectSession(null, "none")) return false;
    state.historyOpen = Boolean(open);
    const url = agentMenuUrl(location.href);
    if (open) url.searchParams.set("view", "sessions");
    updateSelectionHistory(url, historyMode, changed);
    render();
    if (open) void loadSessionHistory();
    return true;
  }

  async function loadSessionHistory(more = false) {
    if (!state.historyOpen) return;
    state.historyController?.abort();
    clearTimeout(state.historyTimer);
    const controller = new AbortController();
    state.historyController = controller;
    const generation = ++state.historyGeneration;
    const filters = { text: $("history-text").value, state: $("history-state").value,
      machine: $("history-machine").value, project: $("history-project").value };
    $("history-status").textContent = "Loading sessions…";
    $("history-more").disabled = true;
    try {
      const page = await request(sessionHistoryQuery(filters, more ? state.historyCursor : null), { signal: controller.signal });
      if (!state.historyOpen || generation !== state.historyGeneration) return;
      const rows = (Array.isArray(page.sessions) ? page.sessions : []).slice(0, 100).map(sessionHistoryRow);
      const combined = more ? [...state.historyRows, ...rows] : rows;
      state.historyRows = [...new Map(combined.map((row) => [row.sessionKey, row])).values()].slice(0, 1000);
      state.historyCursor = state.historyRows.length < 1000 ? page.next_cursor : null;
      renderSessionHistory();
      $("history-status").textContent = state.historyRows.length ? `${state.historyRows.length} sessions${page.next_cursor ? " · more available" : ""}` : "No matching sessions.";
    } catch (error) {
      if (generation !== state.historyGeneration || controller.signal.aborted) return;
      $("history-status").textContent = error.status === 404 ? "Session history is disabled on this node." : `Session history unavailable: ${error.message}`;
    } finally {
      if (generation === state.historyGeneration) {
        $("history-more").disabled = false;
        state.historyTimer = setTimeout(() => { if (state.historyOpen && !document.hidden) void loadSessionHistory(); }, 30_000);
      }
    }
  }

  function renderSessionHistory() {
    const rows = state.historyRows.map((row) => {
      const item = document.createElement("li"); item.className = "session-history-row";
      item.dataset.sessionKey = row.sessionKey;
      const name = document.createElement("strong"); name.textContent = row.name;
      const description = document.createElement("p"); description.textContent = row.description;
      const metadata = document.createElement("p"); metadata.className = "meta";
      const date = row.lastActiveMs ? new Date(row.lastActiveMs) : null;
      metadata.textContent = [row.machine, row.project, date && Number.isFinite(date.getTime()) ? date.toLocaleString() : "No activity recorded", row.state].filter(Boolean).join(" · ");
      item.append(name, description, metadata);
      const live = [...state.sessions.values()].find((session) => session.session_key === row.sessionKey);
      if (live) {
        const button = document.createElement("button"); button.className = "subtle"; button.textContent = "Open";
        button.addEventListener("click", () => selectSession(live.id)); item.append(button);
      }
      const button = document.createElement("button"); button.className = "subtle"; button.textContent = "Resume on…";
      button.dataset.sessionResume = row.sessionKey;
      button.disabled = !resumeMachineOptions(state.machines).length;
      button.addEventListener("click", () => window.atmuxSessionResume(sessionHistoryResumeRequest(row)));
      item.append(button);
      return item;
    });
    $("history-list").replaceChildren(...rows);
    $("history-more").hidden = !state.historyCursor;
  }

  function backToAgentMenu() {
    const route = appRoute(location.href);
    if (route.view === "menu") return;
    // This control is an explicit in-app destination, not a generic browser
    // Back. Replacing the detail guarantees it cannot leave atmux for login.
    selectSession(null, "replace");
  }

  window.addEventListener("popstate", () => {
    const route = appRoute(location.href);
    const accepted = route.view === "session" ? selectSession(route.id, "none")
      : route.view === "machine" ? selectMachine(route.id, "none")
        : route.view === "sessions" ? selectSessionHistory(true, "none")
        : route.view === "usage" ? selectPulse(true, "none")
          : selectSession(null, "none");
    if (accepted === false) {
      const url = new URL(location.href);
      url.searchParams.delete("session"); url.searchParams.delete("machine"); url.searchParams.delete("view");
      if (state.selected) url.searchParams.set("session", state.selected);
      else if (state.selectedMachine) url.searchParams.set("machine", state.selectedMachine);
      else if (state.pulseOpen) url.searchParams.set("view", "usage");
      else if (state.historyOpen) url.searchParams.set("view", "sessions");
      // Back already exposed the existing Agents entry. Put the rejected
      // editor detail back above it; replacing here would consume that only
      // in-app escape hatch and make the next Back leave atmux/login origin.
      history.pushState(appHistoryState(appRoute(url)), "", url);
    }
  });
  window.addEventListener("beforeunload", (event) => {
    persistBoundComposerDraft(true);
    if (!fileEditHasUnsavedWork(state.projectView?.files)) return;
    event.preventDefault();
    event.returnValue = "";
  });

  function renderAgentBranch() {
    const badge = $("agent-branch");
    const view = state.projectView;
    const summary = view?.paneId === state.selected ? view.git.summary : null;
    const available = summary?.available === true;
    badge.hidden = !available;
    badge.textContent = available
      ? (summary.detached
        ? `Git · detached ${summary.branch || "HEAD"}`
        : `Git · ${summary.branch || "unknown branch"}`)
      : "";
    badge.title = available ? badge.textContent : "";
  }

  function render() {
    if (state.inlineRename && !paneOutputMatchesSession(state.inlineRename, state.sessions.get(state.inlineRename.id))) state.inlineRename.close();
    clearTimeout(state.statusTimer);
    state.statusTimer = null;
    const presented = presentSessionStatuses(
      state.statusPresentations,
      [...state.sessions.values()],
      Date.now(),
    );
    state.statusPresentations = presented.presentations;
    if (presented.nextDelay !== null) {
      state.statusTimer = setTimeout(() => {
        state.statusTimer = null;
        render();
      }, Math.ceil(presented.nextDelay) + 1);
    }
    const sessions = presented.sessions;
    renderCounts(sessions);
    renderRecoveryControl();
    renderUpdateAll();
    renderAttachments();

    const navigation = navigationView(sessions, state.machines, {
      query: state.filter,
      status: state.statusFilter,
      harness: state.harnessFilter,
      collapsed: state.collapsedMachines,
      favorites: state.favoriteSessions,
    });
    reconcileRows(navigation.groups);
    $("filter-clear").hidden = !navigation.filtering;
    $("filter-summary").textContent = navigation.filtering
      ? `${navigation.matchedCount} of ${navigation.totalCount} agents`
      : `${navigation.totalCount} agent${navigation.totalCount === 1 ? "" : "s"}`;
    $("empty").hidden = navigation.matchedCount > 0;
    $("empty").textContent = navigation.filtering ? "No agents match these filters." : "No agent sessions yet.";

    const selected = state.sessions.get(state.selected);
    const selectedMachine = state.machines.find((machine) => machine.id === state.selectedMachine) || null;
    $("welcome").hidden = Boolean(selected || selectedMachine || state.pulseOpen || state.historyOpen);
    $("machine-view").hidden = !selectedMachine || Boolean(selected);
    $("agent-view").hidden = !selected;
    $("history-view").hidden = !state.historyOpen;
    $("history-open").setAttribute("aria-pressed", String(state.historyOpen));
    $("history-open").classList.toggle("selected", state.historyOpen);
    $("pulse-view").hidden = !state.pulseOpen;
    $("pulse-open").classList.toggle("selected", state.pulseOpen);
    $("pulse-open").setAttribute("aria-pressed", String(state.pulseOpen));
    document.body.classList.toggle("has-selection", Boolean(selected || selectedMachine || state.pulseOpen || state.historyOpen));
    if (state.historyOpen) return;
    if (state.pulseOpen) {
      renderPulse();
      return;
    }
    if (selectedMachine && !selected) {
      renderMachineDetail(selectedMachine);
      return;
    }
    if (!selected) return;
    const machine = machineOf(selected);
    const controllable = isMachineControllable(machine);
    const launchCommand = selected.launch_command || selected.command || "";
    const agentName = $("agent-name");
    agentName.textContent = selected.name;
    agentName.title = launchCommand ? `tmux launch: ${launchCommand}` : "";
    const agentDescription = $("agent-description");
    agentDescription.textContent = selected.description || "";
    agentDescription.dataset.source = selected.description_source || "";
    agentDescription.hidden = !selected.description;
    renderAgentBranch();
    const folder = sessionFolderLabel(selected);
    const profile = sessionProfileLabel(selected);
    $("agent-meta").textContent = [
      folder, profile,
      machine?.label || sessionMachineId(selected),
      state.statusPresentations.get(selected.id)?.shown || selected.status,
      selected.agent,
      safeMemoryBytes(selected.memory_max_bytes) === null
        ? "Memory cap unavailable"
        : `Memory ${formatMemoryLimit(selected.memory_max_bytes)}`,
    ].filter(Boolean).join(" · ");
    $("agent-meta").title = selected.path || "";
    let inputBadge = $("agent-needs-input");
    if (!inputBadge) {
      inputBadge = document.createElement("span");
      inputBadge.id = "agent-needs-input";
      inputBadge.className = "needs-input-badge";
      $("agent-meta").before(inputBadge);
    }
    const inputReason = needsInputReason(selected, state.agentEventStates);
    inputBadge.hidden = !inputReason;
    inputBadge.textContent = needsInputLabel(inputReason);
    const launch = $("agent-launch");
    launch.hidden = !launchCommand;
    launch.textContent = launchCommand ? `tmux: ${launchCommand}` : "";
    renderModelControl(selected, controllable);
    renderAgentRestartAction(selected, controllable);
    $("quick-resume-on").hidden = !state.registryEnabled || !sessionResumeIntent(selected, "local");
    $("quick-resume-on").disabled = state.resumingOn || !resumeMachineOptions(state.machines).length;
    const resuming = Boolean(state.resumingPaneId);
    const preparingDuplicate = Boolean(state.duplicatingPaneId);
    for (const id of ["interrupt", "kill-open", "attach", "quick-duplicate", "quick-compact", "quick-interrupt", "quick-kill-open"]) $(id).disabled = !controllable || state.composerSending || resuming || preparingDuplicate;
    const keyTarget = paneSpecialKeyDelivery(selected, "enter");
    const keyTargetId = paneSpecialKeyTarget(keyTarget);
    const queuedKeys = keyTargetId
      ? state.specialKeyQueue.filter((item) => paneSpecialKeyTarget(item) === keyTargetId).length
        + Number(paneSpecialKeyTarget(state.specialKeySending) === keyTargetId)
      : 0;
    const keyQueueFull = state.specialKeyQueue.length
      + Number(Boolean(state.specialKeySending)) >= MAX_QUEUED_PANE_KEYS;
    document.querySelector(".quick-pane-keypad").setAttribute(
      "aria-busy",
      String(state.specialKeyQueue.length > 0 || Boolean(state.specialKeySending)),
    );
    document.querySelectorAll("[data-pane-key]").forEach((button) => {
      button.disabled = !controllable || state.composerSending || resuming || preparingDuplicate || keyQueueFull;
    });
    $("tmux-prefix-twice").disabled = !controllable || state.composerSending || resuming || preparingDuplicate || keyQueueFull;
    $("quick-tmux-prefix-twice").disabled = !controllable || state.composerSending || resuming || preparingDuplicate || keyQueueFull;
    const keyStatus = $("quick-pane-key-status");
    if (keyQueueFull) {
      keyStatus.textContent = `Key queue full (${MAX_QUEUED_PANE_KEYS}). Wait for a key to finish.`;
    } else if (queuedKeys > 0) {
      keyStatus.textContent = `${queuedKeys} key${queuedKeys === 1 ? "" : "s"} sending or queued for this agent.`;
    } else {
      keyStatus.textContent = state.specialKeyStatuses.get(keyTargetId) || "One key is sent per tap.";
    }
    $("quick-duplicate").textContent = preparingDuplicate ? "Preparing duplicate…" : "Duplicate agent";
    $("message").disabled = !controllable || resuming;
    $("send").disabled = !controllable || state.composerSending || resuming
      || (state.attachments.length > 0 && !attachmentsMatchCurrentSelection());
    const notice = paneNotice(machine, state.paneError, Date.now());
    const offline = $("agent-offline");
    offline.hidden = !notice;
    offline.textContent = notice;
    renderViewMode();
  }

  /// The topbar action and the dialog roster, both from the fleet document.
  function renderRecoveryControl() {
    const entries = recoveryMachines([...state.fleetRecovery.values()]);
    const button = $("recovery-open");
    button.hidden = entries.length === 0;
    button.textContent = recoveryInFlight(entries) ? "Resuming\u2026" : "Quick resume";
    $("recovery-machines").replaceChildren(...entries.map(createRecoveryRow));
    const status = $("recovery-status");
    status.hidden = entries.length > 0;
    status.textContent = entries.length ? "" : "No machine currently reports a saved session roster.";
  }

  function createRecoveryRow(entry) {
    const view = recoveryRowState(entry, state.recoveryBusy.has(entry.id));
    const li = document.createElement("li");
    li.className = "recovery-machine";
    li.dataset.machineId = view.id;
    const label = textSpan(view.label, "recovery-machine-label");
    const message = textSpan(view.message, "recovery-machine-message");
    message.setAttribute("role", "status");
    const actions = document.createElement("div");
    actions.className = "recovery-machine-actions";
    const button = document.createElement("button");
    button.type = "button";
    button.className = "primary";
    button.textContent = view.action;
    button.disabled = !view.canStart;
    button.addEventListener("click", () => { void startRecovery(view.id); });
    actions.append(button);
    li.append(label, message, actions);
    return li;
  }

  function renderModelControl(session, controllable) {
    const control = $("model-control");
    const quickControl = $("quick-model-control");
    const view = modelPickerState(
      session,
      state.paneModels,
      controllable,
      state.modelSwitchingPaneId,
      state.composerSending,
    );
    control.hidden = !view.visible;
    quickControl.hidden = !view.visible;
    if (!view.visible) return;
    const models = pickerOptions(view.models, view.current);
    const efforts = pickerOptions(view.efforts, view.effort);
    for (const [modelId, effortId, fastId, statusId] of [
      ["agent-model", "agent-effort", "agent-fast", "model-status"],
      ["quick-agent-model", "quick-agent-effort", "quick-agent-fast", "quick-model-status"],
    ]) {
      syncPickerSelect($(modelId), models, view.current, view.disabled, "No switchable models");
      syncPickerSelect($(effortId), efforts, view.effort, view.effortDisabled, "No switchable effort levels");
      const fast = $(fastId);
      fast.checked = view.fast === true;
      fast.disabled = view.fastDisabled;
      fast.closest("label").hidden = !view.fastSupported;
      const status = $(statusId);
      status.textContent = view.status;
      status.title = view.status;
    }
  }

  /// Rebuilds one picker only when its choices changed, so a live refresh never
  /// drops the open dropdown or the selection under the pointer.
  function syncPickerSelect(select, options, selected, disabled, empty) {
    const signature = JSON.stringify(options);
    if (select.dataset.models !== signature) {
      select.replaceChildren(...(options.length
        ? options.map((choice) => option(choice.id, choice.label, !choice.switchable))
        : [option("", empty, true)]));
      select.dataset.models = signature;
    }
    if (selected && options.some((choice) => choice.id === selected)) select.value = selected;
    select.disabled = disabled;
  }

  function renderAgentRestartAction(session, controllable) {
    const button = $("quick-resume");
    const note = $("quick-resume-note");
    const view = agentRestartState(
      session,
      state.paneModels,
      controllable,
      state.resumingPaneId,
      state.composerSending,
    );
    button.hidden = !view.visible;
    button.disabled = view.disabled;
    button.title = view.status;
    note.textContent = view.visible ? view.status : "";
    note.hidden = !view.visible || !view.status;
  }

  function createMachineNode(machine) {
    const li = document.createElement("li");
    li.className = "machine-row";
    const heading = document.createElement("div");
    heading.className = "machine-heading";
    const toggle = document.createElement("button");
    toggle.type = "button";
    toggle.className = "machine-toggle";
    toggle.dataset.machineId = machine.id;
    toggle.dataset.machineAction = "collapse";
    const children = document.createElement("ul");
    children.className = "machine-sessions";
    children.id = `machine-sessions-${encodeURIComponent(machine.id)}`;
    toggle.setAttribute("aria-controls", children.id);
    toggle.addEventListener("click", () => {
      if (state.collapsedMachines.has(machine.id)) state.collapsedMachines.delete(machine.id);
      else state.collapsedMachines.add(machine.id);
      saveNavigationPreferences();
      render();
    });
    const header = document.createElement("button");
    header.type = "button";
    header.className = "machine-header";
    header.dataset.machineId = machine.id;
    header.dataset.machineAction = "details";
    header.addEventListener("click", () => selectMachine(machine.id));
    const dot = textSpan("", "machine-dot");
    dot.setAttribute("aria-hidden", "true");
    const label = textSpan("", "machine-label");
    const pill = textSpan("", "machine-update-pill");
    pill.hidden = true;
    const status = textSpan("", "machine-status");
    header.append(dot, label, pill, status);
    heading.append(toggle, header);
    li.append(heading, children);
    return { li, header, toggle, children, dot, label, pill, status, machineId: machine.id };
  }

  function updateMachineNode(node, group) {
    const { machine, collapsed, filtering } = group;
    const online = isMachineControllable(machine);
    const label = machine.label || machine.id;
    node.toggle.textContent = collapsed ? "›" : "⌄";
    node.toggle.setAttribute("aria-expanded", String(!collapsed));
    node.toggle.setAttribute("aria-label", `${collapsed ? "Expand" : "Collapse"} ${label} agents`);
    node.toggle.disabled = filtering;
    node.toggle.title = filtering ? "Matching agents are expanded while filters are active" : `${collapsed ? "Expand" : "Collapse"} ${label} agents`;
    node.children.hidden = collapsed;
    node.children.setAttribute("aria-label", `${label} agents`);
    node.header.className = `machine-header ${online ? "online" : "offline"}${state.selectedMachine === machine.id ? " selected" : ""}`;
    node.header.title = `View ${label} details`;
    node.dot.textContent = online ? "◉" : "○";
    node.label.textContent = label;
    const pill = machineUpdatePill(state.fleetUpdates.get(machine.id));
    node.pill.textContent = pill;
    node.pill.hidden = !pill;
    node.pill.title = pill ? "A verified atmux update is ready for this machine" : "";
    node.status.textContent = machineStatusLabel(machine, Date.now());
    node.li.setAttribute(
      "aria-label",
      `${machine.label || machine.id}, ${online ? "online" : "offline"}`,
    );
  }

  /// Reconciles machine headers and session buttons in place. Nodes are reused
  /// so streaming updates never destroy focus or scroll position.
  function reconcileRows(groups) {
    const focused = document.activeElement;
    let cursor = sessionList.firstElementChild;
    for (const group of groups) {
      let node = machineNodes.get(group.machine.id);
      if (!node) {
        node = createMachineNode(group.machine);
        machineNodes.set(group.machine.id, node);
      }
      updateMachineNode(node, group);
      node.li.dataset.machineId = group.machine.id;
      if (node.li === cursor) cursor = cursor.nextElementSibling;
      else sessionList.insertBefore(node.li, cursor);
      let childCursor = node.children.firstElementChild;
      for (const session of group.sessions) {
        let child = sessionNodes.get(session.id);
        if (!child) {
          child = createSessionNode(session.id);
          sessionNodes.set(session.id, child);
        }
        updateSessionNode(child, session);
        child.li.dataset.rowKey = session.id;
        if (child.li === childCursor) childCursor = childCursor.nextElementSibling;
        else node.children.insertBefore(child.li, childCursor);
      }
      const childIds = new Set(group.sessions.map((session) => session.id));
      for (const child of [...node.children.children]) {
        if (!childIds.has(child.dataset.rowKey)) child.remove();
      }
    }
    const machineIds = new Set(groups.map((group) => group.machine.id));
    for (const child of [...sessionList.children]) {
      if (!machineIds.has(child.dataset.machineId)) child.remove();
    }
    for (const id of sessionNodes.keys()) {
      if (!state.sessions.has(id)) sessionNodes.delete(id);
    }
    const localMachineId = state.machines.find((machine) => machine.kind === "local")?.id || "local";
    const liveMachineIds = new Set([
      ...state.machines.map((machine) => machine.id),
      ...[...state.sessions.values()].map((session) => sessionMachineId(session, localMachineId)),
    ]);
    for (const id of machineNodes.keys()) {
      if (!liveMachineIds.has(id)) machineNodes.delete(id);
    }
    if (focused?.isConnected && !focused.closest("[hidden]")
        && document.activeElement !== focused) focused.focus({ preventScroll: true });
  }

  function renderCounts(sessions = [...state.sessions.values()]) {
    renderOverviewConnection();
    const counts = $("counts");
    const working = sessions.filter((item) => item.status === "working").length;
    const waiting = sessions.filter((item) => item.status === "waiting").length;
    counts.replaceChildren(
      textSpan(`● ${working} working`, "count-working"),
      textSpan(`◆ ${waiting} waiting`, "count-waiting"),
    );
  }

  function renderOverviewConnection() {
    const view = overviewConnectionPresentation(
      state.overviewConnection,
      Date.now() - state.overviewConnectionSince,
    );
    const status = $("overview-status");
    status.textContent = view.label;
    status.dataset.connection = state.overviewConnection;
    const notice = $("overview-notice");
    notice.hidden = !view.retry;
    $("overview-note").textContent = view.note;
    $("overview-retry").disabled = !view.retry;
    $("health-alert").hidden = !state.health && !view.retry;
  }

  function renderMachineDetail(machine) {
    $("machine-name").textContent = machine.label || machine.id;
    const status = machine.online ? "Online" : "Offline";
    $("machine-meta").textContent = [
      status,
      machine.address,
      `${machine.sessions ?? 0} agent${(machine.sessions ?? 0) === 1 ? "" : "s"}`,
    ].filter(Boolean).join(" · ");
    const offline = $("machine-offline");
    offline.hidden = machine.online !== false;
    offline.textContent = machine.health || "This machine is offline.";
    const metrics = machine.metrics || {};
    const cards = [
      metricCard("CPU", metrics.cpu_percent == null ? "—" : `${metrics.cpu_percent}%`, "Current total utilization"),
      metricCard("Memory", memoryValue(metrics.memory_used_bytes, metrics.memory_total_bytes), "Used / total"),
      metricListCard("System", systemMetricLines(metrics)),
      gpuMetricCard(metrics.gpus, metrics.gpu_diagnostics),
      metricListCard("Temperatures", temperatureLines(metrics.temperatures)),
    ];
    $("machine-metrics").replaceChildren(...cards);
    renderMachineSoftware(machine);
  }

  /// The machine view's Software card.
  ///
  /// Every enabled action comes from the owning node's own document, so this
  /// never offers an operator a button the node would refuse.
  function renderMachineSoftware(machine) {
    const card = $("machine-software");
    const entry = state.fleetUpdates.get(machine.id) || null;
    const model = softwareCardModel(entry, Date.now());
    const busy = state.updateBusy.has(machine.id);
    const heading = document.createElement("h2");
    heading.textContent = "Software";
    const children = [heading, textSpan(model.version, "software-version"),
      textSpan(model.latest, "software-latest")];
    if (model.state) {
      const line = textSpan(model.state, "software-state");
      line.setAttribute("role", "status");
      children.push(line);
    }
    if (model.error) {
      const line = textSpan(model.error, "software-error");
      line.setAttribute("role", "status");
      children.push(line);
    }
    const actions = document.createElement("div");
    actions.className = "software-actions";
    actions.append(
      softwareButton(machine, "check", "Check now", model.canCheck && !busy),
      softwareButton(machine, "apply", "Update", model.canUpdate && !busy),
      softwareButton(machine, "rollback", "Roll back", model.canRollback && !busy),
    );
    children.push(actions);
    card.replaceChildren(...children);
  }

  function softwareButton(machine, action, label, enabled) {
    const button = document.createElement("button");
    button.type = "button";
    button.className = action === "apply" ? "primary" : "subtle";
    button.textContent = label;
    button.disabled = !enabled;
    button.dataset.updateAction = action;
    button.dataset.machineId = machine.id;
    button.addEventListener("click", () => {
      // Both restarting verbs are confirmed; only a read-only check is not.
      if (action === "check") void runMachineUpdate(machine.id, action);
      else openUpdateConfirm([machine.id], action);
    });
    return button;
  }

  /// The one landing-page action, shown only when something can be installed.
  function renderUpdateAll() {
    const button = $("update-all-open");
    const targets = updatableMachines([...state.fleetUpdates.values()]);
    button.hidden = targets.length === 0;
    button.textContent = `\u2191 Update all (${targets.length})`;
    button.disabled = state.updateBusy.size > 0;
    button.dataset.updateCount = String(targets.length);
  }

  function metricCard(title, value, sub) {
    const card = document.createElement("section"); card.className = "metric-card";
    const heading = document.createElement("h2"); heading.textContent = title;
    const main = textSpan(value, "metric-value");
    const detail = textSpan(sub, "metric-sub");
    card.append(heading, main, detail);
    return card;
  }

  function metricListCard(title, lines) {
    const card = document.createElement("section"); card.className = "metric-card";
    const heading = document.createElement("h2"); heading.textContent = title;
    const list = document.createElement("ul"); list.className = "metric-list";
    for (const line of lines.length ? lines : ["Unavailable on this machine"]) {
      const item = document.createElement("li"); item.textContent = line; list.append(item);
    }
    card.append(heading, list);
    return card;
  }

  function gpuMetricCard(gpus, diagnostics) {
    const card = document.createElement("section"); card.className = "metric-card gpu-metric-card";
    const heading = document.createElement("h2"); heading.textContent = "Graphics";
    card.append(heading);
    if (!Array.isArray(gpus) || !gpus.length) {
      card.append(textSpan("Unavailable on this machine", "metric-sub"));
    } else {
      for (const gpu of gpus) {
        const details = document.createElement("details"); details.className = "gpu-device";
        const summary = document.createElement("summary"); summary.textContent = gpuSummary(gpu);
        const list = document.createElement("ul"); list.className = "metric-list gpu-detail-list";
        for (const line of gpuDetailLines(gpu)) {
          const item = document.createElement("li"); item.textContent = line; list.append(item);
        }
        details.append(summary, list);
        card.append(details);
      }
    }
    const diagnosticLines = gpuDiagnosticLines(diagnostics);
    if (diagnosticLines.length) {
      const details = document.createElement("details"); details.className = "gpu-diagnostics";
      const summary = document.createElement("summary"); summary.textContent = "Collector diagnostics";
      const list = document.createElement("ul"); list.className = "metric-list";
      for (const line of diagnosticLines) {
        const item = document.createElement("li"); item.textContent = line; list.append(item);
      }
      details.append(summary, list);
      card.append(details);
    }
    return card;
  }

  function temperatureLines(temperatures) {
    if (!Array.isArray(temperatures)) return [];
    return temperatures.map((reading) => `${reading.label || "Sensor"} · ${reading.celsius}°C`);
  }

  function textSpan(text, className) {
    const span = document.createElement("span"); span.textContent = text; span.className = className; return span;
  }

  function createSessionNode(id) {
    const li = document.createElement("li");
    li.className = "session-row";
    const button = document.createElement("button");
    button.type = "button";
    button.dataset.sessionId = id;
    button.addEventListener("click", () => selectSession(id));
    const dot = textSpan("", "session-dot");
    dot.setAttribute("aria-hidden", "true");
    const copy = document.createElement("span"); copy.className = "session-copy";
    const name = textSpan("", "session-name");
    bindInlineRenameGesture(name, () => openInlineRename(id, name));
    name.title = "Double-click or hold to rename (F2 for selected session)";
    const description = textSpan("", "session-description");
    description.hidden = true;
    const sub = textSpan("", "session-sub");
    copy.append(name, description, sub);
    button.append(dot, copy);
    const pinButton = document.createElement("button");
    pinButton.type = "button";
    pinButton.className = "session-pin";
    pinButton.dataset.sessionId = id;
    pinButton.dataset.sessionAction = "pin";
    pinButton.addEventListener("click", () => {
      const session = state.sessions.get(id);
      const localMachineId = state.machines.find((machine) => machine.kind === "local")?.id || "local";
      const key = favoriteSessionKey(session, localMachineId);
      if (!key) return;
      if (state.favoriteSessions.has(key)) state.favoriteSessions.delete(key);
      else state.favoriteSessions.add(key);
      saveNavigationPreferences();
      render();
    });
    const deleteButton = document.createElement("button");
    deleteButton.type = "button";
    deleteButton.className = "session-delete";
    deleteButton.dataset.sessionId = id;
    deleteButton.dataset.sessionAction = "delete";
    deleteButton.textContent = "🗑";
    deleteButton.title = "Kill this session";
    deleteButton.addEventListener("click", () => openKillDialog(id));
    const editButton = document.createElement("button");
    editButton.type = "button";
    editButton.className = "session-edit";
    editButton.dataset.sessionId = id;
    editButton.dataset.sessionAction = "edit";
    editButton.textContent = "✎";
    editButton.title = "Rename or describe this session";
    editButton.addEventListener("click", () => openSessionEditDialog(id));
    li.append(button, pinButton, editButton, deleteButton);
    return { li, button, pinButton, editButton, deleteButton, dot, name, description, sub };
  }

  function updateSessionNode(node, session) {
    const selected = session.id === state.selected;
    node.button.className = `session-button ${session.status}${selected ? " selected" : ""}`;
    node.button.setAttribute("aria-current", selected ? "true" : "false");
    const folder = sessionFolderLabel(session);
    const profile = sessionProfileLabel(session);
    const localMachineId = state.machines.find((machine) => machine.kind === "local")?.id || "local";
    const favoriteKey = favoriteSessionKey(session, localMachineId);
    const pinned = Boolean(favoriteKey && state.favoriteSessions.has(favoriteKey));
    node.pinButton.textContent = pinned ? "★" : "☆";
    node.pinButton.setAttribute("aria-pressed", String(pinned));
    node.pinButton.setAttribute("aria-label", `${pinned ? "Unpin" : "Pin"} ${session.name}`);
    node.pinButton.disabled = !favoriteKey;
    node.pinButton.title = favoriteKey ? `${pinned ? "Unpin" : "Pin"} ${session.name}` : "Pinning requires a current agent owner";
    node.button.setAttribute("aria-label", [session.name, session.description, folder, profile, session.status, session.agent].filter(Boolean).join(", "));
    node.editButton.setAttribute("aria-label", `Rename or describe ${session.name}`);
    node.editButton.disabled = !isMachineControllable(machineOf(session))
      || !PANE_INSTANCE_PATTERN.test(String(session.instance_id || ""));
    node.deleteButton.setAttribute("aria-label", `Kill ${session.name}`);
    node.deleteButton.disabled = !isMachineControllable(machineOf(session));
    node.dot.textContent = session.status === "working" ? "●" : session.status === "waiting" ? "◆" : "○";
    node.name.textContent = session.name;
    node.description.textContent = session.description || "";
    node.description.title = session.description || "";
    node.description.dataset.source = session.description_source || "";
    node.description.hidden = !session.description;
    node.sub.textContent = [folder, profile, session.status, session.agent].filter(Boolean).join(" · ");
    node.sub.title = session.path || "";
    if (!node.inputBadge) {
      node.inputBadge = document.createElement("span");
      node.inputBadge.className = "needs-input-badge";
      node.sub.after(node.inputBadge);
    }
    const reason = needsInputReason(session, state.agentEventStates);
    node.inputBadge.hidden = !reason;
    node.inputBadge.textContent = needsInputLabel(reason);
    if (reason) node.button.setAttribute("aria-label", `${node.button.getAttribute("aria-label")}, ${needsInputLabel(reason)}`);
  }

  async function request(url, options = {}) {
    const response = await fetch(url, {
      ...options,
      headers: options.body ? { "Content-Type": "application/json", ...(options.headers || {}) } : options.headers,
    });
    if (!response.ok) {
      const data = await response.json().catch(() => ({}));
      const error = new Error(data.error || `${response.status} ${response.statusText}`);
      error.status = response.status;
      throw error;
    }
    return response.status === 204 ? null : response.json();
  }

  function toast(message) {
    const node = $("toast"); node.textContent = message; node.classList.add("visible");
    clearTimeout(toast.timer); toast.timer = setTimeout(() => node.classList.remove("visible"), 3000);
  }

  function pulseNode(tag, className = "", text = null) {
    const node = document.createElement(tag);
    if (className) node.className = className;
    if (text !== null) node.textContent = String(text);
    return node;
  }

  function pulseButton(label, action, className = "subtle") {
    const button = pulseNode("button", className, label);
    button.type = "button";
    button.disabled = state.pulseMutation;
    button.addEventListener("click", action);
    return button;
  }

  function pulseSection(title, meta = "") {
    const section = pulseNode("section", "pulse-section");
    const head = pulseNode("header", "pulse-section-head");
    head.append(pulseNode("h2", "", title));
    if (meta) head.append(pulseNode("span", "meta", meta));
    section.append(head);
    return section;
  }

  function pulseEmpty(message, offline = false) {
    return pulseNode("p", `pulse-empty${offline ? " pulse-offline" : ""}`, message);
  }

  function pulseNumber(value) {
    const number = Number(value);
    return Number.isFinite(number) ? number : 0;
  }

  function pulsePercent(value) {
    return Math.min(100, Math.max(0, pulseNumber(value)));
  }

  function pulseTime(value) {
    const milliseconds = Date.parse(String(value || ""));
    return Number.isFinite(milliseconds) ? formatRelativeTime(milliseconds, Date.now()) : "unknown";
  }

  function pulseLabel(value) {
    return String(value || "unknown").replaceAll("_", " ").replaceAll("-", " ");
  }

  function pulseGauge(value, label, detail = "") {
    const percent = pulsePercent(value);
    const wrapper = pulseNode("div", "pulse-gauge");
    const copy = pulseNode("div", "pulse-gauge-copy");
    copy.append(pulseNode("strong", "", label), pulseNode("span", "", `${percent.toFixed(1)}%`));
    const progress = pulseNode("progress", `pulse-meter ${percent >= 90 ? "critical" : percent >= 70 ? "warn" : ""}`);
    progress.max = 100;
    progress.value = percent;
    progress.setAttribute("aria-label", `${label}: ${percent.toFixed(1)} percent`);
    wrapper.append(copy, progress);
    if (detail) wrapper.append(pulseNode("p", "pulse-card-meta", detail));
    return wrapper;
  }

  function pulseTotals(totals = {}) {
    const row = pulseNode("dl", "pulse-totals");
    for (const [label, value] of [
      ["Tokens", Number(totals.total_tokens || 0).toLocaleString()],
      ["Input", Number(totals.tokens_in || 0).toLocaleString()],
      ["Output", Number(totals.tokens_out || 0).toLocaleString()],
      ["Cache read", Number(totals.cache_read || 0).toLocaleString()],
      ["Cost", `$${pulseNumber(totals.cost_usd).toFixed(2)}`],
    ]) {
      row.append(pulseNode("dt", "", label), pulseNode("dd", "", value));
    }
    return row;
  }

  async function pulseFetchPage(account, resource, query = {}) {
    const items = [];
    let cursor = null;
    let pages = 0;
    do {
      const path = pulseAccountPath(account, resource, { ...query, cursor, limit: PULSE_PAGE_LIMIT });
      if (!path) throw new Error("Invalid Pulse request");
      const page = await request(path);
      if (!page || !Array.isArray(page.items)) throw new Error("Pulse returned an invalid page");
      items.push(...page.items);
      pages += 1;
      cursor = page.next_cursor;
    } while (pulseCanFollowCursor(cursor, pages));
    return items;
  }

  function stopPulseRefresh() {
    clearTimeout(state.pulseTimer);
    state.pulseTimer = null;
  }

  function stopPulseEvents() {
    state.pulseStreamGeneration += 1;
    state.pulseSource?.close();
    state.pulseSource = null;
    state.pulseSourceAccount = null;
    state.pulseStreamAwaitingInitial = false;
    clearTimeout(state.pulseReconnectTimer);
    clearTimeout(state.pulseInvalidationTimer);
    state.pulseReconnectTimer = null;
    state.pulseInvalidationTimer = null;
    state.pulseInvalidationQueued = false;
  }

  function queuePulseInvalidationRefresh(account, streamGeneration) {
    if (state.pulseInvalidationTimer || state.pulseInvalidationQueued) return;
    state.pulseInvalidationTimer = setTimeout(() => {
      state.pulseInvalidationTimer = null;
      if (document.hidden || !state.pulseOpen || state.pulseAccount !== account
        || state.pulseStreamGeneration !== streamGeneration) return;
      if (state.pulseLoading || state.pulseMutation) {
        state.pulseInvalidationQueued = true;
        return;
      }
      void refreshPulse();
    }, PULSE_INVALIDATION_DEBOUNCE_MS);
  }

  function flushPulseInvalidationRefresh() {
    if (!state.pulseInvalidationQueued || state.pulseLoading || state.pulseMutation) return;
    state.pulseInvalidationQueued = false;
    queuePulseInvalidationRefresh(state.pulseAccount, state.pulseStreamGeneration);
  }

  function connectPulseEvents() {
    stopPulseEvents();
    const account = state.pulseAccount;
    const path = pulseEventsPath(account);
    if (!state.pulseOpen || !path || document.hidden) return;
    const streamGeneration = state.pulseStreamGeneration;
    const source = new EventSource(path);
    state.pulseSource = source;
    state.pulseSourceAccount = account;
    state.pulseStreamAwaitingInitial = true;
    source.onopen = () => {
      if (state.pulseSource === source) state.pulseEventFailures = 0;
    };
    source.addEventListener("pulse", (event) => {
      if (state.pulseSource !== source || state.pulseAccount !== account
        || state.pulseStreamGeneration !== streamGeneration || document.hidden) return;
      const initial = state.pulseStreamAwaitingInitial;
      state.pulseStreamAwaitingInitial = false;
      const action = pulseInvalidationAction(state.pulseEventRevision, event.lastEventId, initial);
      if (action === "invalid") {
        source.close();
        state.pulseSource = null;
        return;
      }
      if (action === "ignore") return;
      state.pulseEventRevision = pulseRevisionId(event.lastEventId);
      queuePulseInvalidationRefresh(account, streamGeneration);
    });
    source.onerror = () => {
      if (state.pulseSource !== source || state.pulseAccount !== account
        || state.pulseStreamGeneration !== streamGeneration) return;
      source.close();
      state.pulseSource = null;
      if (document.hidden || !state.pulseOpen) return;
      state.pulseEventFailures += 1;
      clearTimeout(state.pulseReconnectTimer);
      state.pulseReconnectTimer = setTimeout(() => {
        if (!document.hidden && state.pulseOpen && state.pulseAccount === account
          && state.pulseStreamGeneration === streamGeneration) connectPulseEvents();
      }, pulseReconnectDelay(state.pulseEventFailures));
    };
  }

  function schedulePulseRefresh() {
    stopPulseRefresh();
    if (!state.pulseOpen || !state.pulseAccount || document.hidden) return;
    state.pulseTimer = setTimeout(() => { void refreshPulse(); }, pulseRefreshDelay(state.pulseFailures));
  }

  function pulseTasks(account) {
    const tasks = [];
    const page = (key, resource, query) => tasks.push([key, () => pulseFetchPage(account, resource, query)]);
    if (state.pulseTab === "dashboard") {
      page("profiles", "profiles");
      page("usage", "usage");
      page("pace", "pace");
      page("context", "context");
      page("gemini", "gemini");
      page("machines", "machines");
      page("health", "health");
      page("alerts", "alerts", { acknowledged: false });
      page("subscriptions", "alert-subscriptions");
      tasks.push(["limits", () => request(pulseAccountPath(account, "limits"))]);
      const query = { days: state.pulseReport.days, granularity: state.pulseReport.granularity, drill: state.pulseReport.drill };
      tasks.push(["report", () => request(pulseAccountPath(account, "reports", query))]);
    } else if (state.pulseTab === "reports") {
      const query = { days: state.pulseReport.days, granularity: state.pulseReport.granularity, drill: state.pulseReport.drill };
      tasks.push(["report", () => request(pulseAccountPath(account, "reports", query))]);
    } else if (state.pulseTab === "alerts") {
      page("alerts", "alerts", { acknowledged: false });
      page("subscriptions", "alert-subscriptions");
      tasks.push(["limits", () => request(pulseAccountPath(account, "limits"))]);
    } else if (state.pulseTab === "settings") {
      page("profiles", "profiles");
      page("pricing", "pricing");
      page("machines", "machines");
      page("ingestTokens", "ingest-tokens");
      tasks.push(["limits", () => request(pulseAccountPath(account, "limits"))]);
    }
    return tasks;
  }

  async function refreshPulse(manual = false) {
    const account = state.pulseAccount;
    if (!account || !state.pulseOpen || document.hidden) {
      renderPulse();
      return;
    }
    const generation = ++state.pulseGeneration;
    state.pulseLoading = true;
    if (manual) state.pulseErrors = {};
    stopPulseRefresh();
    renderPulse();
    const results = await Promise.all(pulseTasks(account).map(async ([key, load]) => {
      try { return { key, value: await load(), error: null }; }
      catch (error) { return { key, value: null, error: error.message || "Request failed" }; }
    }));
    if (!pulseRequestStillCurrent(account, state.pulseAccount, generation, state.pulseGeneration)) return;
    let successes = 0;
    for (const result of results) {
      if (result.error) state.pulseErrors[result.key] = result.error;
      else {
        state.pulseData[result.key] = result.value;
        delete state.pulseErrors[result.key];
        successes += 1;
      }
    }
    state.pulseLoading = false;
    if (successes) {
      state.pulseFailures = 0;
      state.pulseLastLoadedAt = Date.now();
    } else state.pulseFailures += 1;
    renderPulse();
    schedulePulseRefresh();
    flushPulseInvalidationRefresh();
  }

  function rememberPulseAccount(account) {
    writeLocalStorage("atmux.pulse-account", String(account));
  }

  async function loadPulseAccounts(force = false) {
    if (state.pulseAccountsLoading || (state.pulseAccountsLoaded && !force)) return;
    state.pulseAccountsLoading = true;
    state.pulseAccountsError = null;
    renderPulse();
    try {
      const response = await request("/api/v1/pulse/accounts");
      const accounts = pulseAccounts(response);
      if (!Array.isArray(response) || accounts.length !== response.length) {
        throw new Error("Pulse returned an invalid account list");
      }
      state.pulseAccounts = accounts;
      state.pulseAccountsLoaded = true;
      const account = preferredPulseAccount(accounts, state.pulseAccount, storedPulseAccount);
      if (!account) {
        state.pulseGeneration += 1;
        state.pulseAccount = null;
        stopPulseEvents();
        const url = new URL(location.href);
        url.searchParams.delete("pulseAccount");
        history.replaceState(appHistoryState(appRoute(url)), "", url);
      } else if (account !== state.pulseAccount) {
        setPulseAccount(account);
      } else {
        rememberPulseAccount(account);
        if (state.pulseOpen) {
          connectPulseEvents();
          if (!Object.keys(state.pulseData).length) void refreshPulse(true);
        }
      }
    } catch (error) {
      state.pulseAccounts = [];
      state.pulseAccountsLoaded = false;
      state.pulseAccountsError = error.message || "Pulse account discovery failed";
      state.pulseAccount = null;
      stopPulseEvents();
    } finally {
      state.pulseAccountsLoading = false;
      renderPulse();
    }
  }

  function setPulseAccount(value) {
    const account = pulseAccountId(value);
    if (!account || (state.pulseAccountsLoaded && !state.pulseAccounts.some((item) => item.id === account))) {
      toast("Choose an available Pulse account");
      return false;
    }
    state.pulseGeneration += 1;
    stopPulseEvents();
    state.pulseAccount = account;
    state.pulseEventRevision = null;
    state.pulseEventFailures = 0;
    state.pulseData = {};
    state.pulseErrors = {};
    state.pulseFailures = 0;
    state.pulseIssuedToken = null;
    rememberPulseAccount(account);
    const url = new URL(location.href);
    url.searchParams.set("pulseAccount", String(account));
    history.replaceState(appHistoryState(appRoute(url)), "", url);
    connectPulseEvents();
    void refreshPulse(true);
    return true;
  }

  function renderPulse() {
    const select = $("pulse-account");
    const options = state.pulseAccounts.map((account) => {
      const optionNode = pulseNode("option", "", pulseAccountLabel(account));
      optionNode.value = String(account.id);
      return optionNode;
    });
    if (!options.length) {
      const placeholder = pulseNode("option", "", state.pulseAccountsLoading ? "Discovering…" : "No Pulse accounts");
      placeholder.value = "";
      options.push(placeholder);
    }
    select.replaceChildren(...options);
    select.value = state.pulseAccount ? String(state.pulseAccount) : "";
    select.disabled = state.pulseAccountsLoading || state.pulseAccounts.length <= 1;
    $("pulse-account-form").hidden = state.pulseAccountsLoaded && state.pulseAccounts.length === 0;
    document.querySelectorAll("[data-pulse-tab]").forEach((button) => {
      const selected = button.dataset.pulseTab === state.pulseTab;
      button.classList.toggle("selected", selected);
      button.setAttribute("aria-pressed", String(selected));
    });
    $("pulse-refresh").disabled = !state.pulseAccount || state.pulseLoading || state.pulseMutation;
    const status = $("pulse-status");
    const selectedAccount = state.pulseAccounts.find((account) => account.id === state.pulseAccount);
    const selectedLabel = pulseAccountLabel(selectedAccount);
    if (state.pulseAccountsLoading) status.textContent = "Discovering Pulse dashboard…";
    else if (state.pulseAccountsError) status.textContent = "Pulse dashboard unavailable";
    else if (!state.pulseAccount) status.textContent = "No Pulse account is configured on this server.";
    else if (state.pulseLoading) status.textContent = `Loading ${selectedLabel}…`;
    else if (state.pulseLastLoadedAt) status.textContent = `${selectedLabel} · updated ${formatRelativeTime(state.pulseLastLoadedAt, Date.now())}`;
    else status.textContent = `${selectedLabel} · not loaded`;

    const notice = $("pulse-notice");
    const errors = Object.entries(state.pulseErrors);
    notice.hidden = errors.length === 0;
    notice.className = `pulse-notice${errors.length ? " pulse-offline" : ""}`;
    notice.textContent = errors.length
      ? `${errors.length} section${errors.length === 1 ? "" : "s"} unavailable. ${errors[0][0]}: ${errors[0][1]}`
      : "";

    const content = $("pulse-content");
    if (!state.pulseAccount) {
      const message = state.pulseAccountsError
        ? `Pulse is unavailable: ${state.pulseAccountsError}`
        : "Configure a Pulse account on this atmux server to populate the dashboard.";
      content.replaceChildren(pulseEmpty(message, Boolean(state.pulseAccountsError)));
      return;
    }
    const renderer = {
      dashboard: renderPulseDashboard,
      reports: renderPulseReports,
      alerts: renderPulseAlerts,
      settings: renderPulseSettings,
    }[state.pulseTab];
    content.replaceChildren(renderer());
  }

  function renderPulseDashboard() {
    const root = pulseNode("div", "pulse-stack pulse-dashboard");
    for (const section of [renderPulseOverview(), renderPulseReports(), renderPulseAlerts()]) {
      root.append(...section.childNodes);
    }
    return root;
  }

  function pulseWindowLabel(kind) {
    return ({
      five_hour: "5-hour quota",
      rolling_seven_day: "Rolling 7-day",
      fixed_weekly: "Weekly quota",
      monthly_budget: "Monthly budget",
    })[kind] || pulseLabel(kind);
  }

  function renderPulseQuotaCard(row, pace) {
    const card = pulseNode("article", "pulse-card pulse-quota-card");
    const title = pulseNode("header", "pulse-card-head");
    title.append(pulseNode("h3", "", pulseWindowLabel(row.window?.kind)));
    title.append(pulseNode("span", "pulse-chip", pulseLabel(row.vendor)));
    card.append(title);
    const detail = [
      pace?.band ? pulseLabel(pace.band) : null,
      row.window?.resets_at ? `resets ${pulseTime(row.window.resets_at)}` : null,
    ].filter(Boolean).join(" · ");
    card.append(pulseGauge(row.window?.used_percent, "Used", detail));
    const contributors = pulseNode("ul", "pulse-contributors");
    for (const item of row.contributors || []) {
      const contributor = pulseNode("li");
      contributor.append(pulseNode("strong", "", item.machine || "unknown machine"));
      const provenance = [
        item.chosen ? "account value" : "contributor",
        item.polled_at ? pulseTime(item.polled_at) : "unknown freshness",
        item.reporter_version ? `reporter ${item.reporter_version}` : "reporter version unavailable",
      ].join(" · ");
      contributor.append(pulseNode("span", "", provenance));
      contributors.append(contributor);
    }
    if (!contributors.childNodes.length) contributors.append(pulseNode("li", "meta", "No machine provenance reported."));
    card.append(contributors);
    return card;
  }

  function renderPulseOverview() {
    const root = pulseNode("div", "pulse-stack");
    const profiles = Array.isArray(state.pulseData.profiles) ? state.pulseData.profiles : [];
    const usage = Array.isArray(state.pulseData.usage) ? state.pulseData.usage : [];
    const pace = Array.isArray(state.pulseData.pace) ? state.pulseData.pace : [];
    const profileNames = [...new Set([...profiles.map((item) => item.name), ...usage.map((item) => item.profile)])].sort();
    const quota = pulseSection("Account quotas", `${profileNames.length} profile${profileNames.length === 1 ? "" : "s"}`);
    if (!profileNames.length) quota.append(pulseEmpty("No visible profiles or quota snapshots are available.", Boolean(state.pulseErrors.usage)));
    for (const name of profileNames) {
      const group = pulseNode("section", "pulse-profile-group");
      const configured = profiles.find((item) => item.name === name);
      const heading = pulseNode("header", "pulse-profile-head");
      heading.append(pulseNode("h3", "", name));
      heading.append(pulseNode("span", "meta", [configured?.vendor, configured?.origin].filter(Boolean).map(pulseLabel).join(" · ")));
      group.append(heading);
      const cards = pulseNode("div", "pulse-card-grid");
      const rows = usage.filter((item) => item.profile === name);
      for (const row of rows) {
        const matchingPace = pace.find((item) => item.profile === name && item.window === row.window?.kind);
        cards.append(renderPulseQuotaCard(row, matchingPace));
      }
      if (!rows.length) cards.append(pulseEmpty("Waiting for the first quota snapshot."));
      group.append(cards);
      quota.append(group);
    }
    root.append(quota);
    root.append(renderPulseGaugeHealth(), renderPulseContext(), renderPulseGemini(), renderPulseMachineHealth());
    return root;
  }

  function renderPulseGaugeHealth() {
    const rows = Array.isArray(state.pulseData.health) ? state.pulseData.health : [];
    const section = pulseSection("Collector health", `${rows.length} local profile${rows.length === 1 ? "" : "s"}`);
    const grid = pulseNode("div", "pulse-card-grid");
    const copy = {
      not_applicable: "This provider has no usage gauge.",
      dead_no_observation: "No collection observation has been stored.",
      authentication_failed: "The provider rejected authentication.",
      null_signal: "Collection ran but returned no usable gauge signal.",
      stale: "The last successful gauge is older than its cadence allows.",
      authenticated_unchanged: "Authentication works, but the gauge has remained unchanged across a full freshness window.",
      healthy: "The gauge is fresh and responding.",
    };
    for (const row of rows) {
      const card = pulseNode("article", `pulse-card pulse-health-${row.gauge || "unknown"}`);
      const head = pulseNode("header", "pulse-card-head");
      head.append(pulseNode("h3", "", row.profile || "Unknown profile"));
      head.append(pulseNode("span", "pulse-chip", pulseLabel(row.gauge)));
      card.append(head);
      card.append(pulseNode("p", "pulse-alert-message", copy[row.gauge] || "Collector health is unknown."));
      const credential = row.credential?.state || row.credential?.provider || row.credential || "unknown";
      card.append(pulseNode("p", "pulse-card-meta", `${pulseLabel(row.vendor)} · ${row.machine || "local"} · credentials ${pulseLabel(credential)} · ${row.last_polled_at ? `polled ${pulseTime(row.last_polled_at)}` : "never polled"}`));
      grid.append(card);
    }
    if (!rows.length) grid.append(pulseEmpty("No local collector diagnostics are available.", Boolean(state.pulseErrors.health)));
    section.append(grid);
    return section;
  }

  function renderPulseContext() {
    const sessions = Array.isArray(state.pulseData.context) ? state.pulseData.context : [];
    const section = pulseSection("Context sessions", `${sessions.length} active`);
    const grid = pulseNode("div", "pulse-card-grid pulse-context-grid");
    for (const row of sessions) {
      const session = row.session || {};
      const card = pulseNode("article", "pulse-card");
      const head = pulseNode("header", "pulse-card-head");
      head.append(pulseNode("h3", "", session.session_id || "Unknown session"));
      head.append(pulseNode("span", `pulse-chip pulse-${row.band || "unknown"}`, pulseLabel(row.band)));
      card.append(head);
      card.append(pulseGauge(session.context_percent, "Context", [session.profile, session.machine, session.model].filter(Boolean).join(" · ")));
      const compact = row.tokens_until_compact == null
        ? "Compact recommendation unavailable"
        : row.tokens_until_compact <= 0
          ? "Compact now"
          : `${Number(row.tokens_until_compact).toLocaleString()} tokens until compact`;
      card.append(pulseNode("p", "pulse-recommendation", compact));
      card.append(pulseNode("p", "pulse-card-meta", `active ${pulseTime(session.last_active_at)} · measured ${pulseTime(session.collected_at)}`));
      grid.append(card);
    }
    if (!sessions.length) grid.append(pulseEmpty("No context sessions have been collected.", Boolean(state.pulseErrors.context)));
    section.append(grid);
    return section;
  }

  function renderPulseGemini() {
    const buckets = Array.isArray(state.pulseData.gemini) ? state.pulseData.gemini : [];
    const section = pulseSection("Gemini buckets", `${buckets.length} model${buckets.length === 1 ? "" : "s"}`);
    const grid = pulseNode("div", "pulse-card-grid");
    for (const bucket of buckets) {
      const card = pulseNode("article", "pulse-card");
      card.append(pulseNode("h3", "", bucket.model_id || "Unknown model"));
      const remaining = pulsePercent(pulseNumber(bucket.remaining_fraction) * 100);
      card.append(pulseGauge(100 - remaining, "Consumed", `${remaining.toFixed(1)}% remaining${bucket.remaining_amount ? ` · ${bucket.remaining_amount}` : ""}`));
      card.append(pulseNode("p", "pulse-card-meta", `resets ${bucket.resets_at ? pulseTime(bucket.resets_at) : "not reported"} · measured ${pulseTime(bucket.collected_at)}`));
      grid.append(card);
    }
    if (!buckets.length) grid.append(pulseEmpty("No Gemini quota buckets have been collected.", Boolean(state.pulseErrors.gemini)));
    section.append(grid);
    return section;
  }

  function renderPulseMachineHealth() {
    const machines = Array.isArray(state.pulseData.machines) ? state.pulseData.machines : [];
    const limits = state.pulseData.limits;
    const section = pulseSection("Machines and receiver", limits?.capabilities?.receive ? "receiver enabled" : "receiver disabled");
    const grid = pulseNode("div", "pulse-machine-grid");
    for (const machine of machines) {
      const card = pulseNode("article", "pulse-card pulse-machine-card");
      card.append(pulseNode("h3", "", machine.name || "Unknown machine"));
      card.append(pulseNode("p", "pulse-card-meta", `last seen ${pulseTime(machine.last_seen)} · first seen ${pulseTime(machine.first_seen)}`));
      grid.append(card);
    }
    if (!machines.length) grid.append(pulseEmpty("No machine reporters are registered.", Boolean(state.pulseErrors.machines)));
    section.append(grid);
    return section;
  }

  function pulseSelect(name, choices, value) {
    const select = pulseNode("select");
    select.name = name;
    for (const choice of choices) {
      const optionNode = pulseNode("option", "", pulseLabel(choice));
      optionNode.value = choice;
      optionNode.selected = choice === value;
      select.append(optionNode);
    }
    return select;
  }

  function pulseField(label, control) {
    const wrapper = pulseNode("label", "pulse-field");
    wrapper.append(pulseNode("span", "", label), control);
    return wrapper;
  }

  function renderPulseReports() {
    const root = pulseNode("div", "pulse-stack");
    const controls = pulseNode("form", "pulse-toolbar pulse-report-controls");
    const days = pulseNode("input");
    days.name = "days"; days.type = "number"; days.min = "1"; days.max = "365"; days.required = true;
    days.value = String(state.pulseReport.days);
    const granularity = pulseSelect("granularity", ["daily", "weekly"], state.pulseReport.granularity);
    const drill = pulseSelect("drill", ["profile", "machine", "session", "model"], state.pulseReport.drill);
    const run = pulseNode("button", "subtle", "Run report");
    run.type = "submit";
    controls.append(pulseField("Days", days), pulseField("Group", granularity), pulseField("Drill", drill), run);
    controls.addEventListener("submit", (event) => {
      event.preventDefault();
      const requestedDays = Number(days.value);
      if (!Number.isInteger(requestedDays) || requestedDays < 1 || requestedDays > 365) {
        toast("Report days must be between 1 and 365");
        return;
      }
      state.pulseReport = { days: requestedDays, granularity: granularity.value, drill: drill.value };
      void refreshPulse(true);
    });
    root.append(controls);

    const report = state.pulseData.report;
    const section = pulseSection("Token and cost report", report?.range ? `${report.range.since_day} to ${report.range.through_day}` : "bounded to 365 days");
    if (!report) {
      section.append(pulseEmpty(state.pulseErrors.report ? "Report is currently unavailable." : "Run a report to inspect token and cost totals.", Boolean(state.pulseErrors.report)));
      root.append(section);
      return root;
    }
    section.append(pulseTotals(report.total));
    section.append(pulseNode("p", "pulse-card-meta", `${Number(report.rows_scanned || 0).toLocaleString()} stored rows · ${Number(report.fallback_priced_rows || 0).toLocaleString()} fallback-priced`));
    const list = pulseNode("div", "pulse-report-list");
    for (const profile of report.profiles || []) {
      const details = pulseNode("details", "pulse-report-detail");
      const summary = pulseNode("summary");
      summary.append(pulseNode("strong", "", profile.profile || "Unknown profile"));
      summary.append(pulseNode("span", "", `${Number(profile.total_tokens || 0).toLocaleString()} tokens · $${pulseNumber(profile.cost_usd).toFixed(2)}`));
      details.append(summary, pulseTotals(profile));
      const breakdown = pulseNode("div", "pulse-breakdown");
      const rows = [...(profile.by_period || []), ...(profile.by_machine || []), ...(profile.drill || [])].slice(0, 500);
      for (const row of rows) {
        const item = pulseNode("div", "pulse-breakdown-row");
        item.append(pulseNode("span", "", row.day || row.key || "Other"));
        item.append(pulseNode("span", "", `${Number(row.total_tokens || 0).toLocaleString()} · $${pulseNumber(row.cost_usd).toFixed(2)}`));
        breakdown.append(item);
      }
      if (!rows.length) breakdown.append(pulseEmpty("No drill-down rows in this range."));
      details.append(breakdown);
      list.append(details);
    }
    if (!report.profiles?.length) list.append(pulseEmpty("No token observations matched this report."));
    section.append(list);
    root.append(section);
    return root;
  }

  async function mutatePulse(path, options, successMessage) {
    const account = state.pulseAccount;
    const generation = state.pulseGeneration;
    if (!path || !account || state.pulseMutation) return false;
    const mutationId = ++state.pulseMutationId;
    state.pulseMutation = true;
    renderPulse();
    try {
      await request(path, options);
      if (!pulseRequestStillCurrent(account, state.pulseAccount, generation, state.pulseGeneration)) return false;
      if (successMessage) toast(successMessage);
      await refreshPulse(true);
      return true;
    } catch (error) {
      if (pulseRequestStillCurrent(account, state.pulseAccount, generation, state.pulseGeneration)) toast(error.message);
      return false;
    } finally {
      if (mutationId === state.pulseMutationId) {
        state.pulseMutation = false;
        renderPulse();
        flushPulseInvalidationRefresh();
      }
    }
  }

  function renderPulseAlerts() {
    const root = pulseNode("div", "pulse-stack");
    const alerts = Array.isArray(state.pulseData.alerts) ? state.pulseData.alerts : [];
    const section = pulseSection("Open alerts", `${alerts.length} unacknowledged`);
    const list = pulseNode("div", "pulse-alert-list");
    for (const event of alerts) {
      const card = pulseNode("article", "pulse-card pulse-alert-card");
      const head = pulseNode("header", "pulse-card-head");
      head.append(pulseNode("h3", "", pulseLabel(event.input?.alert_type)));
      head.append(pulseNode("span", "pulse-chip", event.input?.profile || "account"));
      card.append(head);
      card.append(pulseNode("p", "pulse-alert-message", event.input?.message || "Pulse alert"));
      card.append(pulseNode("p", "pulse-card-meta", `triggered ${pulseTime(event.input?.triggered_at)}`));
      const actions = pulseNode("div", "pulse-alert-actions");
      actions.append(pulseButton("Acknowledge", () => {
        void mutatePulse(pulseAlertActionPath(state.pulseAccount, event.id, "acknowledge"), { method: "POST" }, "Alert acknowledged");
      }));
      const replyForm = pulseNode("form", "pulse-reply-form");
      const reply = pulseNode("input");
      reply.name = "message";
      reply.maxLength = Number(state.pulseData.limits?.max_alert_reply_bytes) || 2_048;
      reply.placeholder = "Reply and acknowledge"; reply.required = true;
      const send = pulseNode("button", "subtle", "Reply"); send.type = "submit";
      replyForm.append(reply, send);
      replyForm.addEventListener("submit", (submitEvent) => {
        submitEvent.preventDefault();
        const message = reply.value.trim();
        if (!message) return;
        void mutatePulse(
          pulseAlertActionPath(state.pulseAccount, event.id, "reply"),
          { method: "POST", body: JSON.stringify({ message }) },
          "Reply saved and alert acknowledged",
        );
      });
      actions.append(replyForm);
      card.append(actions);
      list.append(card);
    }
    if (!alerts.length) list.append(pulseEmpty("No unacknowledged alerts.", Boolean(state.pulseErrors.alerts)));
    section.append(list);
    root.append(section, renderPulseSubscriptions());
    return root;
  }

  function renderPulseSubscriptions() {
    const subscriptions = Array.isArray(state.pulseData.subscriptions) ? state.pulseData.subscriptions : [];
    const section = pulseSection("Subscriptions", `${subscriptions.length} configured`);
    const form = pulseNode("form", "pulse-toolbar pulse-subscription-form");
    const profile = pulseNode("input"); profile.name = "profile"; profile.placeholder = "Profile"; profile.required = true; profile.maxLength = 128;
    const alertType = pulseSelect("alert_type", ["five_hour_threshold", "seven_day_threshold", "context_threshold", "auth_failure"], "five_hour_threshold");
    const threshold = pulseNode("input"); threshold.name = "threshold"; threshold.type = "number"; threshold.min = "0"; threshold.max = "100"; threshold.value = "80";
    const cooldown = pulseNode("input"); cooldown.name = "cooldown"; cooldown.type = "number"; cooldown.min = "1"; cooldown.max = "10080"; cooldown.value = "30";
    const delivery = pulseNode("select"); delivery.name = "delivery";
    const deliveryCapabilities = state.pulseData.limits?.delivery || {};
    for (const [value, label, disabled] of [
      ["none", "Pull only", false],
      ["pane", "Agent pane", !deliveryCapabilities.pane],
      ["channel", "Negotiated channel", !deliveryCapabilities.channel],
    ]) {
      const optionNode = pulseNode("option", "", label);
      optionNode.value = value; optionNode.disabled = disabled;
      delivery.append(optionNode);
    }
    const pane = pulseNode("select"); pane.name = "pane";
    for (const session of sortSessions([...state.sessions.values()]).filter((item) => isMachineControllable(machineOf(item)))) {
      const optionNode = pulseNode("option", "", `${session.name} · ${sessionMachineId(session)}`);
      optionNode.value = session.id;
      pane.append(optionNode);
    }
    const add = pulseNode("button", "subtle", "Add"); add.type = "submit";
    const paneField = pulseField("Pane", pane);
    form.append(pulseField("Profile", profile), pulseField("Alert", alertType), pulseField("Threshold %", threshold), pulseField("Cooldown min", cooldown), pulseField("Delivery", delivery), paneField, add);
    const updateThreshold = () => {
      const needed = alertType.value !== "auth_failure";
      threshold.disabled = !needed; threshold.required = needed;
      const paneOption = [...delivery.options].find((candidate) => candidate.value === "pane");
      if (paneOption) paneOption.disabled = !deliveryCapabilities.pane || !needed;
      if (!needed && delivery.value === "pane") delivery.value = "none";
      paneField.hidden = delivery.value !== "pane";
    };
    alertType.addEventListener("change", updateThreshold);
    delivery.addEventListener("change", updateThreshold);
    updateThreshold();
    form.addEventListener("submit", (event) => {
      event.preventDefault();
      const body = {
        profile: profile.value.trim(),
        alert_type: alertType.value,
        threshold: threshold.disabled ? null : Number(threshold.value),
        cooldown_minutes: Number(cooldown.value),
        delivery: delivery.value === "pane" ? { kind: "pane", pane_id: pane.value }
          : delivery.value === "channel" ? { kind: "channel" } : null,
        enabled: true,
      };
      void mutatePulse(pulseSubscriptionPath(state.pulseAccount), { method: "POST", body: JSON.stringify(body) }, "Subscription saved");
    });
    section.append(form);
    const list = pulseNode("div", "pulse-settings-list");
    for (const item of subscriptions) {
      const subscription = item.subscription || {};
      const row = pulseNode("article", "pulse-setting-row");
      const copy = pulseNode("div");
      copy.append(pulseNode("strong", "", `${subscription.profile || "Unknown"} · ${pulseLabel(subscription.alert_type)}`));
      const deliveryLabel = subscription.delivery?.kind === "pane"
        ? `pane ${subscription.delivery.pane_id || "unknown"}`
        : subscription.delivery?.kind === "channel" ? "channel" : "pull only";
      copy.append(pulseNode("span", "meta", `${subscription.threshold == null ? "event" : `${subscription.threshold}%`} · ${subscription.cooldown_minutes || 0} min cooldown · ${deliveryLabel}`));
      row.append(copy, pulseButton("Delete", () => {
        void mutatePulse(pulseSubscriptionPath(state.pulseAccount, item.id), { method: "DELETE" }, "Subscription removed");
      }, "danger"));
      list.append(row);
    }
    if (!subscriptions.length) list.append(pulseEmpty("No alert subscriptions configured."));
    section.append(list);
    return section;
  }

  function renderPulseSettings() {
    const root = pulseNode("div", "pulse-stack");
    root.append(renderPulseProfiles(), renderPulseReceiverTokens(), renderPulsePricing(), renderPulseMachineHealth(), renderPulseCapabilities());
    return root;
  }

  async function issuePulseReceiverToken(machine) {
    const account = state.pulseAccount;
    const generation = state.pulseGeneration;
    const path = pulseIngestTokenPath(account);
    if (!path || !account || state.pulseMutation) return;
    const mutationId = ++state.pulseMutationId;
    state.pulseMutation = true;
    renderPulse();
    try {
      const issued = await request(path, {
        method: "POST",
        body: JSON.stringify({ machine }),
      });
      if (!pulseRequestStillCurrent(account, state.pulseAccount, generation, state.pulseGeneration)) return;
      if (!issued?.token || !issued?.summary) throw new Error("Pulse returned an invalid token response");
      state.pulseIssuedToken = { account, machine, token: String(issued.token), id: issued.summary.id };
      state.pulseData.ingestTokens = await pulseFetchPage(account, "ingest-tokens");
      toast("Receiver token created — copy it now");
    } catch (error) {
      if (pulseRequestStillCurrent(account, state.pulseAccount, generation, state.pulseGeneration)) toast(error.message);
    } finally {
      if (mutationId === state.pulseMutationId) {
        state.pulseMutation = false;
        renderPulse();
      }
    }
  }

  function renderPulseReceiverTokens() {
    const tokens = Array.isArray(state.pulseData.ingestTokens) ? state.pulseData.ingestTokens : [];
    const receive = Boolean(state.pulseData.limits?.capabilities?.receive);
    const section = pulseSection("Receiver tokens", receive ? `${tokens.filter((token) => !token.revoked_at).length} active` : "receiver disabled");
    if (!receive) {
      section.append(pulseEmpty("Enable pulse.receive before registering remote reporters."));
      return section;
    }
    const form = pulseNode("form", "pulse-toolbar pulse-token-form");
    const machine = pulseNode("input");
    machine.name = "machine"; machine.placeholder = "Remote machine name"; machine.required = true; machine.maxLength = 255;
    const issue = pulseNode("button", "subtle", "Create token"); issue.type = "submit"; issue.disabled = state.pulseMutation;
    form.append(pulseField("Machine", machine), issue);
    form.addEventListener("submit", (event) => {
      event.preventDefault();
      const name = machine.value.trim();
      if (!name) return;
      void issuePulseReceiverToken(name);
    });
    section.append(form);

    const issued = state.pulseIssuedToken?.account === state.pulseAccount ? state.pulseIssuedToken : null;
    if (issued) {
      const oneTime = pulseNode("aside", "pulse-token-once");
      oneTime.append(pulseNode("strong", "", `Copy the ${issued.machine} token now. It cannot be shown again.`));
      const tokenRow = pulseNode("div", "pulse-token-copy");
      const value = pulseNode("input"); value.type = "text"; value.readOnly = true; value.value = issued.token; value.setAttribute("aria-label", "One-time receiver token");
      const copy = pulseNode("button", "subtle", "Copy"); copy.type = "button";
      copy.addEventListener("click", async () => {
        try {
          await navigator.clipboard.writeText(issued.token);
          value.select();
          toast("Receiver token copied");
        } catch { toast("Clipboard access failed; select and copy the token manually"); }
      });
      const dismiss = pulseNode("button", "subtle", "Dismiss"); dismiss.type = "button";
      dismiss.addEventListener("click", () => { state.pulseIssuedToken = null; renderPulse(); });
      tokenRow.append(value, copy, dismiss); oneTime.append(tokenRow); section.append(oneTime);
    }

    const list = pulseNode("div", "pulse-settings-list");
    for (const token of tokens) {
      const row = pulseNode("article", "pulse-setting-row");
      const copy = pulseNode("div");
      copy.append(pulseNode("strong", "", token.machine || "Unknown machine"));
      copy.append(pulseNode("span", "meta", token.revoked_at
        ? `revoked ${pulseTime(token.revoked_at)}`
        : `created ${pulseTime(token.created_at)} · ${token.last_used_at ? `last used ${pulseTime(token.last_used_at)}` : "never used"}`));
      row.append(copy);
      if (!token.revoked_at) row.append(pulseButton("Revoke", () => {
        void mutatePulse(pulseIngestTokenPath(state.pulseAccount, token.id), { method: "DELETE" }, "Receiver token revoked");
      }, "danger"));
      list.append(row);
    }
    if (!tokens.length) list.append(pulseEmpty("No receiver tokens have been issued.", Boolean(state.pulseErrors.ingestTokens)));
    section.append(list);
    return section;
  }

  function renderPulseProfiles() {
    const profiles = Array.isArray(state.pulseData.profiles) ? state.pulseData.profiles : [];
    const limits = state.pulseData.limits || {};
    const minimumPoll = Number(limits.min_profile_poll_minutes) || 5;
    const maximumPoll = Number(limits.max_profile_poll_minutes) || 10080;
    const section = pulseSection("Profile settings", `${profiles.length} profile${profiles.length === 1 ? "" : "s"}`);
    const list = pulseNode("div", "pulse-settings-list");
    for (const profile of profiles) {
      const row = pulseNode("article", "pulse-setting-row");
      const copy = pulseNode("div");
      copy.append(pulseNode("strong", "", profile.name || "Unknown profile"));
      copy.append(pulseNode("span", "meta", [profile.vendor, profile.origin, `${profile.poll_interval_minutes || 0}m poll`].map(pulseLabel).join(" · ")));
      const settings = pulseNode("form", "pulse-profile-settings");
      const poll = pulseNode("input"); poll.type = "number"; poll.min = String(minimumPoll); poll.max = String(maximumPoll); poll.step = "1"; poll.required = true; poll.value = String(profile.poll_interval_minutes || minimumPoll); poll.setAttribute("aria-label", `${profile.name} poll interval in minutes`);
      const budget = pulseNode("input"); budget.type = "number"; budget.min = "0.01"; budget.max = "1000000"; budget.step = "0.01"; budget.placeholder = "Budget USD"; budget.value = profile.monthly_budget_usd == null ? "" : String(profile.monthly_budget_usd); budget.setAttribute("aria-label", `${profile.name} monthly budget in USD`);
      const save = pulseNode("button", "subtle", "Save"); save.type = "submit";
      settings.append(poll, budget, save);
      settings.addEventListener("submit", (event) => {
        event.preventDefault();
        const body = {
          poll_interval_minutes: Number(poll.value),
          monthly_budget_usd: budget.value === "" ? null : Number(budget.value),
        };
        void mutatePulse(
          pulseProfileSettingsPath(state.pulseAccount, profile.name),
          { method: "PATCH", body: JSON.stringify(body) },
          `${profile.name} settings updated`,
        );
      });
      const label = pulseNode("label", "pulse-switch");
      const toggle = pulseNode("input"); toggle.type = "checkbox"; toggle.checked = !profile.hidden; toggle.disabled = state.pulseMutation;
      label.append(toggle, pulseNode("span", "", "Visible"));
      toggle.addEventListener("change", () => {
        void mutatePulse(
          pulseProfileVisibilityPath(state.pulseAccount, profile.name),
          { method: "PATCH", body: JSON.stringify({ hidden: !toggle.checked }) },
          `${profile.name} visibility updated`,
        );
      });
      const actions = pulseNode("div", "pulse-profile-actions");
      actions.append(settings, label);
      if (limits.force_poll_available && profile.origin === "local") {
        actions.append(pulseButton("Collect now", () => {
          void mutatePulse(
            pulseForcePollPath(state.pulseAccount),
            { method: "POST", body: JSON.stringify({ profile: profile.name }) },
            `${profile.name} collection queued on the existing scheduler`,
          );
        }, "subtle"));
      }
      row.append(copy, actions); list.append(row);
    }
    if (!profiles.length) list.append(pulseEmpty("No profiles are configured for this account.", Boolean(state.pulseErrors.profiles)));
    section.append(list);
    return section;
  }

  function renderPulsePricing() {
    const pricing = Array.isArray(state.pulseData.pricing) ? state.pulseData.pricing : [];
    const section = pulseSection("Pricing overrides", `${pricing.filter((item) => item.scope === "override").length} account override${pricing.filter((item) => item.scope === "override").length === 1 ? "" : "s"}`);
    const form = pulseNode("form", "pulse-toolbar pulse-pricing-form");
    const key = pulseNode("input"); key.name = "key"; key.placeholder = "Stable key"; key.required = true; key.maxLength = 128;
    const vendor = pulseSelect("vendor", ["anthropic-oauth", "openai-codex", "deepseek-balance", "xai-grok", "gemini", "antigravity"], "anthropic-oauth");
    const model = pulseNode("input"); model.name = "model"; model.placeholder = "Model pattern"; model.required = true; model.maxLength = 256;
    const inputCost = pulseNode("input"); inputCost.type = "number"; inputCost.min = "0"; inputCost.step = "0.0001"; inputCost.value = "0"; inputCost.required = true;
    const outputCost = pulseNode("input"); outputCost.type = "number"; outputCost.min = "0"; outputCost.step = "0.0001"; outputCost.value = "0"; outputCost.required = true;
    const cacheWrite5m = pulseNode("input"); cacheWrite5m.type = "number"; cacheWrite5m.min = "0"; cacheWrite5m.step = "0.0001"; cacheWrite5m.value = "0"; cacheWrite5m.required = true;
    const cacheWrite1h = pulseNode("input"); cacheWrite1h.type = "number"; cacheWrite1h.min = "0"; cacheWrite1h.step = "0.0001"; cacheWrite1h.value = "0"; cacheWrite1h.required = true;
    const cacheRead = pulseNode("input"); cacheRead.type = "number"; cacheRead.min = "0"; cacheRead.step = "0.0001"; cacheRead.value = "0"; cacheRead.required = true;
    const save = pulseNode("button", "subtle", "Save override"); save.type = "submit";
    form.append(
      pulseField("Key", key), pulseField("Vendor", vendor), pulseField("Model", model),
      pulseField("Input $/M", inputCost), pulseField("Output $/M", outputCost),
      pulseField("Cache write 5m $/M", cacheWrite5m), pulseField("Cache write 1h $/M", cacheWrite1h),
      pulseField("Cache read $/M", cacheRead), save,
    );
    form.addEventListener("submit", (event) => {
      event.preventDefault();
      const body = {
        key: key.value.trim(), vendor: vendor.value, model_pattern: model.value.trim(), settings_match: {},
        input_per_million_usd: Number(inputCost.value), output_per_million_usd: Number(outputCost.value),
        cache_write_5m_per_million_usd: Number(cacheWrite5m.value),
        cache_write_1h_per_million_usd: Number(cacheWrite1h.value),
        cache_read_per_million_usd: Number(cacheRead.value),
      };
      void mutatePulse(pulseAccountPath(state.pulseAccount, "pricing"), { method: "POST", body: JSON.stringify(body) }, "Pricing override saved");
    });
    section.append(form);
    const list = pulseNode("div", "pulse-settings-list");
    for (const item of pricing.slice(0, 400)) {
      const rule = item.rule || item;
      const row = pulseNode("article", "pulse-setting-row");
      const copy = pulseNode("div");
      copy.append(pulseNode("strong", "", `${rule.key || "rule"} · ${rule.model_pattern || "*"}`));
      copy.append(pulseNode("span", "meta", `${pulseLabel(item.scope)} · ${pulseLabel(rule.vendor)} · $${pulseNumber(rule.input_per_million_usd).toFixed(4)}/$${pulseNumber(rule.output_per_million_usd).toFixed(4)} per M`));
      row.append(copy);
      if (item.scope === "override") {
        row.append(pulseButton("Revert", () => {
          void mutatePulse(
            pulsePricingPath(state.pulseAccount, rule.key),
            { method: "DELETE" },
            `${rule.key} reverted to seeded pricing`,
          );
        }, "subtle"));
      }
      list.append(row);
    }
    if (!pricing.length) list.append(pulseEmpty("No pricing rules are available.", Boolean(state.pulseErrors.pricing)));
    section.append(list);
    return section;
  }

  function renderPulseCapabilities() {
    const limits = state.pulseData.limits;
    const section = pulseSection("Pulse settings", "server-enforced limits");
    if (!limits) {
      section.append(pulseEmpty("Capability and receiver settings are unavailable.", Boolean(state.pulseErrors.limits)));
      return section;
    }
    const list = pulseNode("dl", "pulse-capabilities");
    for (const [label, value] of [
      ["Collect", limits.capabilities?.collect ? "enabled" : "disabled"],
      ["Serve", limits.capabilities?.serve ? "enabled" : "disabled"],
      ["Receive", limits.capabilities?.receive ? "enabled" : "disabled"],
      ["Page limit", limits.max_page_size],
      ["Report days", limits.max_report_days],
      ["Force poll", limits.force_poll_available ? "available" : "not exposed"],
      ["Pane alerts", limits.delivery?.pane ? "available" : "unavailable"],
      ["Channel alerts", limits.delivery?.channel ? "connected" : "not negotiated"],
    ]) list.append(pulseNode("dt", "", label), pulseNode("dd", "", value));
    section.append(list);
    if (limits.force_poll_available) {
      section.append(pulseButton("Collect this account now", () => {
        void mutatePulse(
          pulseForcePollPath(state.pulseAccount),
          { method: "POST", body: JSON.stringify({}) },
          "Account collection queued on the existing scheduler",
        );
      }));
    }
    if (!limits.delivery?.channel) {
      section.append(pulseNode("p", "pulse-card-meta", "Channel delivery requires a live negotiated client capability. Pull-based alerts remain available; pane delivery is separately account/profile checked."));
    }
    return section;
  }

  function attachmentTargetLabel() {
    const target = state.sessions.get(state.attachmentPaneId);
    if (!target) return "These images belong to an unavailable agent. Clear them before sending.";
    const machine = machineOf(target);
    const label = `${target.name}${machine?.label ? ` on ${machine.label}` : ""}`;
    return attachmentsMatchCurrentSelection()
      ? `Sending to ${label}`
      : `Images belong to ${label}. Return to that agent or clear them before sending.`;
  }

  function attachmentsMatchCurrentSelection() {
    return attachmentSelectionMatches(
      state.attachmentPaneId,
      state.attachmentInstanceKey,
      state.selected,
      selectedComposerDraftIdentity()?.key,
    );
  }

  function renderAttachments() {
    const tray = $("attachment-tray");
    tray.hidden = state.attachments.length === 0;
    $("attachment-target").textContent = attachmentTargetLabel();
    const previews = state.attachments.map((attachment, index) => {
      const figure = document.createElement("figure");
      figure.className = "attachment-preview";
      const image = document.createElement("img");
      image.src = attachment.url;
      image.alt = attachment.file.name || `Image ${index + 1}`;
      const remove = document.createElement("button");
      remove.type = "button";
      remove.className = "attachment-remove";
      remove.disabled = state.composerSending;
      remove.setAttribute("aria-label", `Remove ${image.alt}`);
      remove.textContent = "×";
      remove.addEventListener("click", () => removeAttachment(index));
      figure.append(image, remove);
      return figure;
    });
    $("attachment-list").replaceChildren(...previews);
    $("attachment-clear").disabled = state.composerSending;
  }

  function clearAttachments() {
    if (state.composerSending) {
      toast("Wait for the current message to finish sending");
      return false;
    }
    for (const attachment of state.attachments) URL.revokeObjectURL?.(attachment.url);
    state.attachments = [];
    state.attachmentPaneId = null;
    state.attachmentInstanceKey = null;
    $("image-input").value = "";
    render();
    return true;
  }

  function removeAttachment(index) {
    if (state.composerSending) {
      toast("Wait for the current message to finish sending");
      return false;
    }
    const [removed] = state.attachments.splice(index, 1);
    if (removed) URL.revokeObjectURL?.(removed.url);
    if (!state.attachments.length) {
      state.attachmentPaneId = null;
      state.attachmentInstanceKey = null;
    }
    render();
    return Boolean(removed);
  }

  function removeDeliveredAttachments(delivered) {
    const remaining = remainingAttachmentsAfterDelivery(state.attachments, delivered);
    const retained = new Set(remaining);
    for (const attachment of delivered) {
      if (!retained.has(attachment)) URL.revokeObjectURL?.(attachment.url);
    }
    state.attachments = remaining;
    if (!remaining.length) {
      state.attachmentPaneId = null;
      state.attachmentInstanceKey = null;
    }
    $("image-input").value = "";
    render();
  }

  function addAttachmentFiles(files) {
    if (state.composerSending) {
      toast("Wait for the current message to finish sending");
      return false;
    }
    if (!state.selected) {
      toast("Select an agent before attaching an image");
      return false;
    }
    const selectedIdentity = selectedComposerDraftIdentity();
    if (!selectedIdentity?.persistent) {
      toast("This agent's identity is unavailable; reconnect before attaching images");
      return false;
    }
    if (state.attachments.length && !attachmentsMatchCurrentSelection()) {
      toast("These images belong to another agent; clear them before adding more");
      return false;
    }
    const selection = validateImageSelection(files, state.attachments);
    if (selection.error) {
      toast(selection.error);
      return false;
    }
    if (!state.attachmentPaneId) {
      state.attachmentPaneId = state.selected;
      state.attachmentInstanceKey = selectedIdentity.key;
    }
    state.attachments = state.attachments.concat(selection.files.map((file) => ({
      file,
      url: URL.createObjectURL(file),
    })));
    render();
    return true;
  }

  function rememberMessage(identity, message) {
    if (!identity) return;
    const history = state.messageHistory.get(identity.key) || [];
    if (history[history.length - 1] !== message) history.push(message);
    if (history.length > MAX_MESSAGE_HISTORY_ENTRIES) {
      history.splice(0, history.length - MAX_MESSAGE_HISTORY_ENTRIES);
    }
    state.messageHistory.set(identity.key, history);
    state.messageHistoryNavigation = null;
  }

  function browseMessageHistory(direction) {
    const identity = selectedComposerDraftIdentity();
    const input = $("message");
    if (!identity || input.disabled) return false;
    const history = state.messageHistory.get(identity.key) || [];
    const navigation = state.messageHistoryNavigation;
    const samePane = navigation?.draftKey === identity.key;
    const index = samePane ? navigation.index : history.length;
    const draft = samePane ? navigation.draft : input.value;
    const next = moveMessageHistory(history, index, direction);
    if (next === null) return false;
    state.messageHistoryNavigation = { draftKey: identity.key, index: next, draft };
    replaceComposerValue(next === history.length ? draft : history[next]);
    input.setSelectionRange(input.value.length, input.value.length);
    return true;
  }

  function handlesMessageHistoryKey(event, fromPane = false) {
    const input = $("message");
    const identity = selectedComposerDraftIdentity();
    const start = input.selectionStart ?? input.value.length;
    const direction = messageHistoryDirection(event, {
      value: input.value,
      selectionStart: start,
      selectionEnd: input.selectionEnd ?? start,
      browsing: Boolean(identity && state.messageHistoryNavigation?.draftKey === identity.key),
      fromPane,
    });
    if (!direction) return false;
    const handled = browseMessageHistory(direction);
    if (handled && fromPane) input.focus({ preventScroll: true });
    return handled;
  }

  $("filter").addEventListener("input", (event) => { state.filter = event.target.value; render(); });
  $("filter-status").addEventListener("change", (event) => { state.statusFilter = event.target.value; render(); });
  $("filter-harness").addEventListener("change", (event) => { state.harnessFilter = event.target.value; render(); });
  $("filter-clear").addEventListener("click", () => {
    state.filter = "";
    state.statusFilter = "";
    state.harnessFilter = "";
    for (const id of ["filter", "filter-status", "filter-harness"]) $(id).value = "";
    render();
    $("filter").focus({ preventScroll: true });
  });

  function saveNavigationPreferences() {
    const preferences = navigationPreferences({
      collapsed: [...state.collapsedMachines],
      favorites: [...state.favoriteSessions],
    });
    state.collapsedMachines = new Set(preferences.collapsed);
    state.favoriteSessions = new Set(preferences.favorites);
    if (!writeLocalStorage(NAVIGATION_STORAGE_KEY, JSON.stringify(preferences))) {
      toast("Navigation preferences apply for this page; browser storage is unavailable.");
    }
  }
  $("rail-toggle").addEventListener("click", () => setRailCollapsed(!state.railCollapsed));
  document.addEventListener("keydown", (event) => {
    const action = agentSearchShortcut(event, {
      dialogOpen: Boolean(document.querySelector("dialog[open]")),
      searchVisible: !mobileViewportActive() || !document.body.classList.contains("has-selection"),
    });
    if (!action) return;
    event.preventDefault();
    const filter = $("filter");
    if (action === "focus") {
      setRailCollapsed(false);
      filter.focus({ preventScroll: true });
      filter.select();
    } else if (action === "clear") {
      filter.value = "";
      state.filter = "";
      render();
    } else {
      filter.blur();
    }
  });
  $("overview-retry").addEventListener("click", () => {
    if (!$("overview-retry").disabled) connectOverview();
  });
  $("history-open").addEventListener("click", () => selectSessionHistory(!state.historyOpen));
  $("history-back").addEventListener("click", backToAgentMenu);
  $("history-refresh").addEventListener("click", () => void loadSessionHistory());
  $("history-more").addEventListener("click", () => void loadSessionHistory(true));
  $("history-filters").addEventListener("submit", (event) => { event.preventDefault(); void loadSessionHistory(); });
  $("pulse-open").addEventListener("click", () => selectPulse(!state.pulseOpen));
  function stopRecoveryPolling() {
    if (state.recoveryPoll !== null) clearTimeout(state.recoveryPoll);
    state.recoveryPoll = null;
  }
  /// Reads every machine's Quick Resume document through the coordinator.
  ///
  /// One request covers the fleet; each owning node reports only whether its
  /// own fixed roster is available and what state it is in.
  async function refreshFleetRecovery(showDialog = false) {
    stopRecoveryPolling();
    let entries = [];
    try {
      entries = await request("/api/v1/fleet/quick-resume");
    } catch (error) {
      if (showDialog) toast(error.message);
      scheduleRecoveryRefresh();
      return;
    }
    state.fleetRecovery = new Map(
      (Array.isArray(entries) ? entries : []).map((entry) => [entry.id, entry]),
    );
    if (showDialog && !$("recovery-dialog").open) $("recovery-dialog").showModal();
    renderRecoveryControl();
    scheduleRecoveryRefresh();
  }

  function scheduleRecoveryRefresh(delay = null) {
    stopRecoveryPolling();
    if (document.hidden) return;
    const wait = delay ?? recoveryPollDelay([...state.fleetRecovery.values()]);
    state.recoveryPoll = setTimeout(() => { void refreshFleetRecovery(false); }, wait);
  }

  /// Starts one machine's fixed roster script. The body is intentionally
  /// empty: the owning node runs only its own validated script.
  async function startRecovery(machineId) {
    if (!machineId || state.recoveryBusy.has(machineId)) return;
    state.recoveryBusy.add(machineId);
    renderRecoveryControl();
    try {
      const status = await request(`/api/v1/machines/${encodeURIComponent(machineId)}/quick-resume`, {
        method: "POST",
        body: JSON.stringify({}),
      });
      const existing = state.fleetRecovery.get(machineId) || { id: machineId, label: machineId, online: true };
      state.fleetRecovery.set(machineId, { ...existing, recovery: status, error: null });
      toast(`${existing.label || machineId} recovery started`);
    } catch (error) {
      toast(error.message);
    } finally {
      state.recoveryBusy.delete(machineId);
      renderRecoveryControl();
      scheduleRecoveryRefresh(1000);
    }
  }
  function stopFleetUpdatePolling() {
    if (state.fleetUpdatePoll !== null) clearTimeout(state.fleetUpdatePoll);
    state.fleetUpdatePoll = null;
  }

  /// Reads every machine's update document.
  ///
  /// One coordinator request covers the whole fleet, so the cadence is the
  /// fleet's, not one request per machine per tick.
  async function refreshFleetUpdates() {
    stopFleetUpdatePolling();
    let entries = [];
    try {
      entries = await request("/api/v1/fleet/updates");
    } catch {
      // A coordinator that cannot answer leaves the last roster in place; the
      // Software card already shows whatever each node last reported.
      scheduleFleetUpdates();
      return;
    }
    state.fleetUpdates = new Map(
      (Array.isArray(entries) ? entries : []).map((entry) => [entry.id, entry]),
    );
    scheduleFleetUpdates();
    render();
  }

  function scheduleFleetUpdates() {
    stopFleetUpdatePolling();
    if (document.hidden) return;
    const delay = fleetUpdatePollDelay([...state.fleetUpdates.values()]);
    state.fleetUpdatePoll = setTimeout(() => { void refreshFleetUpdates(); }, delay);
  }

  /// Sends one fixed verb to one machine and repaints from the answer.
  async function runMachineUpdate(machineId, action) {
    if (state.updateBusy.has(machineId)) return false;
    state.updateBusy.add(machineId);
    render();
    try {
      const status = await request(
        `/api/v1/machines/${encodeURIComponent(machineId)}/update/${action}`,
        { method: "POST", body: JSON.stringify({}) },
      );
      const existing = state.fleetUpdates.get(machineId);
      state.fleetUpdates.set(machineId, {
        id: machineId,
        label: existing?.label || machineId,
        online: true,
        update: status,
        error: null,
      });
      return true;
    } catch (error) {
      toast(error.message);
      return false;
    } finally {
      state.updateBusy.delete(machineId);
      scheduleFleetUpdates();
      render();
    }
  }

  function machineLabelFor(machineId) {
    return state.fleetUpdates.get(machineId)?.label
      || state.machines.find((machine) => machine.id === machineId)?.label
      || machineId;
  }

  /// Nothing that restarts a node happens without an explicit confirmation
  /// naming the machines it will restart.
  function openUpdateConfirm(machineIds, action) {
    if (!machineIds.length) return;
    state.pendingUpdate = { action, machines: machineIds };
    const copy = updateConfirmCopy(action, machineIds.map(machineLabelFor));
    $("update-dialog-title").textContent = copy.title;
    $("update-dialog-target").textContent = copy.target;
    $("update-dialog-note").textContent = copy.note;
    $("update-confirm").textContent = copy.confirm;
    const dialog = $("update-dialog");
    if (!dialog.open) dialog.showModal();
  }

  $("update-dialog").addEventListener("close", () => { state.pendingUpdate = null; });
  $("update-all-open").addEventListener("click", () => {
    openUpdateConfirm(
      updatableMachines([...state.fleetUpdates.values()]).map((entry) => entry.id),
      "apply",
    );
  });
  $("update-confirm").addEventListener("click", async () => {
    const pending = state.pendingUpdate;
    $("update-dialog").close();
    if (!pending?.machines.length) return;
    const results = await Promise.all(
      pending.machines.map((id) => runMachineUpdate(id, pending.action)),
    );
    const started = results.filter(Boolean).length;
    if (!started) return;
    toast(pending.action === "rollback"
      ? `Rolling back ${started} machine${started === 1 ? "" : "s"}`
      : `Updating ${started} machine${started === 1 ? "" : "s"}`);
  });
  $("recovery-open").addEventListener("click", () => { void refreshFleetRecovery(true); });
  $("pulse-mobile-back").addEventListener("click", backToAgentMenu);
  $("pulse-refresh").addEventListener("click", () => { void refreshPulse(true); });
  $("pulse-account").addEventListener("change", (event) => { setPulseAccount(event.target.value); });
  document.querySelectorAll("[data-pulse-tab]").forEach((button) => button.addEventListener("click", () => {
    const tab = button.dataset.pulseTab;
    if (!new Set(["dashboard", "reports", "alerts", "settings"]).has(tab) || tab === state.pulseTab) return;
    state.pulseGeneration += 1;
    state.pulseTab = tab;
    if (tab !== "settings") state.pulseIssuedToken = null;
    state.pulseErrors = {};
    void refreshPulse(true);
    renderPulse();
  }));
  $("conversation-view").addEventListener("click", () => setViewMode("conversation"));
  $("raw-view").addEventListener("click", () => setViewMode("raw"));
  $("files-view").addEventListener("click", () => setViewMode("files"));
  $("file-viewer").addEventListener("keydown", handleFileViewerKeydown);
  document.addEventListener("keydown", (event) => {
    if (event.key === "Control" || event.key === "Meta") setNavigationModifier(true);
  });
  document.addEventListener("keyup", (event) => {
    if (event.key === "Control" || event.key === "Meta") setNavigationModifier(false);
  });
  window.addEventListener("blur", () => setNavigationModifier(false));
  $("git-view").addEventListener("click", () => setViewMode("git"));
  $("conversation-filters-open").addEventListener("click", () => {
    const dialog = $("conversation-filters-dialog");
    if (!dialog.open) {
      dialog.showModal();
      $("conversation-filters-open").setAttribute("aria-expanded", "true");
    }
  });
  $("conversation-filters-dialog").addEventListener("close", () => {
    $("conversation-filters-open").setAttribute("aria-expanded", "false");
  });
  for (const [id, key] of [["conversation-show-human", "human"], ["conversation-show-internal", "internal"]]) {
    $(id).addEventListener("change", (event) => {
      setConversationVisibility({
        ...state.conversationVisibility,
        [key]: event.currentTarget.checked,
      });
    });
  }
  $("conversation-filters-reset").addEventListener("click", () => {
    setConversationVisibility({ human: true, internal: true });
  });
  document.querySelector(".view-switch").addEventListener("keydown", (event) => {
    if (!["ArrowLeft", "ArrowRight", "Home", "End"].includes(event.key)) return;
    const modes = ["conversation", "raw", "files", "git"];
    const current = Math.max(0, modes.indexOf(state.viewMode));
    const index = event.key === "Home" ? 0
      : event.key === "End" ? modes.length - 1
        : (current + (event.key === "ArrowRight" ? 1 : -1) + modes.length) % modes.length;
    event.preventDefault();
    if (setViewMode(modes[index]) !== false) {
      $(`${modes[index]}-view`).focus({ preventScroll: true });
    }
  });
  $("mobile-back").addEventListener("click", backToAgentMenu);
  $("machine-mobile-back").addEventListener("click", backToAgentMenu);

  function composerDraftIdentityForPane(paneId) {
    const session = state.sessions.get(paneId);
    const localMachineId = state.machines.find((machine) => machine.kind === "local")?.id || "local";
    return composerDraftIdentity(session, localMachineId);
  }

  function selectedComposerDraftIdentity() {
    return composerDraftIdentityForPane(state.selected);
  }

  function composerTargetMatches(paneId, identityKey) {
    const localMachineId = state.machines.find((machine) => machine.kind === "local")?.id || "local";
    return sessionMatchesComposerIdentity(state.sessions.get(paneId), identityKey, localMachineId);
  }

  function protectedComposerDraftKeys() {
    const keys = new Set(state.optimisticComposerClears.keys());
    if (state.composerDraftIdentity) keys.add(state.composerDraftIdentity.key);
    if (state.inFlightComposerIdentity) keys.add(state.inFlightComposerIdentity);
    for (const queued of state.queuedComposerMessages) {
      const key = queued.options?.composerSubmission?.draftIdentity?.key;
      if (key) keys.add(key);
    }
    return keys;
  }

  function nextComposerDraftTimestamp() {
    state.composerDraftTimestamp = Math.max(state.composerDraftTimestamp + 1, Date.now());
    return state.composerDraftTimestamp;
  }

  function syncComposerDraftTimestamp() {
    for (const draft of state.composerDrafts.values()) {
      state.composerDraftTimestamp = Math.max(state.composerDraftTimestamp, draft.updatedAt);
    }
    for (const tombstone of state.composerDraftTombstones.values()) {
      state.composerDraftTimestamp = Math.max(state.composerDraftTimestamp, tombstone.deletedAt);
    }
  }

  function recordComposerDraftTombstone(identity) {
    if (!identity?.persistent) return false;
    const deletedAt = nextComposerDraftTimestamp();
    state.composerDraftTombstones.delete(identity.key);
    state.composerDraftTombstones.set(identity.key, { deletedAt });
    while (state.composerDraftTombstones.size > MAX_COMPOSER_DRAFT_TOMBSTONES) {
      state.composerDraftTombstones.delete(state.composerDraftTombstones.keys().next().value);
    }
    return true;
  }

  function saveComposerDraftStorage(immediate = false) {
    if (state.composerDraftStorageTimer !== null) clearTimeout(state.composerDraftStorageTimer);
    state.composerDraftStorageTimer = null;
    const write = () => {
      state.composerDraftStorageTimer = null;
      mergeComposerDraftState(
        state.composerDrafts,
        state.composerDraftTombstones,
        readLocalStorage(COMPOSER_DRAFT_STORAGE_KEY),
        Date.now(),
        protectedComposerDraftKeys(),
      );
      syncComposerDraftTimestamp();
      writeLocalStorage(
        COMPOSER_DRAFT_STORAGE_KEY,
        composerDraftJson(
          state.composerDrafts,
          protectedComposerDraftKeys(),
          state.composerDraftTombstones,
        ),
      );
    };
    if (immediate) write();
    else state.composerDraftStorageTimer = setTimeout(write, 250);
  }

  function composerClearRevisionIsPending(identity, revision) {
    return Boolean(identity)
      && state.optimisticComposerClears.get(identity.key)?.has(revision);
  }

  function persistBoundComposerDraft(flush = false) {
    const identity = state.composerDraftIdentity;
    if (!identity) return null;
    const input = $("message");
    if (!input.value) {
      if (composerClearRevisionIsPending(identity, state.composerRevision)) {
        if (flush && identity.persistent) saveComposerDraftStorage(true);
        return state.composerDrafts.get(identity.key) || null;
      }
      const removed = state.composerDrafts.delete(identity.key);
      if (removed && identity.persistent) {
        recordComposerDraftTombstone(identity);
        saveComposerDraftStorage(flush);
      }
      return null;
    }
    const selectionStart = input.selectionStart ?? input.value.length;
    const selectionEnd = input.selectionEnd ?? selectionStart;
    const existing = state.composerDrafts.get(identity.key);
    const textChanged = existing?.text !== input.value;
    const draft = {
      text: input.value,
      selectionStart,
      selectionEnd,
      version: textChanged ? ++state.composerDraftSequence : existing.version,
      updatedAt: textChanged ? nextComposerDraftTimestamp() : existing.updatedAt,
    };
    if (textChanged) {
      state.composerDrafts.delete(identity.key);
      state.composerDraftTombstones.delete(identity.key);
    }
    state.composerDrafts.set(identity.key, draft);
    pruneComposerDraftEntries(state.composerDrafts, protectedComposerDraftKeys());
    if (identity.persistent) saveComposerDraftStorage(flush);
    return draft;
  }

  function bindComposerDraftToSelection() {
    const nextIdentity = selectedComposerDraftIdentity();
    if (state.composerDraftIdentity?.key === nextIdentity?.key) return;
    persistBoundComposerDraft(true);
    state.composerDraftIdentity = nextIdentity;
    state.messageHistoryNavigation = null;
    const input = $("message");
    const draft = nextIdentity ? state.composerDrafts.get(nextIdentity.key) : null;
    input.value = draft?.text || "";
    state.composerRevision += 1;
    if (draft) {
      try { input.setSelectionRange(draft.selectionStart, draft.selectionEnd); }
      catch { /* An unfocused mobile textarea can reject selection updates. */ }
    }
  }

  function forgetComposerDraft(identity, detach = false, save = true) {
    if (!identity) return false;
    const removed = state.composerDrafts.delete(identity.key);
    const tombstoned = removed && recordComposerDraftTombstone(identity);
    // A delivered message clears its draft, not the sent-message history.
    // Only retiring the pane (removal/replacement) discards both.
    if (detach) {
      state.messageHistory.delete(identity.key);
      if (state.messageHistoryNavigation?.draftKey === identity.key) {
        state.messageHistoryNavigation = null;
      }
    }
    state.optimisticComposerClears.delete(identity.key);
    if (detach && state.composerDraftIdentity?.key === identity.key) {
      state.composerDraftIdentity = null;
      replaceComposerValue("", false);
    }
    if ((removed || tombstoned) && identity.persistent && save) saveComposerDraftStorage(true);
    return removed || tombstoned;
  }

  function captureComposerDraftSubmission(paneId, message) {
    const identity = composerDraftIdentityForPane(paneId);
    if (!identity) return { draftIdentity: null, draftVersion: null };
    if (state.composerDraftIdentity?.key === identity.key && $("message").value === message) {
      persistBoundComposerDraft();
    }
    const draft = state.composerDrafts.get(identity.key);
    return {
      draftIdentity: identity,
      draftVersion: draft?.text === message ? draft.version : null,
    };
  }

  function finishComposerDraftSubmission(submission) {
    const identity = submission?.draftIdentity;
    if (!identity) return false;
    const pending = state.optimisticComposerClears.get(identity.key);
    if (submission.clearedRevision !== null) {
      pending?.delete(submission.clearedRevision);
      if (!pending?.size) state.optimisticComposerClears.delete(identity.key);
    }
    const draft = state.composerDrafts.get(identity.key);
    if (!composerDraftCanClear(draft, submission)) return false;
    forgetComposerDraft(identity);
    if (state.composerDraftIdentity?.key === identity.key
        && $("message").value === submission.message) {
      replaceComposerValue("", false);
    }
    return true;
  }

  function replaceComposerValue(value, persist = true) {
    const input = $("message");
    const next = String(value);
    if (input.value === next) return state.composerRevision;
    input.value = next;
    state.composerRevision += 1;
    if (persist) persistBoundComposerDraft();
    return state.composerRevision;
  }

  function acceptComposerSubmission(paneId, message) {
    const submission = {
      paneId,
      message,
      clearedRevision: null,
      ...captureComposerDraftSubmission(paneId, message),
    };
    const input = $("message");
    if (composerSubmissionMatches(state.selected, paneId, input.value, message)) {
      submission.clearedRevision = replaceComposerValue("", false);
      if (submission.draftIdentity) {
        const revisions = state.optimisticComposerClears.get(submission.draftIdentity.key) || new Set();
        revisions.add(submission.clearedRevision);
        state.optimisticComposerClears.set(submission.draftIdentity.key, revisions);
      }
    }
    return submission;
  }

  function restoreComposerSubmission(submission) {
    if (!submission || submission.clearedRevision === null) return false;
    const canRestore = composerSubmissionCanRestore(
      state.selected,
      $("message").value,
      state.composerRevision,
      submission,
    ) && composerTargetMatches(submission.paneId, submission.draftIdentity?.key);
    // Consume the rollback token even if newer composer activity made it stale.
    const pending = submission.draftIdentity
      ? state.optimisticComposerClears.get(submission.draftIdentity.key) : null;
    pending?.delete(submission.clearedRevision);
    if (submission.draftIdentity && !pending?.size) {
      state.optimisticComposerClears.delete(submission.draftIdentity.key);
    }
    submission.clearedRevision = null;
    if (!canRestore) return false;
    const input = $("message");
    replaceComposerValue(submission.message, false);
    input.setSelectionRange(input.value.length, input.value.length);
    return true;
  }

  function drainQueuedComposerMessage() {
    const queued = state.queuedComposerMessages.shift();
    if (!queued) return;
    void sendComposerMessage(queued.paneId, queued.message, {
      ...queued.options,
      fromQueue: true,
    }).then(queued.resolve);
  }

  async function sendComposerMessage(paneId = state.selected, messageOverride = null, options = {}) {
    const input = $("message");
    const abortStaleTarget = (submission = options.composerSubmission || null) => {
      restoreComposerSubmission(submission);
      toast("Agent restarted before this message could be sent. Review the preserved draft and try again.");
      if (options.fromQueue === true) drainQueuedComposerMessage();
      return false;
    };
    if (!paneId || (messageOverride === null && input.disabled)) {
      if (options.fromQueue === true) drainQueuedComposerMessage();
      return false;
    }
    const message = messageOverride === null ? input.value : String(messageOverride);
    const attachments = messageOverride === null ? [...state.attachments] : [];
    if (attachments.length && !attachmentSelectionMatches(
      state.attachmentPaneId,
      state.attachmentInstanceKey,
      paneId,
      composerDraftIdentityForPane(paneId)?.key,
    )) {
      toast("These images belong to another agent. Return to that agent or clear them before sending.");
      return false;
    }
    const targetPaneId = attachments.length
      ? attachmentDeliveryTarget(state.attachmentPaneId, paneId)
      : paneId;
    const targetIdentityKey = options.targetIdentityKey
      || options.composerSubmission?.draftIdentity?.key
      || (attachments.length ? state.attachmentInstanceKey : composerDraftIdentityForPane(targetPaneId)?.key);
    if (!message.trim() && !attachments.length) {
      if (options.fromQueue === true) drainQueuedComposerMessage();
      return false;
    }
    if (!composerTargetMatches(targetPaneId, targetIdentityKey)) return abortStaleTarget();
    const targetInstanceId = composerDraftInstanceId(targetIdentityKey);
    const messageLimit = attachments.length ? MAX_MESSAGE_BYTES - IMAGE_MESSAGE_TEXT_RESERVE : MAX_MESSAGE_BYTES;
    if (utf8ByteLength(message) > messageLimit) {
      toast("Message exceeds the 64 KiB UTF-8 limit");
      if (options.fromQueue === true) drainQueuedComposerMessage();
      return false;
    }
    const clearOnAccept = options.clearOnAccept === true;
    let composerSubmission = options.composerSubmission || null;
    const markAccepted = () => {
      if (composerSubmission) return;
      composerSubmission = clearOnAccept
        ? acceptComposerSubmission(targetPaneId, message)
        : {
          paneId: targetPaneId,
          message,
          clearedRevision: null,
          ...captureComposerDraftSubmission(targetPaneId, message),
        };
    };
    if (state.composerSending) {
      if (messageOverride === null) return false;
      if (state.queuedComposerMessages.length >= MAX_QUEUED_COMPOSER_MESSAGES) {
        toast("Quick Talk queue is full; wait for the current send");
        return false;
      }
      markAccepted();
      toast("Quick Talk queued behind the current send");
      return new Promise((resolve) => {
        state.queuedComposerMessages.push({
          paneId,
          message,
          options: {
            clearOnAccept,
            composerSubmission,
            targetIdentityKey,
            fromQueue: options.fromQueue === true,
          },
          resolve,
        });
      });
    }
    markAccepted();
    if (composerSubmission?.draftIdentity?.key !== targetIdentityKey
        || !composerTargetMatches(targetPaneId, targetIdentityKey)) {
      return abortStaleTarget(composerSubmission);
    }
    const button = $("send");
    state.composerSending = true;
    state.inFlightComposerText = message;
    state.inFlightComposerIdentity = composerSubmission?.draftIdentity?.key || null;
    button.disabled = true;
    render();
    try {
      if (attachments.length) {
        const images = await Promise.all(attachments.map(async ({ file }) => ({
          media_type: file.type,
          data: arrayBufferToBase64(await file.arrayBuffer()),
        })));
        if (!composerTargetMatches(targetPaneId, targetIdentityKey)) {
          throw new Error("Agent restarted while images were being prepared. Images were kept; return to the original agent or clear them.");
        }
        await request(`/api/v1/panes/${encodeURIComponent(targetPaneId)}/image-messages`, {
          method: "POST",
          body: JSON.stringify({ text: message, images, instance_id: targetInstanceId }),
        });
      } else {
        if (!composerTargetMatches(targetPaneId, targetIdentityKey)) {
          throw new Error("Agent restarted before this message could be sent. The draft was kept.");
        }
        await request(`/api/v1/panes/${encodeURIComponent(targetPaneId)}/messages`, {
          method: "POST",
          body: JSON.stringify({ text: message, submit: true, instance_id: targetInstanceId }),
        });
      }
      if (message.trim()) rememberMessage(composerSubmission?.draftIdentity, message);
      finishComposerDraftSubmission(composerSubmission);
      if (attachments.length) removeDeliveredAttachments(attachments);
      toast(attachments.length === 1 ? "Image sent" : attachments.length > 1 ? "Images sent" : "Message sent");
      return true;
    } catch (error) {
      restoreComposerSubmission(composerSubmission);
      toast(error.message);
    }
    finally {
      state.composerSending = false;
      state.inFlightComposerText = null;
      state.inFlightComposerIdentity = null;
      render();
      drainQueuedComposerMessage();
    }
    return false;
  }

  $("composer").addEventListener("submit", async (event) => {
    event.preventDefault(); await sendComposerMessage();
  });
  $("send").addEventListener("click", () => { void sendComposerMessage(); });
  $("attach").addEventListener("click", () => $("image-input").click());
  $("image-input").addEventListener("change", (event) => {
    addAttachmentFiles(event.target.files);
    event.target.value = "";
  });
  $("attachment-clear").addEventListener("click", clearAttachments);
  /// Sends one control's change on its own. The request names only that
  /// control, so the harness keeps the model, effort, or fast mode it omits.
  ///
  /// The owner answers with what it observed once the switch settled, so the
  /// picker repaints from the switch itself. Reading `/models` back instead
  /// would race the ~750 ms pane poll and redisplay the pre-switch controls
  /// until the next reload.
  async function switchAgentModel(change, label, warning = "") {
    const paneId = state.selected;
    const sessionName = state.sessions.get(paneId)?.name || paneId;
    if (!paneId || state.modelSwitchingPaneId) return;
    const before = paneModelSignature(state.paneModels);
    state.modelSwitchingPaneId = paneId;
    render();
    let adopted = false;
    try {
      const settled = await request(`/api/v1/panes/${encodeURIComponent(paneId)}/model`, {
        method: "POST",
        body: JSON.stringify(change),
      });
      adopted = adoptPaneModels(paneId, settled);
      toast(`Switched ${sessionName} to ${label}.${warning}`);
    } catch (error) {
      toast(error.message);
    } finally {
      state.modelSwitchingPaneId = null;
      if (state.selected === paneId && !adopted) await refreshModels(paneId);
      render();
    }
    if (adopted && paneModelSignature(state.paneModels) === before) {
      await settlePaneModels(paneId, before);
    }
  }
  function switchAgentModelChoice(model) {
    if (!model || state.paneModels?.current === model) return;
    void switchAgentModel({ model }, model);
  }
  function switchAgentEffort(effort) {
    if (!effort || state.paneModels?.effort === effort) return;
    const warning = state.sessions.get(state.selected)?.agent === "claude"
      ? " Claude saves this effort as the profile default."
      : "";
    void switchAgentModel({ effort }, `${effort} effort`, warning);
  }
  function switchAgentFast(fast) {
    if (state.paneModels?.fast === fast) return;
    void switchAgentModel({ fast }, fast ? "fast mode on" : "fast mode off");
  }
  $("agent-model").addEventListener("change", (event) => { switchAgentModelChoice(event.currentTarget.value); });
  $("quick-agent-model").addEventListener("change", (event) => { switchAgentModelChoice(event.currentTarget.value); });
  $("agent-effort").addEventListener("change", (event) => { switchAgentEffort(event.currentTarget.value); });
  $("quick-agent-effort").addEventListener("change", (event) => { switchAgentEffort(event.currentTarget.value); });
  $("agent-fast").addEventListener("change", (event) => { switchAgentFast(event.currentTarget.checked); });
  $("quick-agent-fast").addEventListener("change", (event) => { switchAgentFast(event.currentTarget.checked); });
  $("quick-actions-open").addEventListener("click", () => {
    const dialog = $("quick-actions-dialog");
    if (!dialog.open) {
      const paneId = state.selected;
      // Readiness and the native process token may have changed since the
      // pane opened. Keep restart unavailable until this fresh read settles.
      state.paneModels = null;
      render();
      if (paneId) void refreshModels(paneId);
      $("quick-copy-link-status").hidden = true;
      dialog.showModal();
      $("quick-actions-open").setAttribute("aria-expanded", "true");
    }
  });
  $("quick-actions-dialog").addEventListener("close", () => {
    $("quick-actions-open").setAttribute("aria-expanded", "false");
  });
  $("quick-copy-link").addEventListener("click", async () => {
    const session = state.sessions.get(state.selected);
    if (!session) return;
    const button = $("quick-copy-link");
    const status = $("quick-copy-link-status");
    button.disabled = true;
    status.hidden = false;
    status.textContent = "Copying link…";
    try {
      await copySelectedAgentLink(location.href, session.id, navigator.clipboard);
      status.textContent = `Link copied for ${session.name}.`;
    } catch (error) {
      status.textContent = error.message;
    } finally {
      button.disabled = false;
    }
  });
  $("quick-duplicate").addEventListener("click", () => {
    const session = state.sessions.get(state.selected);
    if (!session || state.duplicatingPaneId) return;
    state.duplicatingPaneId = session.id;
    $("quick-actions-dialog").close();
    render();
    void openLaunchDialog(session)
      .catch((error) => {
        invalidateLaunchDialog(false);
        toast(`Could not duplicate agent: ${error.message}`);
      })
      .finally(() => {
        state.duplicatingPaneId = null;
        render();
      });
  });
  $("quick-compact").addEventListener("click", () => {
    if ($("quick-compact").disabled) return;
    $("quick-actions-dialog").close();
    void compactSelectedAgent();
  });
  $("quick-download-output").addEventListener("click", () => {
    const selected = state.sessions.get(state.selected);
    const snapshot = paneOutputMatchesSession(state.paneOutputBinding, selected)
      ? paneOutputDownload(selected, state.paneLines) : null;
    if (!snapshot) { toast("No raw output is available yet"); return; }
    const url = URL.createObjectURL(new Blob([snapshot.content], { type: "text/plain;charset=utf-8" }));
    const link = document.createElement("a");
    link.href = url;
    link.download = snapshot.filename;
    document.body.append(link);
    link.click();
    link.remove();
    setTimeout(() => URL.revokeObjectURL(url), 30_000);
    $("quick-actions-dialog").close();
    toast("Raw output download started");
  });
  function openRestartDialog() {
    const session = state.sessions.get(state.selected);
    const view = agentRestartState(
      session,
      state.paneModels,
      isMachineControllable(machineOf(session)),
      state.resumingPaneId,
      state.composerSending,
    );
    if (!session || !view.available || view.disabled) {
      toast(view.status || "Session restart is unavailable");
      return;
    }
    state.pendingResumeId = agentRestartRequest(session, state.paneModels);
    if (!state.pendingResumeId) {
      toast("Refresh this agent before restarting; its process identity is unavailable");
      return;
    }
    $("quick-actions-dialog").close();
    $("resume-dialog").showModal();
  }
  async function refreshRegistryCapability() {
    try { await request("/api/v1/session-history?limit=1"); state.registryEnabled = true; render(); }
    catch { state.registryEnabled = false; }
  }
  function openResumeOn(session) {
    if (!state.registryEnabled || !sessionResumeIntent(session, "local")) return;
    const machines = resumeMachineOptions(state.machines);
    if (!machines.length) { toast("No machine is available to resume this session"); return; }
    state.pendingResumeOn = { session_key: session.session_key, name: session.name, machine: session.machine };
    $("quick-actions-dialog").close();
    $("resume-on-machine").replaceChildren(...machines.map((machine) => {
      const option = document.createElement("option"); option.value = machine.id; option.textContent = machine.label || machine.id;
      return option;
    }));
    $("resume-on-machine").value = machines.find((machine) => machine.id !== session.machine)?.id || machines[0].id;
    $("resume-on-session").textContent = session.name;
    $("resume-on-move").checked = false;
    $("resume-on-error").hidden = true;
    $("resume-on-confirm").disabled = false;
    $("resume-on-dialog").showModal();
  }
  $("quick-resume-on").addEventListener("click", () => openResumeOn(state.sessions.get(state.selected)));
  window.atmuxSessionResume = (request) => {
    state.registryEnabled = true;
    const row = state.historyRows.find((row) => row.sessionKey === request.session_key);
    openResumeOn({session_key:request.session_key,name:row?.name || request.session_key,machine:row?.machine});
  };
  document.addEventListener("atmux:session-resume", (event) => openResumeOn(event.detail));
  $("resume-on-dialog").addEventListener("close", () => { state.pendingResumeOn = null; });
  $("resume-on-form").addEventListener("submit", async (event) => {
    event.preventDefault();
    if (state.resumingOn || !state.pendingResumeOn) return;
    const session = state.pendingResumeOn;
    const machine = $("resume-on-machine").value;
    if (!resumeMachineOptions(state.machines).some((candidate) => candidate.id === machine)) {
      $("resume-on-error").textContent = "The target machine is offline. Choose another machine.";
      $("resume-on-error").hidden = false; return;
    }
    const body = sessionResumeIntent(session, machine, $("resume-on-move").checked);
    if (!body) return;
    state.resumingOn = true; $("resume-on-confirm").disabled = true;
    try {
      const result = await request("/api/v1/registry/resume", { method: "POST", body: JSON.stringify(body) });
      if (!result.verified || result.session_key !== body.session_key || result.machine !== body.machine) throw new Error("The target could not verify this session");
      if (state.pendingResumeOn === session) $("resume-on-dialog").close();
      state.pendingSelectionName = { name: result.name, machine: result.machine };
      reconcileSelection(); toast(`Resumed ${session.name} on ${result.machine}`);
    } catch (error) {
      if (state.pendingResumeOn === session) { $("resume-on-error").textContent = error.message; $("resume-on-error").hidden = false; }
      else toast(error.message);
    } finally {
      state.resumingOn = false; $("resume-on-confirm").disabled = false; render();
    }
  });

  $("quick-resume").addEventListener("click", openRestartDialog);
  for (const [quickId, actionId] of [["quick-tmux-prefix-twice", "tmux-prefix-twice"], ["quick-interrupt", "interrupt"], ["quick-kill-open", "kill-open"]]) {
    $(quickId).addEventListener("click", () => {
      if ($(quickId).disabled) return;
      $("quick-actions-dialog").close();
      $(actionId).click();
    });
  }
  $("message").addEventListener("paste", (event) => {
    const images = imageFilesFromTransfer(event.clipboardData);
    if (!images.length) return;
    event.preventDefault();
    addAttachmentFiles(images);
  });
  const composer = $("composer");
  composer.addEventListener("dragenter", (event) => {
    if (!event.dataTransfer?.types?.includes("Files")) return;
    event.preventDefault();
    composer.classList.add("drop-target");
  });
  composer.addEventListener("dragover", (event) => {
    if (!event.dataTransfer?.types?.includes("Files")) return;
    event.preventDefault();
    event.dataTransfer.dropEffect = "copy";
  });
  composer.addEventListener("dragleave", (event) => {
    if (!composer.contains(event.relatedTarget)) composer.classList.remove("drop-target");
  });
  composer.addEventListener("drop", (event) => {
    event.preventDefault();
    composer.classList.remove("drop-target");
    const images = imageFilesFromTransfer(event.dataTransfer);
    addAttachmentFiles(images);
  });
  async function compactSelectedAgent() {
    const paneId = state.selected;
    if (!paneId) return;
    const button = $("quick-compact"); button.disabled = true;
    try {
      await request(`/api/v1/panes/${encodeURIComponent(paneId)}/messages`, { method: "POST", body: JSON.stringify({ text: "/compact", submit: true }) });
      toast("Sent /compact");
    } catch (error) { toast(error.message); }
    finally { render(); }
  }
  function queuedPaneKeyCount() {
    return state.specialKeyQueue.length + Number(Boolean(state.specialKeySending));
  }
  function discardQueuedPaneKeys(delivery) {
    const target = paneSpecialKeyTarget(delivery);
    const before = state.specialKeyQueue.length;
    state.specialKeyQueue = state.specialKeyQueue.filter((item) => paneSpecialKeyTarget(item) !== target);
    return before - state.specialKeyQueue.length;
  }
  function setPaneKeyStatus(delivery, message) {
    const target = paneSpecialKeyTarget(delivery);
    state.specialKeyStatuses.delete(target);
    state.specialKeyStatuses.set(target, message);
    while (state.specialKeyStatuses.size > MAX_PANE_KEY_STATUSES) {
      state.specialKeyStatuses.delete(state.specialKeyStatuses.keys().next().value);
    }
  }
  function paneKeyFailureMessage(error, discarded) {
    const suffix = discarded > 0
      ? ` ${discarded} queued key${discarded === 1 ? " was" : "s were"} discarded for that agent.`
      : "";
    if ([400, 404, 422].includes(error.status)) {
      return `Key controls and this atmux server are out of sync.${suffix} Refresh after the server updates, then try again.`;
    }
    if (error.status === 409) {
      return `This agent changed before the key arrived.${suffix} Reopen the agent and try again.`;
    }
    if (error.status === 401 || error.status === 403) {
      return `Key delivery was rejected by sign-in or origin checks.${suffix} Refresh or sign in again, then try again.`;
    }
    return `Key delivery failed: ${error.message}.${suffix} Check the agent connection, then try again.`;
  }
  async function drainPaneSpecialKeyQueue() {
    if (state.specialKeySending) return;
    while (state.specialKeyQueue.length > 0) {
      const delivery = state.specialKeyQueue.shift();
      state.specialKeySending = delivery;
      render();
      try {
        await request(`/api/v1/panes/${encodeURIComponent(delivery.paneId)}/input-keys`, {
          method: "POST",
          body: JSON.stringify({
            action: delivery.action,
            machine: delivery.machine,
            instance_id: delivery.instanceId,
          }),
        });
        setPaneKeyStatus(delivery, `Sent ${delivery.label}.`);
      } catch (error) {
        const discarded = discardQueuedPaneKeys(delivery);
        const message = paneKeyFailureMessage(error, discarded);
        setPaneKeyStatus(delivery, message);
        toast(message);
      } finally {
        state.specialKeySending = null;
        render();
      }
    }
  }
  function sendPaneSpecialKey(action, label) {
    const captured = paneSpecialKeyDelivery(state.sessions.get(state.selected), action);
    const delivery = captured ? Object.freeze({ ...captured, label }) : null;
    if (!delivery) {
      toast("This agent changed or does not report a safe key-delivery target");
      return;
    }
    if (queuedPaneKeyCount() >= MAX_QUEUED_PANE_KEYS) {
      const message = `Key queue full (${MAX_QUEUED_PANE_KEYS}). Wait for a key to finish.`;
      setPaneKeyStatus(delivery, message);
      toast(message);
      render();
      return;
    }
    state.specialKeyQueue.push(delivery);
    render();
    void drainPaneSpecialKeyQueue();
  }
  document.querySelectorAll("[data-pane-key]").forEach((button) => {
    button.addEventListener("click", () => {
      if (button.disabled) return;
      const labels = { up: "Up", down: "Down", left: "Left", right: "Right", enter: "blank Enter" };
      sendPaneSpecialKey(button.dataset.paneKey, labels[button.dataset.paneKey] || "key");
    });
  });
  $("tmux-prefix-twice").addEventListener("click", () => {
    sendPaneSpecialKey("tmux_prefix_twice", "Ctrl+B twice");
  });
  $("message").addEventListener("input", () => {
    state.composerRevision += 1;
    state.messageHistoryNavigation = null;
    persistBoundComposerDraft();
  });
  $("message").addEventListener("select", () => { persistBoundComposerDraft(); });
  $("message").addEventListener("keydown", (event) => {
    const action = composerEnterAction(event);
    if (action === "send") {
      event.preventDefault();
      void sendComposerMessage();
    } else if (action === "newline") {
      event.preventDefault();
      const input = event.currentTarget;
      const start = input.selectionStart ?? input.value.length;
      const end = input.selectionEnd ?? start;
      input.setRangeText("\n", start, end, "end");
      input.dispatchEvent(new Event("input", { bubbles: true }));
    } else if (handlesMessageHistoryKey(event)) {
      event.preventDefault();
    }
  });
  pane.addEventListener("pointerdown", () => {
    state.panePointerDown = true;
    state.paneFollowing = false;
  });
  // Tapping the transcript — to expand a tool card, or to dismiss the mobile
  // keyboard — must not silently stop the pane from following. Only a real
  // upward gesture unpins ahead of the scroll event, and the scroll handler
  // re-pins the moment the reader returns to the tail.
  conversation.addEventListener("pointerdown", () => {
    state.transcriptPointerDown = true;
  });
  pane.addEventListener("wheel", () => { state.paneFollowing = false; }, { passive: true });
  pane.addEventListener("touchstart", () => { state.paneFollowing = false; }, { passive: true });
  conversation.addEventListener("wheel", (event) => {
    if (event.deltaY < 0) state.transcriptFollowing = false;
  }, { passive: true });
  conversation.addEventListener("touchmove", () => { state.transcriptFollowing = false; }, { passive: true });
  $("conversation-jump").addEventListener("click", () => {
    state.transcriptFollowing = true;
    state.transcriptUnseen = false;
    scrollConversationToBottom();
  });
  pane.addEventListener("scroll", () => {
    if (pane.hidden) return;
    state.paneReadingScrollTop = pane.scrollTop;
    if (scrollMatchesExpectedPosition(pane, state.paneExpectedScrollTop)) {
      state.paneExpectedScrollTop = null;
      return;
    }
    state.paneExpectedScrollTop = null;
    state.paneFollowing = followsLiveTail(pane, LIVE_TAIL_TOLERANCE);
  }, { passive: true });
  conversation.addEventListener("scroll", () => {
    if (conversation.hidden) return;
    state.transcriptReadingScrollTop = conversation.scrollTop;
    if (scrollMatchesExpectedPosition(conversation, state.transcriptExpectedScrollTop)) {
      state.transcriptExpectedScrollTop = null;
      return;
    }
    state.transcriptExpectedScrollTop = null;
    // Geometry, not gesture bookkeeping, decides pinning: reaching the tail by
    // any means re-pins and retires the pill.
    state.transcriptFollowing = followsLiveTail(conversation, STICKY_BOTTOM_TOLERANCE);
    if (state.transcriptFollowing) state.transcriptUnseen = false;
    renderTranscriptJump();
  }, { passive: true });
  const finishPanePointerSelection = () => {
    state.panePointerDown = false;
    state.transcriptPointerDown = false;
    flushPendingPaneRender();
    flushPendingTranscriptRender();
  };
  document.addEventListener("pointerup", finishPanePointerSelection);
  document.addEventListener("pointercancel", finishPanePointerSelection);
  document.addEventListener("selectionchange", () => {
    flushPendingPaneRender();
    flushPendingTranscriptRender();
  });
  const handleSessionSurfaceKeydown = (event) => {
    if (handlesMessageHistoryKey(event, true)) {
      event.preventDefault();
      return;
    }
    const text = paneTypingText(event);
    const message = $("message");
    if (!text || !state.selected || message.disabled) return;
    event.preventDefault();
    message.focus({ preventScroll: true });
    const start = message.selectionStart ?? message.value.length;
    const end = message.selectionEnd ?? start;
    message.setRangeText(text, start, end, "end");
    message.dispatchEvent(new Event("input", { bubbles: true }));
  };
  pane.addEventListener("keydown", handleSessionSurfaceKeydown);
  conversation.addEventListener("keydown", handleSessionSurfaceKeydown);

  const talkButton = $("talk");
  const SpeechRecognition = window.SpeechRecognition || window.webkitSpeechRecognition;
  const dictation = {
    recognition: null,
    active: false,
    holding: false,
    releaseRequested: false,
    failed: false,
    paneId: null,
    identityKey: null,
    prefix: "",
    finalText: "",
    interimText: "",
    restartAttempts: 0,
    restartTimer: null,
    stopTimer: null,
    generation: 0,
  };

  function dictationText() {
    return [dictation.prefix, dictation.finalText, dictation.interimText].filter(Boolean).join(" ").trim();
  }

  function finishDictation(abortActive = false) {
    if (!dictation.releaseRequested) return;
    if (dictation.restartTimer !== null) clearTimeout(dictation.restartTimer);
    if (dictation.stopTimer !== null) clearTimeout(dictation.stopTimer);
    dictation.restartTimer = null;
    dictation.stopTimer = null;
    const recognition = dictation.recognition;
    dictation.recognition = null;
    dictation.releaseRequested = false;
    dictation.holding = false;
    dictation.active = false;
    dictation.generation += 1;
    if (recognition) {
      recognition.onresult = null;
      recognition.onerror = null;
      recognition.onend = null;
      if (abortActive) {
        try { recognition.abort?.(); } catch { /* best-effort stale recognizer cleanup */ }
      }
    }
    talkButton.classList.remove("recording");
    talkButton.textContent = "Hold to talk";
    const paneId = dictation.paneId;
    const identityKey = dictation.identityKey;
    const targetMatches = composerTargetMatches(paneId, identityKey);
    const delivery = targetMatches
      ? dictationDelivery(paneId, dictation.prefix, dictation.finalText)
      : null;
    dictation.paneId = null;
    dictation.identityKey = null;
    if (!targetMatches && !dictation.failed) {
      toast("Agent restarted while listening. Speech was not sent to the replacement agent.");
    }
    if (!dictation.failed && delivery) {
      void sendComposerMessage(delivery.paneId, delivery.message, {
        clearOnAccept: true,
        targetIdentityKey: identityKey,
      });
    }
  }

  if (!SpeechRecognition) {
    talkButton.disabled = true;
    talkButton.title = "Speech recognition is not available in this browser";
  } else {
    const clearRestart = () => {
      if (dictation.restartTimer !== null) clearTimeout(dictation.restartTimer);
      dictation.restartTimer = null;
    };
    const scheduleRestart = (generation) => {
      clearRestart();
      const delay = dictationRestartDelay(dictation.restartAttempts);
      dictation.restartAttempts += 1;
      dictation.restartTimer = setTimeout(() => {
        dictation.restartTimer = null;
        if (generation !== dictation.generation) return;
        if (!dictation.holding || dictation.releaseRequested || dictation.failed) {
          if (dictation.releaseRequested || dictation.failed) finishDictation();
          return;
        }
        startRecognition();
      }, delay);
    };
    const startRecognition = () => {
      if (!dictation.holding || dictation.releaseRequested || dictation.failed || dictation.active) return;
      if (!composerTargetMatches(dictation.paneId, dictation.identityKey)) {
        dictation.failed = true;
        dictation.holding = false;
        dictation.releaseRequested = true;
        toast("Agent restarted while listening. Speech was not sent to the replacement agent.");
        finishDictation(true);
        return;
      }
      const generation = dictation.generation;
      const recognition = new SpeechRecognition();
      recognition.continuous = true;
      recognition.interimResults = true;
      recognition.lang = navigator.language || "en-US";
      const isCurrent = () => generation === dictation.generation
        && recognition === dictation.recognition;
      recognition.onresult = (event) => {
        if (!isCurrent()) return;
        if (!composerTargetMatches(dictation.paneId, dictation.identityKey)) {
          dictation.failed = true;
          dictation.holding = false;
          dictation.releaseRequested = true;
          toast("Agent restarted while listening. Speech was not sent to the replacement agent.");
          requestRecognitionStop(generation, recognition);
          return;
        }
        dictation.restartAttempts = 0;
        let interim = "";
        for (let index = event.resultIndex; index < event.results.length; index += 1) {
          const transcript = event.results[index][0]?.transcript?.trim() || "";
          if (event.results[index].isFinal) dictation.finalText = [dictation.finalText, transcript].filter(Boolean).join(" ");
          else interim = [interim, transcript].filter(Boolean).join(" ");
        }
        dictation.interimText = interim;
        if (state.selected === dictation.paneId
            && composerTargetMatches(dictation.paneId, dictation.identityKey)) {
          replaceComposerValue(dictationText());
        }
      };
      recognition.onerror = (event) => {
        if (!isCurrent()) return;
        const policy = dictationErrorPolicy(event.error);
        if (policy !== "fail") return;
        dictation.failed = true;
        dictation.holding = false;
        dictation.releaseRequested = true;
        toast(event.error === "not-allowed" || event.error === "service-not-allowed"
          ? "Microphone access was denied"
          : "Speech recognition failed");
        requestRecognitionStop(generation, recognition);
      };
      recognition.onend = () => {
        if (!isCurrent()) return;
        if (dictation.stopTimer !== null) clearTimeout(dictation.stopTimer);
        dictation.stopTimer = null;
        dictation.recognition = null;
        dictation.active = false;
        recognition.onresult = null;
        recognition.onerror = null;
        recognition.onend = null;
        if (dictationEndAction(dictation.holding, dictation.releaseRequested, dictation.failed) === "restart") {
          scheduleRestart(generation);
          return;
        }
        if (!dictation.releaseRequested) dictation.releaseRequested = true;
        finishDictation();
      };
      dictation.recognition = recognition;
      dictation.active = true;
      try { recognition.start(); }
      catch {
        recognition.onresult = null;
        recognition.onerror = null;
        recognition.onend = null;
        if (dictation.recognition === recognition) dictation.recognition = null;
        dictation.active = false;
        scheduleRestart(generation);
      }
    };
    const requestRecognitionStop = (generation, recognition) => {
      if (generation !== dictation.generation || recognition !== dictation.recognition) return;
      try { recognition.stop(); }
      catch { finishDictation(true); return; }
      if (generation !== dictation.generation || recognition !== dictation.recognition) return;
      if (dictation.stopTimer !== null) clearTimeout(dictation.stopTimer);
      dictation.stopTimer = setTimeout(() => {
        dictation.stopTimer = null;
        if (generation !== dictation.generation || recognition !== dictation.recognition) return;
        finishDictation(true);
      }, 2000);
    };
    const stopTalking = () => {
      if (!dictation.holding && !dictation.active && dictation.restartTimer === null) return;
      dictation.holding = false;
      dictation.releaseRequested = true;
      clearRestart();
      const recognition = dictation.recognition;
      if (!dictation.active || !recognition) { finishDictation(); return; }
      requestRecognitionStop(dictation.generation, recognition);
    };
    talkButton.addEventListener("pointerdown", (event) => {
      if (!state.selected || dictation.holding || dictation.active || dictation.restartTimer !== null) return;
      const identity = selectedComposerDraftIdentity();
      if (!identity?.persistent) {
        toast("This agent's identity is unavailable; reconnect before using Quick Talk");
        return;
      }
      event.preventDefault();
      talkButton.setPointerCapture?.(event.pointerId);
      // Starting another hold is composer activity even before speech arrives;
      // a late failure from the prior hold must not repopulate this new draft.
      state.composerRevision += 1;
      dictation.generation += 1;
      dictation.holding = true;
      dictation.releaseRequested = false;
      dictation.failed = false;
      dictation.paneId = state.selected;
      dictation.identityKey = identity.key;
      dictation.prefix = dictationPrefix(
        $("message").value,
        state.composerSending,
        state.inFlightComposerText,
        state.inFlightComposerIdentity,
        selectedComposerDraftIdentity()?.key,
      );
      dictation.finalText = "";
      dictation.interimText = "";
      dictation.restartAttempts = 0;
      talkButton.classList.add("recording");
      talkButton.textContent = "Release to send";
      startRecognition();
    });
    talkButton.addEventListener("pointerup", stopTalking);
    talkButton.addEventListener("pointercancel", stopTalking);
    window.addEventListener("blur", stopTalking);
    document.addEventListener("visibilitychange", () => { if (document.hidden) stopTalking(); });
  }
  $("interrupt").addEventListener("click", async () => {
    if (!state.selected) return;
    try { await request(`/api/v1/panes/${encodeURIComponent(state.selected)}/interrupt`, { method: "POST" }); toast("Interrupt sent"); }
    catch (error) { toast(error.message); }
  });

  function openKillDialog(id) {
    const session = state.sessions.get(id);
    if (!session || !isMachineControllable(machineOf(session))) return;
    state.pendingKillId = id;
    $("kill-name").textContent = session.name;
    $("kill-dialog").showModal();
  }

  function openInlineRename(id, anchor = $("agent-name")) {
    const session = state.sessions.get(id);
    if (!session || !PANE_INSTANCE_PATTERN.test(String(session.instance_id || "")) || !isMachineControllable(machineOf(session))) return;
    state.inlineRename?.close();
    const previousName = session.name;
    const host = anchor.closest(".session-row") || anchor.parentElement;
    state.inlineRename = createInlineRenameEditor({ document, host, anchor, session,
      save: async (edit) => {
        await request(sessionDeletePath(edit.id), { method: "PATCH", body: JSON.stringify(edit.body) });
        const current = state.sessions.get(edit.id);
        if (current?.instance_id === session.instance_id) { current.name = edit.body.name; render(); }
        toast(`Renamed ${previousName} to ${edit.body.name}`);
      },
      suggest: async (snapshot) => {
        const summary = await request(`/api/v1/panes/${encodeURIComponent(snapshot.id)}/summary`);
        if (state.sessions.get(snapshot.id)?.instance_id !== snapshot.instance_id) throw new Error("The session changed; open rename again.");
        return summary.title;
      },
      close: () => { state.inlineRename = null; },
    });
  }
  bindInlineRenameGesture($("agent-name"), () => openInlineRename(state.selected));
  document.addEventListener("keydown", (event) => {
    if (event.key !== "F2") return;
    if (inlineRenameAction(event, { selected: Boolean(state.selected), dialogOpen: Boolean(document.querySelector("dialog[open]")) }) === "open") {
      event.preventDefault(); openInlineRename(state.selected);
    }
  });

  function openSessionEditDialog(id) {
    const session = state.sessions.get(id);
    if (!session || !isMachineControllable(machineOf(session))) return;
    // Bind the edit to the pane generation seen now, not whatever the row
    // holds by the time Save is pressed.
    state.pendingSessionEdit = {
      id: session.id,
      instance_id: session.instance_id,
      name: session.name,
      description: session.description || "",
      description_source: session.description_source,
    };
    $("session-edit-current").textContent = session.name;
    $("session-edit-name").value = session.name;
    $("session-edit-description").value = session.description || "";
    const note = $("session-edit-note"); note.textContent = ""; note.hidden = true;
    $("session-edit-dialog").showModal();
    $("session-edit-name").focus();
  }

  $("session-edit-form").addEventListener("input", () => { $("session-edit-note").hidden = true; });
  $("session-edit-form").addEventListener("submit", async (event) => {
    event.preventDefault();
    const pending = state.pendingSessionEdit;
    if (!pending) return;
    const note = $("session-edit-note");
    const showNote = (message) => { note.textContent = message; note.hidden = false; };
    const edit = sessionEditRequest(pending, $("session-edit-name").value, $("session-edit-description").value);
    if (edit.error) { showNote(edit.error); return; }
    if (edit.unchanged) { $("session-edit-dialog").close(); return; }
    const button = $("session-edit-save");
    button.disabled = true;
    try {
      await request(sessionDeletePath(edit.id), { method: "PATCH", body: JSON.stringify(edit.body) });
      if (state.pendingSessionEdit === pending) $("session-edit-dialog").close();
      toast(edit.body.name ? `Renamed ${pending.name} to ${edit.body.name}` : "Description saved");
    } catch (error) {
      if (state.pendingSessionEdit === pending) showNote(error.message);
      else toast(error.message);
    } finally { button.disabled = false; }
  });

  $("kill-open").addEventListener("click", () => openKillDialog(state.selected));
  $("kill-confirm").addEventListener("click", async () => {
    const target = state.pendingKillId;
    if (!target) return;
    if (state.selected === target && !confirmDiscardFileEdit()) return;
    try {
      await request(sessionDeletePath(target), { method: "DELETE" });
      const deleted = state.sessions.get(target);
      if (deleted) forgetComposerDraft(composerDraftIdentity(deleted), true);
      $("kill-dialog").close();
      state.pendingKillId = null;
      if (state.selected === target) selectSession(null, "replace");
      toast("Session killed");
    } catch (error) { toast(error.message); }
  });

  $("resume-confirm").addEventListener("click", async () => {
    const confirmed = state.pendingResumeId;
    if (!confirmed || state.resumingPaneId) return;
    const target = confirmed.id;
    const button = $("resume-confirm");
    state.resumingPaneId = target;
    button.disabled = true;
    render();
    try {
      await request(`/api/v1/panes/${encodeURIComponent(target)}/restart-instance`, {
        method: "POST",
        body: JSON.stringify({ instance_id: confirmed.instance_id, restart_token: confirmed.restart_token }),
      });
      $("resume-dialog").close();
      toast("Agent session restarted");
    } catch (error) {
      state.pendingResumeId = null;
      $("resume-dialog").close();
      toast(error.message);
    } finally {
      state.resumingPaneId = null;
      button.disabled = false;
      if (state.selected === target) await refreshModels(target);
      render();
    }
  });

  async function openLaunchDialog(duplicateSession = null) {
    const sourceSnapshot = duplicateSourceSnapshot(duplicateSession);
    const existingDialog = $("launch-dialog");
    if (existingDialog.open) invalidateLaunchDialog();
    const generation = ++state.launchDialogGeneration;
    state.launchFlow = null;
    state.launchSummarySourceId = null;
    const capabilitiesRequest = duplicateSession
      ? request(`/api/v1/panes/${encodeURIComponent(duplicateSession.id)}/models`)
      : Promise.resolve(null);
    let options;
    let capabilities;
    try {
      [options, capabilities] = await Promise.all([
        request("/api/v1/launch-options"),
        capabilitiesRequest,
      ]);
    } catch (error) {
      if (generation !== state.launchDialogGeneration) return false;
      throw error;
    }
    if (generation !== state.launchDialogGeneration) return false;
    const liveDuplicateSession = sourceSnapshot
      ? state.sessions.get(sourceSnapshot.id)
      : null;
    if (sourceSnapshot && !duplicateSourceMatches(sourceSnapshot, liveDuplicateSession)) {
      throw new Error("The source agent changed while Duplicate was loading; try again");
    }
    const sourceSession = liveDuplicateSession || duplicateSession;
    state.launchFlow = sourceSnapshot ? "duplicate" : "launch";
    state.launchOptions = options;
    const machines = launchMachines(options);
    $("launch-machine").replaceChildren(...machines.map((machine) =>
      option(
        machine.id,
        !machine.online
          ? `${machine.label} (offline)`
          : (isLaunchCapableMachine(machine) ? machine.label : `${machine.label} (launch unavailable)`),
        !isLaunchCapableMachine(machine),
      )));
    const selectedSession = sourceSession || state.sessions.get(state.selected)
      || (state.selected ? { id: state.selected } : null);
    const localMachineId = state.machines.find((machine) => machine.kind === "local")?.id || "local";
    const preferredMachineId = preferredLaunchMachineId(
      machines,
      state.selectedMachine,
      selectedSession,
      localMachineId,
    );
    $("launch-machine").value = preferredMachineId || "";
    $("launch-machine").disabled = !preferredMachineId;
    $("launch-machine-row").hidden = machines.length < 2 && Boolean(preferredMachineId);
    $("launch-directory").value = "";
    $("launch-directory").dataset.selectedDirectory = "";
    clearLaunchSessions();
    state.launchNamePristine = true;
    applyLaunchMachine();
    if (sourceSnapshot) {
      const selection = duplicateLaunchSelection(
        options,
        sourceSession,
        capabilities,
        [...state.sessions.values()],
      );
      applyDuplicateLaunchSelection(selection);
    }
    applyDuplicateSummary(sourceSnapshot ? sourceSession : null);
    if (generation !== state.launchDialogGeneration) return false;
    $("launch-dialog-title").textContent = sourceSnapshot ? "Duplicate agent" : "Launch agent";
    $("launch-form").querySelector("button[type=submit]").textContent = sourceSnapshot
      ? "Launch duplicate"
      : "Launch";
    $("launch-dialog").dataset.launchGeneration = String(generation);
    $("launch-dialog").showModal();
    return true;
  }

  $("launch-open").addEventListener("click", () => {
    void openLaunchDialog().catch((error) => {
      invalidateLaunchDialog(false);
      toast(error.message);
    });
  });

  function fallbackLaunchMachine() {
    return {
      id: "",
      online: false,
      directories: [],
      profiles: [],
      project_preferences: {},
      memory: null,
      note: "No online machine currently has both runnable agent profiles and configured project folders.",
    };
  }

  function applyLaunchMachine() {
    cancelLaunchDirectorySearch();
    state.launchDirectoryCandidates = null;
    state.launchDirectorySuggestionsDismissed = false;
    const machines = launchMachines(state.launchOptions);
    const candidate = machines.find((machine) => machine.id === $("launch-machine").value);
    const selected = isLaunchCapableMachine(candidate) ? candidate : fallbackLaunchMachine();
    const available = isLaunchCapableMachine(selected);
    for (const id of [
      "launch-directory", "launch-browse", "launch-harness", "launch-profile", "launch-mode", "launch-name",
    ]) $(id).disabled = !available;
    $("launch-directory").value = "";
    $("launch-directory").dataset.selectedDirectory = "";
    state.launchNamePristine = true;
    clearLaunchSessions();
    closeLaunchBrowser();
    renderLaunchMemory(selected);
    renderLaunchDirectories(selected);
  }

  function currentLaunchMachine() {
    const machines = launchMachines(state.launchOptions);
    const selected = machines.find((machine) => machine.id === $("launch-machine").value);
    return isLaunchCapableMachine(selected) ? selected : fallbackLaunchMachine();
  }

  function launchDirectoryCandidates(selected = currentLaunchMachine()) {
    const cache = state.launchDirectoryCandidates;
    if (cache?.machine === selected
        && cache.remembered === state.rememberedLaunchDirectories) return cache.directories;
    const directories = availableLaunchDirectories(selected, state.rememberedLaunchDirectories);
    state.launchDirectoryCandidates = {
      machine: selected,
      remembered: state.rememberedLaunchDirectories,
      directories,
    };
    return directories;
  }

  function renderLaunchDirectories(selected = currentLaunchMachine()) {
    cancelLaunchDirectorySearch();
    const input = $("launch-directory");
    const available = launchDirectoryCandidates(selected);
    const directories = filterDirectories(available, input.value);
    renderLaunchDirectorySuggestions(directories);
    const directory = available.includes(input.value) ? input.value : "";
    const manual = !directory && isManualDirectory(input.value) ? input.value.trim() : "";
    const previous = input.dataset.selectedDirectory || "";
    input.dataset.selectedDirectory = directory;
    if (directory) applyProjectPreferences(selected, directory, directory !== previous);
    else {
      if (!previous) renderLaunchHarnesses(selected);
      if (manual) suggestName({}, true);
    }
    updateLaunchAvailability(selected, directories, directory || manual);
    void refreshLaunchSessions();
  }

  function hideLaunchDirectorySuggestions(dismissed = false) {
    const input = $("launch-directory");
    const suggestions = $("launch-directory-suggestions");
    suggestions.hidden = true;
    input.setAttribute("aria-expanded", "false");
    input.removeAttribute("aria-activedescendant");
    state.launchDirectoryActiveIndex = -1;
    state.launchDirectorySuggestionsDismissed = dismissed;
    for (const option of suggestions.children) option.setAttribute("aria-selected", "false");
  }

  function showLaunchDirectorySuggestions() {
    const suggestions = $("launch-directory-suggestions");
    if (!suggestions.children.length || state.launchDirectorySuggestionsDismissed) return;
    suggestions.hidden = false;
    $("launch-directory").setAttribute("aria-expanded", "true");
  }

  function activateLaunchDirectorySuggestion(index) {
    const input = $("launch-directory");
    const suggestions = $("launch-directory-suggestions");
    const options = [...suggestions.children];
    if (!options.length) return;
    const next = Math.max(0, Math.min(index, options.length - 1));
    state.launchDirectoryActiveIndex = next;
    options.forEach((option, optionIndex) => {
      option.setAttribute("aria-selected", String(optionIndex === next));
    });
    input.setAttribute("aria-activedescendant", options[next].id);
    options[next].scrollIntoView({ block: "nearest" });
  }

  function selectLaunchDirectorySuggestion(directory) {
    if (!launchDirectoryCandidates().includes(directory)) return;
    $("launch-directory").value = directory;
    state.launchDirectorySuggestionsDismissed = true;
    renderLaunchDirectories();
    hideLaunchDirectorySuggestions(true);
  }

  function renderLaunchDirectorySuggestions(directories) {
    const suggestions = $("launch-directory-suggestions");
    state.launchDirectoryActiveIndex = -1;
    $("launch-directory").removeAttribute("aria-activedescendant");
    suggestions.replaceChildren(...directories.map((directory, index) => {
      const button = document.createElement("button");
      button.type = "button";
      button.className = "launch-directory-suggestion";
      button.id = `launch-directory-suggestion-${index}`;
      button.setAttribute("role", "option");
      button.setAttribute("aria-selected", "false");
      button.tabIndex = -1;
      button.dataset.directory = directory;
      button.setAttribute("aria-label", `${projectLabel(directory)}, ${directory}`);
      const label = document.createElement("strong");
      label.textContent = projectLabel(directory);
      const path = document.createElement("small");
      path.textContent = directory;
      button.append(label, path);
      button.addEventListener("pointerdown", (event) => {
        state.launchDirectorySuppressClick = null;
        if (!["touch", "pen"].includes(event.pointerType)) return;
        state.launchDirectoryPointerGesture = {
          pointerId: event.pointerId,
          x: event.clientX,
          y: event.clientY,
          moved: false,
        };
      });
      button.addEventListener("pointermove", (event) => {
        const gesture = state.launchDirectoryPointerGesture;
        if (!gesture || gesture.pointerId !== event.pointerId) return;
        if (Math.hypot(event.clientX - gesture.x, event.clientY - gesture.y) >= 10) {
          gesture.moved = true;
        }
      });
      const finishPointerGesture = (event) => {
        const gesture = state.launchDirectoryPointerGesture;
        if (!gesture || gesture.pointerId !== event.pointerId) return;
        state.launchDirectoryPointerGesture = null;
        if (!gesture.moved) return;
        const suppression = {};
        state.launchDirectorySuppressClick = suppression;
        setTimeout(() => {
          if (state.launchDirectorySuppressClick === suppression) {
            state.launchDirectorySuppressClick = null;
          }
        }, 250);
      };
      button.addEventListener("pointerup", finishPointerGesture);
      button.addEventListener("pointercancel", finishPointerGesture);
      // Keep the combobox focused between mouse press and release so its blur
      // frame cannot hide the option before the click. This is intentionally a
      // mouse event: touch/pen pointer events remain uncancelled for pan-y.
      button.addEventListener("mousedown", (event) => event.preventDefault());
      button.addEventListener("click", (event) => {
        if (state.launchDirectorySuppressClick) {
          event.preventDefault();
          state.launchDirectorySuppressClick = null;
          return;
        }
        selectLaunchDirectorySuggestion(directory);
      });
      return button;
    }));
    if (document.activeElement === $("launch-directory")
        && directories.length
        && !state.launchDirectorySuggestionsDismissed) {
      showLaunchDirectorySuggestions();
    } else {
      hideLaunchDirectorySuggestions(state.launchDirectorySuggestionsDismissed);
    }
  }

  function cancelLaunchDirectorySearch() {
    if (state.launchDirectorySearchTimer !== null) {
      clearTimeout(state.launchDirectorySearchTimer);
      state.launchDirectorySearchTimer = null;
    }
  }

  function scheduleLaunchDirectorySearch() {
    cancelLaunchDirectorySearch();
    const machine = currentLaunchMachine();
    const input = $("launch-directory");
    state.launchDirectorySuggestionsDismissed = false;
    hideLaunchDirectorySuggestions();
    input.dataset.selectedDirectory = "";
    clearLaunchSessions();
    const manual = isManualDirectory(input.value) ? input.value.trim() : "";
    updateLaunchAvailability(machine, [], manual);
    const machineId = machine.id;
    state.launchDirectorySearchTimer = setTimeout(() => {
      state.launchDirectorySearchTimer = null;
      if ($("launch-dialog").open && currentLaunchMachine().id === machineId) {
        renderLaunchDirectories();
      }
    }, LAUNCH_DIRECTORY_SEARCH_DEBOUNCE_MS);
  }

  function applyDuplicateLaunchSelection(selection) {
    $("launch-machine").value = selection.machineId;
    applyLaunchMachine();
    const machine = currentLaunchMachine();
    $("launch-directory").value = selection.directory;
    renderLaunchDirectories(machine);
    $("launch-harness").value = selection.harness;
    renderLaunchProfiles(machine);
    $("launch-profile").value = selection.profileId;
    renderLaunchModes(machine);
    if (selection.modeId) $("launch-mode").value = selection.modeId;
    selectLaunchMemory(selection.memoryMaxBytes);
    $("launch-session").value = "";
    $("launch-name").value = selection.name;
    state.launchNamePristine = false;
    updateLaunchAvailability(machine);
  }

  /// Offers the summarized handover only for a duplicate of a readable agent.
  /// The profile selector above stays untouched, so the same dialog swaps the
  /// credential profile and carries the previous conversation forward.
  function applyDuplicateSummary(sourceSession) {
    const local = state.machines.find((machine) => machine.kind === "local")?.id || "local";
    const view = duplicateSummaryState(sourceSession, state.paneLines, state.selected, local);
    state.launchSummarySourceId = view.available ? String(sourceSession.pane_id || "") : null;
    $("launch-summary").hidden = !view.available;
    $("launch-summary-resume").checked = view.available && view.checked;
  }

  function renderLaunchMemory(selected = currentLaunchMachine()) {
    const choices = memoryLimitChoices(selected.memory);
    const select = $("launch-memory");
    const defaultLabel = defaultMemoryLimitLabel(selected.memory);
    const options = [option("", defaultLabel)];
    if (choices.supported && choices.ceiling !== null) {
      options.push(...choices.presets.map((bytes) => option(String(bytes), formatMemoryLimit(bytes))));
      options.push(option("custom", "Custom…"));
    }
    select.replaceChildren(...options);
    select.value = "";
    select.disabled = !isLaunchCapableMachine(selected) || choices.ceiling === null;
    $("launch-memory-custom").value = "";
    $("launch-memory-custom").max = choices.ceiling === null
      ? ""
      : String(Math.floor(choices.ceiling / GIBIBYTE_BYTES));
    $("launch-memory-custom").disabled = select.disabled;
    $("launch-memory-custom-row").hidden = true;
    $("launch-memory-note").textContent = choices.note;
    $("launch-memory-group").hidden = selected === null;
  }

  function selectLaunchMemory(memoryMaxBytes) {
    const select = $("launch-memory");
    if (memoryMaxBytes == null) {
      select.value = "";
      $("launch-memory-custom-row").hidden = true;
      return;
    }
    const preset = [...select.options].find((candidate) => candidate.value === String(memoryMaxBytes));
    if (preset) {
      select.value = preset.value;
      $("launch-memory-custom-row").hidden = true;
      return;
    }
    select.value = "custom";
    $("launch-memory-custom").value = String(memoryMaxBytes / GIBIBYTE_BYTES);
    $("launch-memory-custom-row").hidden = false;
  }

  function applyProjectPreferences(selected, directory, forceName) {
    const preferences = projectPreference(selected, directory);
    renderLaunchHarnesses(selected, preferences);
    suggestName(preferences, forceName);
  }

  function renderLaunchHarnesses(selected = currentLaunchMachine(), preferences = {}) {
    const harnesses = harnessesForProfiles(selected.profiles);
    const select = $("launch-harness");
    const previous = select.value;
    const preferred = typeof preferences.harness === "string" ? preferences.harness : "";
    const chosen = harnesses.find((harness) => harness.toLowerCase() === preferred.toLowerCase())
      || harnesses.find((harness) => harness.toLowerCase() === previous.toLowerCase())
      || harnesses[0]
      || "";
    select.replaceChildren(...harnesses.map((harness) => option(harness, harness)));
    if (chosen) select.value = chosen;
    // Keep the agent selector visible even when this machine currently has
    // one harness. It makes the launch flow predictable across machines.
    $("launch-harness-row").hidden = harnesses.length === 0;
    renderLaunchProfiles(selected, preferences);
  }

  function renderLaunchProfiles(selected = currentLaunchMachine(), preferences = {}) {
    const profiles = profilesForHarness(selected.profiles, $("launch-harness").value);
    const select = $("launch-profile");
    const previous = select.value;
    const preferred = typeof preferences.profile === "string" ? preferences.profile : "";
    const chosen = profiles.find((profile) => profile.name.toLowerCase() === preferred.toLowerCase())
      || profiles.find((profile) => profile.id === previous)
      || profiles[0];
    select.replaceChildren(...profiles.map((profile) => option(profile.id, profile.name)));
    if (chosen) select.value = chosen.id;
    // A single "Default" profile is still useful context, and showing it
    // keeps Agent → Profile → Project explicit for every launch.
    $("launch-profile-row").hidden = profiles.length === 0;
    renderLaunchModes(selected);
  }

  function renderLaunchModes(selected = currentLaunchMachine()) {
    const profile = (selected.profiles || []).find((item) => item.id === $("launch-profile").value);
    const modes = Array.isArray(profile?.modes) ? profile.modes : [];
    const select = $("launch-mode");
    const previous = select.value;
    const chosen = modes.find((mode) => mode.id === previous) || modes[0];
    select.replaceChildren(...modes.map((mode) => option(mode.id, mode.label || mode.model || mode.id)));
    if (chosen) select.value = chosen.id;
    // Legacy profiles remain launchable, but only profiles with explicit
    // modes expose a model selector. A single mode stays visible as useful
    // confirmation of the account/model that will launch.
    $("launch-mode-row").hidden = modes.length === 0;
    void refreshLaunchSessions();
  }

  function updateLaunchAvailability(
    selected = currentLaunchMachine(),
    directories = null,
    directory = null,
  ) {
    const available = launchDirectoryCandidates(selected);
    const matches = directories ?? filterDirectories(available, $("launch-directory").value);
    const chosen = directory ?? (available.includes($("launch-directory").value)
      ? $("launch-directory").value
      : (isManualDirectory($("launch-directory").value) ? $("launch-directory").value.trim() : ""));
    const profiles = profilesForHarness(selected.profiles, $("launch-harness").value);
    const button = $("launch-form").querySelector("button[type=submit]");
    let memoryError = "";
    try {
      parseMemoryLimitSelection(
        selected.memory,
        $("launch-memory").value,
        $("launch-memory-custom").value,
      );
    } catch (error) {
      memoryError = error.message;
    }
    button.disabled = !isLaunchCapableMachine(selected) || !chosen || !profiles.length || Boolean(memoryError);
    const note = $("launch-note");
    const listed = available.includes(chosen);
    const message = selected.note || memoryError || (!chosen
      ? (!matches.length
        ? "No project matches. Type an absolute folder within a configured project root."
        : "Choose a project or type an absolute folder within a configured project root.")
      : (!profiles.length
        ? "No runnable agent profiles were discovered on this machine."
        : (!listed ? "Manual folder will be checked by that machine before launch." : "")));
    note.textContent = message;
    note.hidden = !message;
  }

  $("launch-machine").addEventListener("change", () => {
    // The summary source is a pane on the machine the duplicate came from.
    // Retarget the launch and that pane is no longer the right conversation.
    applyDuplicateSummary(null);
    applyLaunchMachine();
  });
  $("launch-directory").addEventListener("input", scheduleLaunchDirectorySearch);
  $("launch-directory").addEventListener("change", () => renderLaunchDirectories());
  $("launch-directory").addEventListener("focus", () => {
    state.launchDirectorySuggestionsDismissed = false;
    showLaunchDirectorySuggestions();
  });
  $("launch-directory").addEventListener("blur", () => {
    requestAnimationFrame(() => {
      if (document.activeElement !== $("launch-directory")) hideLaunchDirectorySuggestions();
    });
  });
  $("launch-directory").addEventListener("keydown", (event) => {
    if (event.isComposing) return;
    const suggestions = $("launch-directory-suggestions");
    const options = suggestions.children;
    if (["ArrowDown", "ArrowUp"].includes(event.key) && options.length) {
      event.preventDefault();
      state.launchDirectorySuggestionsDismissed = false;
      showLaunchDirectorySuggestions();
      const next = event.key === "ArrowDown"
        ? (state.launchDirectoryActiveIndex + 1) % options.length
        : (state.launchDirectoryActiveIndex <= 0
          ? options.length - 1
          : state.launchDirectoryActiveIndex - 1);
      activateLaunchDirectorySuggestion(next);
      return;
    }
    if (event.key === "Enter" && !suggestions.hidden && state.launchDirectoryActiveIndex >= 0) {
      event.preventDefault();
      selectLaunchDirectorySuggestion(options[state.launchDirectoryActiveIndex]?.dataset.directory);
      return;
    }
    if (event.key === "Escape" && !suggestions.hidden) {
      event.preventDefault();
      event.stopPropagation();
      hideLaunchDirectorySuggestions(true);
    }
  });
  $("launch-harness").addEventListener("change", () => {
    renderLaunchProfiles();
    updateLaunchAvailability();
  });
  $("launch-profile").addEventListener("change", () => {
    renderLaunchModes();
    updateLaunchAvailability();
  });
  $("launch-memory").addEventListener("change", () => {
    $("launch-memory-custom-row").hidden = $("launch-memory").value !== "custom";
    updateLaunchAvailability();
    if ($("launch-memory").value === "custom") $("launch-memory-custom").focus();
  });
  $("launch-memory").addEventListener("focus", revealFocusedLaunchMemoryControl);
  $("launch-memory-custom").addEventListener("focus", revealFocusedLaunchMemoryControl);
  $("launch-memory-custom").addEventListener("input", () => updateLaunchAvailability());
  $("launch-name").addEventListener("input", () => { state.launchNamePristine = false; });
  function persistLaunchDirectory(machine, directory) {
    state.rememberedLaunchDirectories = rememberLaunchDirectory(
      state.rememberedLaunchDirectories,
      machine,
      directory,
    );
    state.launchDirectoryCandidates = null;
    // A privacy-restricted browser may deny storage. Selection still works
    // for this page and the launch itself remains fully server-validated.
    writeLocalStorage(
      LAUNCH_DIRECTORY_STORAGE_KEY,
      JSON.stringify(state.rememberedLaunchDirectories),
    );
  }

  function clearLaunchSessions() {
    state.launchSessionsController?.abort();
    state.launchSessionsController = null;
    state.launchSessionsGeneration += 1;
    state.launchSessionsKey = "";
    $("launch-session").replaceChildren(option("", "Start a new conversation"));
    $("launch-sessions-note").textContent = "";
    $("launch-sessions").hidden = true;
  }

  async function refreshLaunchSessions() {
    if (state.launchFlow === "duplicate") {
      clearLaunchSessions();
      return;
    }
    const machine = currentLaunchMachine();
    const directory = $("launch-directory").dataset.selectedDirectory || "";
    const profileId = $("launch-profile").value;
    const profile = (machine.profiles || []).find((item) => item.id === profileId);
    const harness = String(profile?.harness || "").toLowerCase();
    if (!directory || !profileId || !["claude", "codex"].includes(harness)) {
      clearLaunchSessions();
      return;
    }
    const key = JSON.stringify([machine.id, directory, profileId]);
    if (state.launchSessionsKey === key) return;
    state.launchSessionsController?.abort();
    const controller = new AbortController();
    state.launchSessionsController = controller;
    const generation = ++state.launchSessionsGeneration;
    state.launchSessionsKey = key;
    const section = $("launch-sessions");
    const select = $("launch-session");
    const note = $("launch-sessions-note");
    select.replaceChildren(option("", "Start a new conversation"));
    note.textContent = "Looking for saved conversations…";
    section.hidden = false;
    const params = new URLSearchParams({
      machine: machine.id,
      directory,
      profile_id: profileId,
    });
    try {
      const listing = await request(`/api/v1/launch-sessions?${params}`, { signal: controller.signal });
      if (generation !== state.launchSessionsGeneration || state.launchSessionsKey !== key) return;
      if (listing?.directory !== directory || listing?.profile_id !== profileId) {
        throw new Error("Saved conversations changed; select the folder again");
      }
      const sessions = (Array.isArray(listing.sessions) ? listing.sessions : [])
        .slice(0, 20)
        .filter((session) => /^saved-[0-9a-f]{32}$/.test(String(session?.id || "")))
        .filter((session) => ["claude", "codex"].includes(String(session?.harness || "").toLowerCase()));
      const options = [option("", "Start a new conversation")];
      for (const session of sessions) {
        const updated = Number(session.updated_ms);
        const when = Number.isFinite(updated) && updated >= 0
          ? new Date(updated).toLocaleString()
          : "Saved conversation";
        const preview = savedSessionPreview(session.preview);
        const agent = String(session.harness).toLowerCase() === "claude" ? "Claude" : "Codex";
        const saved = option(session.id, `${agent} · ${when} · ${preview}`);
        saved.dataset.harness = String(session.harness).toLowerCase();
        saved.dataset.preview = preview;
        options.push(saved);
      }
      select.replaceChildren(...options);
      section.hidden = sessions.length === 0;
      note.textContent = listing?.truncated
        ? "Showing the newest saved conversations."
        : "Choose one to continue it, or start a new conversation.";
    } catch (error) {
      if (error?.name === "AbortError") return;
      if (generation !== state.launchSessionsGeneration) return;
      state.launchSessionsKey = "";
      select.replaceChildren(option("", "Start a new conversation"));
      note.textContent = `${error.message}. A new conversation can still be launched.`;
      section.hidden = false;
    } finally {
      if (state.launchSessionsController === controller) state.launchSessionsController = null;
    }
  }

  function closeLaunchBrowser() {
    state.launchBrowseGeneration += 1;
    resetLaunchBrowserMutation();
    closeLaunchBrowserOperation();
    $("launch-browser").hidden = true;
    $("launch-browser").dataset.current = "";
    $("launch-browser").dataset.parent = "";
  }

  function renderLaunchBrowser(listing, machine) {
    const browser = $("launch-browser");
    const current = validRememberedLaunchDirectory(listing?.current) ? listing.current.trim() : "";
    const parent = validRememberedLaunchDirectory(listing?.parent) ? listing.parent.trim() : "";
    browser.dataset.current = current;
    browser.dataset.parent = parent;
    $("launch-browser-path").textContent = current || `${machine.label || machine.id} project roots`;
    $("launch-browser-path").title = current;
    $("launch-browser-up").disabled = !parent;
    $("launch-browser-use").disabled = !current;
    $("launch-browser-new").disabled = !current;
    $("launch-browser-clone").disabled = !current;
    const folders = (Array.isArray(listing?.directories) ? listing.directories : [])
      .slice(0, 512)
      .filter((folder) => validRememberedLaunchDirectory(folder?.path));
    $("launch-browser-list").replaceChildren(...folders.map((folder) => {
      const button = document.createElement("button");
      button.type = "button";
      button.className = "launch-browser-folder";
      button.dataset.path = folder.path;
      button.textContent = `📁 ${String(folder.name || projectLabel(folder.path)).slice(0, 200)}`;
      button.title = folder.path;
      button.addEventListener("click", () => { void loadLaunchBrowser(folder.path); });
      return button;
    }));
    const note = $("launch-browser-note");
    note.textContent = listing?.truncated
      ? "This folder has more directories than can be shown. Choose a visible folder or type its absolute path."
      : (!folders.length ? "No subfolders here. You can use this folder." : "");
    note.hidden = !note.textContent;
  }

  async function loadLaunchBrowser(path = null) {
    const machine = currentLaunchMachine();
    const endpoint = launchDirectoryBrowsePath(machine.id, path);
    if (!endpoint) {
      toast("Choose a valid machine and folder");
      return;
    }
    const generation = ++state.launchBrowseGeneration;
    resetLaunchBrowserMutation();
    $("launch-browser").hidden = false;
    $("launch-browser-path").textContent = "Loading folders…";
    $("launch-browser-list").replaceChildren();
    $("launch-browser-up").disabled = true;
    $("launch-browser-use").disabled = true;
    $("launch-browser-new").disabled = true;
    $("launch-browser-clone").disabled = true;
    closeLaunchBrowserOperation();
    try {
      const listing = await request(endpoint);
      if (generation !== state.launchBrowseGeneration
          || machine.id !== currentLaunchMachine().id) return;
      renderLaunchBrowser(listing, machine);
    } catch (error) {
      if (generation !== state.launchBrowseGeneration) return;
      closeLaunchBrowser();
      toast(error.message);
    }
  }

  $("launch-browse").addEventListener("click", () => {
    const current = $("launch-directory").value.trim();
    void loadLaunchBrowser(validRememberedLaunchDirectory(current) ? current : null);
  });
  $("launch-browser-close").addEventListener("click", closeLaunchBrowser);
  $("launch-browser-up").addEventListener("click", () => {
    const parent = $("launch-browser").dataset.parent;
    if (parent) void loadLaunchBrowser(parent);
  });
  $("launch-browser-use").addEventListener("click", () => {
    const directory = $("launch-browser").dataset.current;
    const machine = currentLaunchMachine();
    if (!validRememberedLaunchDirectory(directory)) return;
    persistLaunchDirectory(machine.id, directory);
    $("launch-directory").value = directory;
    closeLaunchBrowser();
    renderLaunchDirectories(machine);
  });

  function closeLaunchBrowserOperation() {
    const operation = $("launch-browser-operation");
    operation.hidden = true;
    operation.dataset.kind = "";
    $("launch-browser-new").setAttribute("aria-expanded", "false");
    $("launch-browser-clone").setAttribute("aria-expanded", "false");
    $("launch-browser-operation-note").textContent = "";
    $("launch-browser-new-name").value = "";
    $("launch-browser-repository").value = "";
    $("launch-browser-destination").value = "";
    $("launch-browser-destination").dataset.manual = "false";
  }

  function openLaunchBrowserOperation(kind) {
    const current = $("launch-browser").dataset.current;
    if (!validRememberedLaunchDirectory(current) || state.launchBrowseMutation) return;
    const cloning = kind === "clone";
    const operation = $("launch-browser-operation");
    operation.dataset.kind = cloning ? "clone" : "folder";
    operation.hidden = false;
    $("launch-browser-operation-title").textContent = cloning ? "Clone repository here" : "Create folder here";
    $("launch-browser-new-row").hidden = cloning;
    $("launch-browser-repository-row").hidden = !cloning;
    $("launch-browser-destination-row").hidden = !cloning;
    $("launch-browser-operation-confirm").textContent = cloning ? "Clone" : "Create";
    $("launch-browser-operation-note").textContent = "";
    $("launch-browser-new").setAttribute("aria-expanded", String(!cloning));
    $("launch-browser-clone").setAttribute("aria-expanded", String(cloning));
    $("launch-browser-destination").dataset.manual = "false";
    requestAnimationFrame(() => {
      (cloning ? $("launch-browser-repository") : $("launch-browser-new-name")).focus();
    });
  }

  function setLaunchBrowserMutation(busy) {
    state.launchBrowseMutation = busy;
    const current = validRememberedLaunchDirectory($("launch-browser").dataset.current);
    for (const id of [
      "launch-browser-up", "launch-browser-use", "launch-browser-new", "launch-browser-clone",
      "launch-browser-operation-cancel", "launch-browser-operation-confirm",
      "launch-browser-new-name", "launch-browser-repository", "launch-browser-destination",
    ]) $(id).disabled = busy || (!current && id.startsWith("launch-browser-"));
    if (!busy) {
      $("launch-browser-up").disabled = !$("launch-browser").dataset.parent;
      $("launch-browser-use").disabled = !current;
      $("launch-browser-new").disabled = !current;
      $("launch-browser-clone").disabled = !current;
    }
  }

  function resetLaunchBrowserMutation() {
    state.launchBrowseMutation = false;
    for (const id of [
      "launch-browser-operation-cancel", "launch-browser-operation-confirm",
      "launch-browser-new-name", "launch-browser-repository", "launch-browser-destination",
    ]) $(id).disabled = false;
  }

  async function submitLaunchBrowserOperation() {
    if (state.launchBrowseMutation) return;
    const browser = $("launch-browser");
    const current = browser.dataset.current;
    const machine = currentLaunchMachine();
    const kind = $("launch-browser-operation").dataset.kind;
    if (!validRememberedLaunchDirectory(current) || !["folder", "clone"].includes(kind)) return;
    const body = { machine: machine.id, directory: current };
    let endpoint;
    let success;
    if (kind === "folder") {
      const name = $("launch-browser-new-name").value.trim();
      if (!validLaunchChildName(name)) {
        $("launch-browser-operation-note").textContent = "Enter one folder name without slashes or a leading dash.";
        return;
      }
      body.name = name;
      endpoint = "/api/v1/launch-directories/folders";
      success = `Created ${name}`;
    } else {
      const repository = $("launch-browser-repository").value.trim();
      const destination = $("launch-browser-destination").value.trim();
      if (!repository || repository.startsWith("-") || /[\u0000-\u001f\u007f]/.test(repository)) {
        $("launch-browser-operation-note").textContent = "Enter an HTTPS or SSH repository URL.";
        return;
      }
      if (destination && !validLaunchChildName(destination)) {
        $("launch-browser-operation-note").textContent = "Destination must be one folder name without slashes or a leading dash.";
        return;
      }
      body.repository = repository;
      body.destination = destination || null;
      endpoint = "/api/v1/launch-directories/clone";
      success = `Cloned ${destination || repositoryDestinationName(repository) || "repository"}`;
    }
    const generation = state.launchBrowseGeneration;
    setLaunchBrowserMutation(true);
    $("launch-browser-operation-note").textContent = kind === "clone" ? "Cloning repository…" : "Creating folder…";
    try {
      const result = await request(endpoint, { method: "POST", body: JSON.stringify(body) });
      if (generation !== state.launchBrowseGeneration || machine.id !== currentLaunchMachine().id) return;
      if (result?.listing?.machine !== machine.id
          || !validRememberedLaunchDirectory(result?.directory?.path)) {
        throw new Error("The owning machine returned an invalid folder result");
      }
      renderLaunchBrowser(result.listing, machine);
      closeLaunchBrowserOperation();
      [...document.querySelectorAll(".launch-browser-folder")]
        .find((button) => button.dataset.path === result.directory.path)
        ?.focus();
      toast(success);
    } catch (error) {
      if (generation === state.launchBrowseGeneration) {
        $("launch-browser-operation-note").textContent = error.message;
      }
    } finally {
      if (generation === state.launchBrowseGeneration) setLaunchBrowserMutation(false);
    }
  }

  $("launch-browser-new").addEventListener("click", () => openLaunchBrowserOperation("folder"));
  $("launch-browser-clone").addEventListener("click", () => openLaunchBrowserOperation("clone"));
  $("launch-browser-operation-cancel").addEventListener("click", closeLaunchBrowserOperation);
  $("launch-browser-operation-confirm").addEventListener("click", () => { void submitLaunchBrowserOperation(); });
  $("launch-browser-repository").addEventListener("input", () => {
    const destination = $("launch-browser-destination");
    if (destination.dataset.manual !== "true") {
      destination.value = repositoryDestinationName($("launch-browser-repository").value);
    }
  });
  $("launch-browser-destination").addEventListener("input", () => {
    $("launch-browser-destination").dataset.manual = "true";
  });
  $("launch-browser-operation").addEventListener("keydown", (event) => {
    if (event.key !== "Enter" || event.isComposing) return;
    event.preventDefault();
    void submitLaunchBrowserOperation();
  });
  function suggestName(preferences = {}, force = false) {
    if (!force && !state.launchNamePristine && $("launch-name").value) return;
    $("launch-name").value = suggestedSessionName($("launch-directory").value, preferences);
    state.launchNamePristine = true;
  }
  function option(value, label, disabled = false) {
    const node = document.createElement("option");
    node.value = value; node.textContent = label; node.disabled = disabled;
    return node;
  }
  $("launch-form").addEventListener("submit", async (event) => {
    event.preventDefault();
    const button = event.currentTarget.querySelector("button[type=submit]");
    const duplicateFlow = state.launchFlow === "duplicate";
    const launchMachine = currentLaunchMachine();
    let memoryMaxBytes;
    try {
      memoryMaxBytes = parseMemoryLimitSelection(
        launchMachine.memory,
        $("launch-memory").value,
        $("launch-memory-custom").value,
      );
    } catch (error) {
      toast(error.message);
      return;
    }
    const body = {
      name: $("launch-name").value,
      directory: $("launch-directory").value,
      profile_id: $("launch-profile").value,
      mode_id: $("launch-mode").value || null,
      machine: $("launch-machine").value || null,
      resume_session_id: duplicateFlow ? null : ($("launch-session").value || null),
      memory_max_bytes: memoryMaxBytes,
      summarize_pane_id: duplicateFlow && $("launch-summary-resume").checked
        ? (state.launchSummarySourceId || null)
        : null,
    };
    if (body.resume_session_id) {
      const machine = currentLaunchMachine();
      const profile = (machine.profiles || []).find((item) => item.id === body.profile_id);
      const saved = $("launch-session").selectedOptions[0];
      if (!saved || saved.value !== body.resume_session_id || !profile) {
        toast("Saved conversation details changed; choose it again");
        return;
      }
      const confirmed = window.confirm(savedSessionConfirmation({
        machineId: machine.id,
        machineLabel: machine.label || machine.id,
        profileLabel: profile.name || profile.id,
        directory: body.directory,
        harness: saved.dataset.harness || profile.harness || "",
        preview: saved.dataset.preview,
      }));
      if (!confirmed) return;
    }
    const label = button.textContent;
    button.disabled = true;
    // Summarizing runs the chosen profile's CLI before tmux is touched, so the
    // request stays open for as long as that turn takes.
    if (body.summarize_pane_id) button.textContent = "Summarizing previous session…";
    try {
      await request("/api/v1/sessions", { method: "POST", body: JSON.stringify(body) });
      persistLaunchDirectory(body.machine || currentLaunchMachine().id, body.directory);
      state.pendingSelectionName = { name: body.name, machine: body.machine };
      reconcileSelection();
      invalidateLaunchDialog();
      toast(body.summarize_pane_id
        ? `Launched ${body.name} with a summary of the previous session`
        : `Launched ${body.name}${body.machine ? ` on ${body.machine}` : ""}`);
    } catch (error) { toast(error.message); }
    finally { button.disabled = false; button.textContent = label; }
  });
  document.querySelectorAll(".dialog-cancel").forEach((button) => button.addEventListener("click", () => {
    const dialog = button.closest("dialog");
    if (dialog?.id === "launch-dialog") invalidateLaunchDialog();
    else dialog?.close();
  }));
  $("kill-dialog").addEventListener("close", () => { state.pendingKillId = null; });
  $("session-edit-dialog").addEventListener("close", () => { state.pendingSessionEdit = null; });
  $("resume-dialog").addEventListener("close", () => { state.pendingResumeId = null; });
  $("launch-dialog").addEventListener("close", () => {
    const generation = Number($("launch-dialog").dataset.launchGeneration);
    $("launch-dialog").dataset.launchGeneration = "";
    clearLaunchSessions();
    if (generation && generation === state.launchDialogGeneration) {
      invalidateLaunchDialog(false);
    }
  });
  $("launch-dialog").addEventListener("cancel", (event) => {
    event.preventDefault();
    invalidateLaunchDialog();
  });

  document.addEventListener("visibilitychange", () => {
    if (document.hidden) {
      persistBoundComposerDraft(true);
      state.overviewSource?.close();
      state.paneSource?.close();
      state.paneSource = null;
      stopTranscriptPolling();
      state.overviewConnection = "paused";
      stopPulseRefresh();
      stopPulseEvents();
      stopRecoveryPolling();
      stopFleetUpdatePolling();
    } else {
      connectOverview();
      connectPane(false);
      if (state.pulseOpen) {
        void loadPulseAccounts(true);
      }
      void refreshFleetRecovery(false);
      void refreshFleetUpdates();
    }
  });
  window.addEventListener("storage", (event) => {
    if (event.key !== COMPOSER_DRAFT_STORAGE_KEY) return;
    const identity = state.composerDraftIdentity;
    const before = identity ? state.composerDrafts.get(identity.key) : null;
    mergeComposerDraftState(
      state.composerDrafts,
      state.composerDraftTombstones,
      event.newValue,
      Date.now(),
      protectedComposerDraftKeys(),
    );
    syncComposerDraftTimestamp();
    const after = identity ? state.composerDrafts.get(identity.key) : null;
    if (before?.updatedAt === after?.updatedAt && before?.text === after?.text) return;
    const input = $("message");
    input.value = after?.text || "";
    state.composerRevision += 1;
    if (after) {
      try { input.setSelectionRange(after.selectionStart, after.selectionEnd); }
      catch { /* An unfocused mobile textarea can reject selection updates. */ }
    }
  });
  window.addEventListener("pagehide", () => { persistBoundComposerDraft(true); });
  window.addEventListener("pagehide", stopTranscriptPolling);
  window.addEventListener("pagehide", stopAgentEvents);
  document.addEventListener("visibilitychange", () => {
    if (document.hidden) stopAgentEvents();
    else void pollAgentEvents();
  });
  window.addEventListener("pageshow", startTranscriptPolling);
  window.addEventListener("pageshow", () => { void pollAgentEvents(); });

  function stopAgentEvents() {
    clearTimeout(state.agentEventTimer);
    state.agentEventTimer = null;
    state.agentEventController?.abort();
  }

  async function pollAgentEvents() {
    if (document.hidden || !state.agentEventAvailable || state.agentEventController) return;
    clearTimeout(state.agentEventTimer);
    const controller = new AbortController();
    state.agentEventController = controller;
    let delay = 100;
    try {
      const query = new URLSearchParams({ wait: "25", limit: "100" });
      if (state.agentEventCursor) query.set("after", state.agentEventCursor);
      const response = await fetch(`/api/v1/fleet/agent-events?${query}`, { signal: controller.signal });
      if (response.status === 404) { state.agentEventAvailable = false; return; }
      if (!response.ok) throw new Error("Agent events unavailable");
      const page = await response.json();
      state.agentEventStates = reconcileAgentEvents(state.agentEventStates, page);
      state.agentEventCursor = typeof page.next === "string" ? page.next : null;
      render();
    } catch { delay = 5000; }
    finally {
      state.agentEventController = null;
      if (!document.hidden && state.agentEventAvailable) state.agentEventTimer = setTimeout(() => { void pollAgentEvents(); }, delay);
    }
  }

  render();
  void pollAgentEvents();
  connectOverview();
  if (state.historyOpen) void loadSessionHistory();
  void refreshFleetUpdates();
  void refreshFleetRecovery(false);
  void refreshRegistryCapability();
  if (state.selected) connectPane();
  if (state.pulseOpen) void loadPulseAccounts();
}

// Durable lifecycle signals supplement the status classifier. Keep only
// generation-bound state; event bodies are never rendered as HTML.
function reconcileAgentEvents(previous, page) {
  const next = page?.reset ? new Map() : new Map(previous instanceof Map ? previous : []);
  for (const record of Array.isArray(page?.events) ? page.events.slice(0, 100) : []) {
    const event = record?.event;
    if (!event || typeof event.session_key !== "string" || typeof event.machine !== "string") continue;
    const type = event.type;
    if (!["agent.needs_input", "agent.working", "agent.turn_completed", "agent.started", "agent.exited", "session.closed", "session.archived", "session.resumed"].includes(type)) continue;
    next.delete(event.session_key);
    next.set(event.session_key, { instance: event.instance_id, machine: event.machine, type, reason: type === "agent.needs_input" ? event.reason : type === "agent.turn_completed" ? "idle_prompt" : null });
    if (next.size > 4096) next.delete(next.keys().next().value);
  }
  return next;
}

function needsInputReason(session, events) {
  const event = events?.get(session?.session_key);
  if (event && event.machine === session.machine && event.instance === session.instance_id) {
    if (["agent.working", "agent.exited", "session.closed", "session.archived"].includes(event.type)) return null;
    if (["idle_prompt", "question", "permission", "startup_prompt", "plan_approval"].includes(event.reason)) return event.reason;
  }
  return session?.status === "waiting" ? "idle_prompt" : null;
}

function needsInputLabel(reason) {
  const labels = { idle_prompt: "idle", question: "question", permission: "permission", startup_prompt: "startup prompt", plan_approval: "plan approval" };
  return labels[reason] ? `Needs input · ${labels[reason]}` : "";
}

if (typeof module !== "undefined" && module.exports) Object.assign(module.exports, { reconcileAgentEvents, needsInputReason, needsInputLabel });
