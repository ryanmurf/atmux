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
const MAX_COLLAPSED_TOOL_RUN = 24;
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
const MAX_COMPOSER_DRAFT_ENTRIES = 64;
const MAX_COMPOSER_DRAFT_TOMBSTONES = 256;
const MAX_COMPOSER_DRAFT_STORAGE_CHARS = 512 * 1024;
const MAX_COMPOSER_DRAFT_TEXT_CHARS = 65_536;
const COMPOSER_DRAFT_TOMBSTONE_TTL_MS = 7 * 24 * 60 * 60 * 1000;
const PANE_INSTANCE_PATTERN = /^pane-v1-[a-f0-9]{64}$/;
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

function highlightCode(text) {
  const source = String(text || "");
  const pattern = /(\/\/.*$|(?:^|\s)#.*$|"(?:\\.|[^"\\])*"|'(?:\\.|[^'\\])*'|`(?:\\.|[^`\\])*`|\b(?:async|await|break|case|class|const|continue|def|else|enum|false|fn|for|function|if|impl|import|in|let|match|mod|new|null|pub|return|self|static|struct|throw|trait|true|try|type|use|var|while)\b|\b\d+(?:\.\d+)?\b)/gm;
  const segments = [];
  let position = 0;
  for (const match of source.matchAll(pattern)) {
    if (match.index > position) segments.push({ kind: "plain", text: source.slice(position, match.index) });
    const token = match[0];
    const trimmed = token.trimStart();
    const kind = trimmed.startsWith("//") || trimmed.startsWith("#") ? "comment"
      : /^["'`]/.test(trimmed) ? "string"
        : /^\d/.test(trimmed) ? "number" : "keyword";
    segments.push({ kind, text: token });
    position = match.index + token.length;
  }
  if (position < source.length) segments.push({ kind: "plain", text: source.slice(position) });
  return segments;
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
    if (token.type === "text") parent.append(document.createTextNode(token.text));
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
  if (token.type === "text") { parent.append(document.createTextNode(token.text)); return; }
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
      for (const segment of highlightCode(block.text)) {
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
  if (!available) {
    return {
      hash: "",
      transcript: {
        available: false,
        source,
        messages: [],
        truncated: false,
        error: null,
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
    },
  };
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
    const groupKey = internalToolGroupKey(source[index]);
    if (!groupKey) {
      items.push({ kind: "item", message: source[index] }); index += 1; continue;
    }
    let end = index;
    while (end < source.length && internalToolGroupKey(source[end]) === groupKey) end += 1;
    let cursor = index;
    while (cursor < end) {
      const remaining = end - cursor;
      const size = remaining === boundedMax + 1 ? boundedMax - 1 : Math.min(boundedMax, remaining);
      if (size < 2) {
        items.push({ kind: "item", message: source[cursor] }); cursor += 1; continue;
      }
      const grouped = source.slice(cursor, cursor + size);
      const firstId = String(grouped[0]?.id || cursor);
      items.push({
        kind: "tool-group",
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

function toolGroupSummary(group) {
  const calls = group?.messages?.length || 0;
  const counts = group?.counts || [];
  if (counts.length === 1) return `${counts[0].name} ×${calls}`;
  const labels = counts.map(({ name, count }) => `${name} ×${count}`).join(" · ");
  return `${calls} internal calls · ${labels}`;
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

function modelPickerState(session, capabilities, online, switchingPaneId, composerSending = false) {
  const recognized = session?.agent === "claude" || session?.agent === "codex";
  const matches = capabilities?.pane_id === session?.id;
  const models = matches && Array.isArray(capabilities.models) ? capabilities.models : [];
  const current = matches && typeof capabilities.current === "string" ? capabilities.current : "";
  const effort = matches && typeof capabilities.effort === "string" ? capabilities.effort : "";
  const currentMode = matches && typeof capabilities.current_mode === "string" ? capabilities.current_mode : "";
  const busy = Boolean(switchingPaneId);
  return {
    visible: recognized,
    loading: recognized && !matches,
    current,
    effort,
    currentMode,
    models,
    disabled: !online || busy || composerSending || !models.some((model) => model.switchable),
    status: !recognized ? ""
      : !online ? "Machine offline"
        : busy ? (switchingPaneId === session?.id ? "Switching…" : "Another model switch is in progress")
          : !matches ? "Checking models…"
            : capabilities.note || (current ? `Current: ${[current, effort].filter(Boolean).join(" · ")}` : "Current model unavailable"),
  };
}

function claudeResumeState(session, capabilities, online, resumingPaneId, composerSending = false) {
  const isClaude = session?.agent === "claude";
  const matches = capabilities?.pane_id === session?.id;
  const available = matches && capabilities?.resume_available === true;
  const note = matches && typeof capabilities?.resume_note === "string" ? capabilities.resume_note : "";
  const restarting = Boolean(resumingPaneId);
  return {
    visible: isClaude,
    available,
    disabled: !online || restarting || composerSending || !available,
    status: !isClaude ? ""
      : !online ? "Machine offline"
        : restarting ? (resumingPaneId === session?.id ? "Relaunching Claude…" : "Another Claude relaunch is in progress")
          : !matches ? "Checking resume…"
            : note || (available ? "Ready to relaunch the saved conversation" : "Claude resume is unavailable"),
  };
}

function followsLiveTail(element, tolerance = 16) {
  return element.scrollHeight - element.scrollTop - element.clientHeight <= tolerance;
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

function appRoute(urlValue) {
  const url = urlValue instanceof URL ? urlValue : new URL(String(urlValue), "https://atmux.invalid/");
  const session = url.searchParams.get("session");
  if (session) return { view: "session", id: session };
  const machine = url.searchParams.get("machine");
  if (machine) return { view: "machine", id: machine };
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

if (typeof module !== "undefined" && module.exports) {
  module.exports = {
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
    filterDirectories,
    defaultMemoryLimitLabel,
    formatMemoryLimit,
    memoryLimitChoices,
    parseMemoryLimitSelection,
    formatRelativeTime,
    groupSessionsByMachine,
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
    profilesForHarness,
    projectLabel,
    projectPreference,
    preferredLaunchMachineId,
    reconcileSessions,
    reduceOverview,
    reduceTranscript,
    sessionDeletePath,
    modelPickerState,
    claudeResumeState,
    followsLiveTail,
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
    toolGroupSummary,
    diffLineKind,
    safeLinkUrl,
    sortSessions,
    presentSessionStatuses,
    WORKING_TO_WAITING_HOLD_MS,
    suggestedSessionName,
    utf8ByteLength,
    validateImageSelection,
    validContentHash,
  };
}

if (typeof document !== "undefined") initialize();

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
  const requestedPulseAccount = pulseAccountId(pageUrl.searchParams.get("pulseAccount"));
  const state = {
    revision: 0,
    sessions: new Map(),
    machines: [],
    selected: initialRoute.view === "session" ? initialRoute.id : null,
    selectedMachine: initialRoute.view === "machine" ? initialRoute.id : null,
    paneLines: [],
    paneRevision: 0,
    overviewSource: null,
    paneSource: null,
    overviewConnection: "connecting",
    statusPresentations: new Map(),
    statusTimer: null,
    filter: "",
    health: null,
    pendingSelectionName: null,
    launchOptions: null,
    launchNamePristine: true,
    rememberedLaunchDirectories: storedLaunchDirectories,
    launchBrowseGeneration: 0,
    launchBrowseMutation: false,
    launchDialogGeneration: 0,
    launchFlow: null,
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
    transcriptTimer: null,
    transcriptPoll: null,
    transcriptRequest: 0,
    transcriptPointerDown: false,
    pendingTranscriptRender: false,
    pendingTranscriptFilterChange: false,
    transcriptFollowing: true,
    transcriptExpectedScrollTop: null,
    conversationVisibility: storedConversationVisibility,
    viewMode: "conversation",
    projectView: null,
    filesRequest: 0,
    fileSaveRequest: 0,
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
    inFlightComposerText: null,
    inFlightComposerIdentity: null,
    composerRevision: 0,
    queuedComposerMessages: [],
    attachments: [],
    attachmentPaneId: null,
    attachmentInstanceKey: null,
    pendingKillId: null,
    pendingResumeId: null,
    paneModels: null,
    paneModelsRequest: 0,
    modelSwitchingPaneId: null,
    duplicatingPaneId: null,
    resumingPaneId: null,
    recoveryStatus: null,
    recoveryLoading: false,
    recoveryPoll: null,
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
      document.documentElement.style.setProperty("--app-height", `${Math.floor(height)}px`);
      revealFocusedLaunchMemoryControl();
    }
  }

  window.addEventListener("resize", syncMobileViewport, { passive: true });
  window.addEventListener("orientationchange", syncMobileViewport, { passive: true });
  syncMobileViewport();

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
      return;
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
    setHealth(data.health);
    reconcileSelection();
    render();
  }

  function machineOf(session) {
    if (!session) return null;
    const localMachineId = state.machines.find((machine) => machine.kind === "local")?.id || "local";
    const id = sessionMachineId(session, localMachineId);
    return state.machines.find((machine) => machine.id === id) || null;
  }

  function connectOverview() {
    state.overviewSource?.close();
    state.overviewConnection = "connecting";
    renderCounts();
    const source = new EventSource("/api/v1/events");
    state.overviewSource = source;
    source.onopen = () => {
      state.overviewConnection = "live";
      renderCounts();
    };
    source.addEventListener("sessions.snapshot", (event) => {
      const data = parseEvent(event); if (data) applyOverview(data);
    });
    source.addEventListener("sessions.patch", (event) => {
      const data = parseEvent(event); if (data) applyOverview(data);
    });
    source.addEventListener("protocol.error", (event) => setHealth(event.data || "stream protocol error"));
    source.onerror = () => {
      state.overviewConnection = "reconnecting";
      renderCounts();
    };
  }

  function resetProjectView() {
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
    state.paneSource?.close();
    if (resetProject) resetProjectView();
    state.paneSource = null;
    state.paneLines = [];
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
    state.transcriptRequest += 1;
    state.transcriptPointerDown = false;
    state.pendingTranscriptRender = false;
    state.pendingTranscriptFilterChange = false;
    state.transcriptFollowing = true;
    state.transcriptExpectedScrollTop = null;
    clearTimeout(state.transcriptTimer);
    clearInterval(state.transcriptPoll);
    state.transcriptTimer = null;
    state.transcriptPoll = null;
    pane.textContent = "";
    conversation.replaceChildren();
    if (!state.selected || document.hidden) return;
    void refreshModels(state.selected);
    // The branch belongs in the agent header, so discover it in the
    // background without making the reader open the Git tab first.
    void loadGitSummary();
    $("stream-state").textContent = "Connecting…";
    scheduleTranscript(0);
    state.transcriptPoll = setInterval(() => scheduleTranscript(0), 2500);
    const source = new EventSource(`/api/v1/panes/${encodeURIComponent(state.selected)}/events`);
    state.paneSource = source;
    source.addEventListener("pane.snapshot", (event) => {
      const data = parseEvent(event); if (!data) return;
      state.paneLines = contentToLines(data.content);
      state.paneRevision = data.revision;
      state.paneError = null;
      drawPane(true);
      scheduleTranscript(100);
      $("stream-state").textContent = "Live";
      render();
    });
    source.addEventListener("pane.patch", (event) => {
      const data = parseEvent(event); if (!data) return;
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
      forgetComposerDraft(selectedComposerDraftIdentity(), true);
      selectSession(null, "replace");
    });
    // A failure on the owning machine belongs to this pane, not to the local
    // tmux monitor, so it never touches the global health alert.
    source.addEventListener("pane.error", (event) => {
      const data = parseEvent(event); if (!data) return;
      state.paneError = data;
      $("stream-state").textContent = paneErrorLabel(data.kind);
      render();
    });
    source.addEventListener("protocol.error", (event) => {
      state.paneError = { error: event.data || "stream protocol error", kind: "protocol" };
      $("stream-state").textContent = paneErrorLabel("protocol");
      render();
    });
    source.onerror = () => { $("stream-state").textContent = "Reconnecting…"; };
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
        note: error.message,
      };
    }
    render();
  }

  function scheduleTranscript(delay = 300) {
    clearTimeout(state.transcriptTimer);
    if (!state.selected || document.hidden) return;
    state.transcriptTimer = setTimeout(() => { void refreshTranscript(); }, delay);
  }

  async function refreshTranscript() {
    const paneId = state.selected;
    if (!paneId) return;
    const generation = ++state.transcriptRequest;
    const suffix = state.transcriptHash ? `?known_hash=${encodeURIComponent(state.transcriptHash)}` : "";
    try {
      const data = await request(`/api/v1/panes/${encodeURIComponent(paneId)}/transcript${suffix}`);
      if (state.selected !== paneId || generation !== state.transcriptRequest) return;
      const next = reduceTranscript(state.transcript, data);
      const shouldDraw = next.transcript.messages !== state.transcript.messages
        || next.transcript.available !== state.transcript.available
        || next.transcript.source !== state.transcript.source
        || next.transcript.truncated !== state.transcript.truncated
        || Boolean(state.transcript.error);
      state.transcriptHash = next.hash;
      state.transcript = next.transcript;
      if (shouldDraw) drawConversation();
      renderViewMode();
    } catch (error) {
      if (state.selected !== paneId || generation !== state.transcriptRequest) return;
      state.transcript.error = error.message;
      drawConversation();
    }
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
    summary.textContent = [name, suffix].filter(Boolean).join(" · ");
    details.append(summary);
    const body = document.createElement("div");
    body.className = "tool-body";
    for (const [label, value] of [["Input", message.tool_input], ["Result", message.tool_output]]) {
      if (!value) continue;
      const section = document.createElement("section");
      const heading = document.createElement("span"); heading.textContent = label;
      const pre = document.createElement("pre"); pre.textContent = value;
      section.append(heading, pre); body.append(section);
    }
    if (body.childNodes.length) details.append(body);
    return details;
  }

  function renderToolGroup(group, expandedTools) {
    const details = document.createElement("details");
    details.className = "tool-card tool-call-group";
    details.dataset.transcriptId = group.id;
    details.dataset.transcriptMembers = JSON.stringify(
      group.messages.map((message) => String(message?.id || "")).filter(Boolean),
    );
    details.dataset.transcriptVisibility = "internal";
    details.open = expandedTools.has(group.id);
    const summary = document.createElement("summary");
    summary.className = "tool-call-group-summary";
    summary.textContent = toolGroupSummary(group);
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

  function drawConversation(filterChanged = false) {
    if (state.transcriptPointerDown || selectionTouchesPane(conversation, window.getSelection())) {
      state.pendingTranscriptRender = true;
      state.pendingTranscriptFilterChange ||= filterChanged;
      return;
    }
    const shouldFollow = state.transcriptFollowing
      && followsLiveTail(conversation, LIVE_TAIL_TOLERANCE);
    const readingOffset = conversation.scrollTop;
    const retainAfterFilter = filterChanged
      ? (node) => node.dataset.transcriptVisibility === "agent"
        || (node.dataset.transcriptVisibility === "human" && state.conversationVisibility.human)
        || (node.dataset.transcriptVisibility === "internal" && state.conversationVisibility.internal)
      : null;
    const readingAnchor = shouldFollow
      ? null
      : transcriptReadingAnchor(conversation, retainAfterFilter);
    const expandedTools = new Set(
      [...conversation.querySelectorAll("details.tool-card[open]")]
        .map((node) => node.dataset.transcriptId)
        .filter(Boolean),
    );
    const nodes = [];
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
      if (item.kind === "tool-group") {
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
      const visibility = transcriptVisibilityKind(message);
      if (visibility === "internal" && typeof message.markdown !== "string") continue;
      const article = document.createElement("article");
      article.className = `message-card ${String(message.role || "internal")} ${visibility}`;
      article.dataset.transcriptId = String(message.id || "");
      article.dataset.transcriptVisibility = visibility;
      const label = document.createElement("header");
      label.textContent = visibility === "human" ? "You"
        : visibility === "agent" ? "Agent" : "Internal";
      const body = document.createElement("div");
      body.className = "markdown-body";
      body.append(markdownFragment(message.markdown));
      article.append(label, body); nodes.push(article);
      renderedMessages += 1;
    }
    const hasOnlyNotice = nodes.length === 1 && nodes[0].classList.contains("transcript-notice");
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
          : "No agent session log is mapped yet. Raw pane remains available.");
      nodes.push(empty);
    }
    conversation.replaceChildren(...nodes);
    state.pendingTranscriptRender = false;
    state.pendingTranscriptFilterChange = false;
    // Stream updates replace transcript cards wholesale. Following is an
    // explicit reader choice, not merely a position that happens to be near
    // the tail. When reading, anchor the same transcript item in the viewport.
    if (shouldFollow) conversation.scrollTop = conversation.scrollHeight;
    else restoreTranscriptReadingAnchor(conversation, readingAnchor, readingOffset);
    state.transcriptFollowing = shouldFollow;
    state.transcriptExpectedScrollTop = conversation.scrollTop;
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

  function appendSource(parent, content, language, diff = false, onSelectLine = null, selection = null) {
    const source = String(content || "").slice(0, MAX_PROJECT_SOURCE_CHARS);
    const code = document.createElement("pre");
    code.className = "code-source";
    const lines = source.split("\n").slice(0, MAX_PROJECT_SOURCE_LINES);
    for (let index = 0; index < lines.length; index += 1) {
      const row = document.createElement("span");
      const diffKind = diff ? diffLineKind(lines[index]) : null;
      row.className = `code-line${diffKind && diffKind !== "context" ? ` diff-line-${diffKind}` : ""}`;
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
        for (const segment of highlightCode(lines[index], language)) {
          const token = document.createElement("span");
          if (segment.kind !== "plain") token.className = `syntax-${segment.kind}`;
          token.textContent = segment.text;
          lineContent.append(token);
        }
      }
      row.append(lineContent);
      code.append(row);
    }
    parent.append(code);
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
          } else void loadProjectFile(entry.path);
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
        appendSource(
          viewer,
          files.file.content,
          files.file.language,
          false,
          (line, extend) => updateFileSelection(view, line, extend),
          files.selection,
        );
        if (files.file.truncated) viewer.append(projectStateNode("Preview truncated at the safe display limit."));
      }
    }
    restoreProjectScroll("files", view.paneId, files.file?.path || "");
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

  async function loadProjectFile(path) {
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
    pane.hidden = !raw;
    conversation.hidden = state.viewMode !== "conversation";
    $("conversation-filters-open").hidden = state.viewMode !== "conversation";
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
    const alert = $("health-alert");
    alert.textContent = normalized ? `tmux monitor: ${normalized}` : "";
    alert.hidden = !normalized;
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
    const changed = state.selected !== id || state.selectedMachine !== null || state.pulseOpen;
    const paneChanged = state.selected !== id;
    if (changed && !confirmDiscardFileEdit()) return false;
    if (changed) {
      persistBoundComposerDraft(true);
      invalidateLaunchDialog();
    }
    state.selected = id;
    state.selectedMachine = null;
    state.pulseOpen = false;
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
    const changed = state.selected !== null || state.selectedMachine !== id || state.pulseOpen;
    if (changed && !confirmDiscardFileEdit()) return false;
    if (changed) {
      persistBoundComposerDraft(true);
      invalidateLaunchDialog();
    }
    state.selected = null;
    state.selectedMachine = id;
    state.pulseOpen = false;
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
    clearTimeout(state.transcriptTimer);
    clearInterval(state.transcriptPoll);
    state.transcriptTimer = null;
    state.transcriptPoll = null;
    render();
    return true;
  }

  function selectPulse(open, historyMode = "push") {
    const nextOpen = Boolean(open);
    const changed = state.selected !== null || state.selectedMachine !== null || state.pulseOpen !== nextOpen;
    if (changed && !confirmDiscardFileEdit()) return false;
    if (changed) {
      persistBoundComposerDraft(true);
      invalidateLaunchDialog();
    }
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
    clearTimeout(state.transcriptTimer);
    clearInterval(state.transcriptPoll);
    state.transcriptTimer = null;
    state.transcriptPoll = null;
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
        : route.view === "usage" ? selectPulse(true, "none")
          : selectSession(null, "none");
    if (accepted === false) {
      const url = new URL(location.href);
      url.searchParams.delete("session"); url.searchParams.delete("machine"); url.searchParams.delete("view");
      if (state.selected) url.searchParams.set("session", state.selected);
      else if (state.selectedMachine) url.searchParams.set("machine", state.selectedMachine);
      else if (state.pulseOpen) url.searchParams.set("view", "usage");
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
    renderAttachments();

    const query = state.filter.toLowerCase();
    const visible = sessions.filter((session) =>
      !query || `${session.name} ${session.path} ${session.agent} ${session.profile || ""} ${sessionMachineId(session)}`.toLowerCase().includes(query));
    const groups = groupSessionsByMachine(visible, state.machines)
      .filter((group) => group.sessions.length > 0 || !query);
    reconcileRows(groups);
    $("empty").hidden = visible.length > 0;

    const selected = state.sessions.get(state.selected);
    const selectedMachine = state.machines.find((machine) => machine.id === state.selectedMachine) || null;
    $("welcome").hidden = Boolean(selected || selectedMachine || state.pulseOpen);
    $("machine-view").hidden = !selectedMachine || Boolean(selected);
    $("agent-view").hidden = !selected;
    $("pulse-view").hidden = !state.pulseOpen;
    $("pulse-open").classList.toggle("selected", state.pulseOpen);
    $("pulse-open").setAttribute("aria-pressed", String(state.pulseOpen));
    document.body.classList.toggle("has-selection", Boolean(selected || selectedMachine || state.pulseOpen));
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
    const launch = $("agent-launch");
    launch.hidden = !launchCommand;
    launch.textContent = launchCommand ? `tmux: ${launchCommand}` : "";
    renderModelControl(selected, controllable);
    renderClaudeResumeAction(selected, controllable);
    const resuming = Boolean(state.resumingPaneId);
    const preparingDuplicate = Boolean(state.duplicatingPaneId);
    for (const id of ["tmux-prefix-twice", "interrupt", "kill-open", "attach", "quick-actions-open", "quick-duplicate", "quick-compact", "quick-tmux-prefix-twice", "quick-interrupt", "quick-kill-open"]) $(id).disabled = !controllable || state.composerSending || resuming || preparingDuplicate;
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

  function renderRecoveryControl() {
    const machine = state.machines.find((candidate) => candidate.id === "tron") || null;
    const button = $("recovery-open");
    button.hidden = !machine;
    if (!machine) return;
    const running = state.recoveryStatus?.phase === "running";
    button.disabled = !isMachineControllable(machine) || state.recoveryLoading || running;
    button.textContent = running ? "Resuming…" : "Quick resume";
    const status = $("recovery-status");
    status.textContent = state.recoveryStatus?.message || "Ready to restore Tron's saved session roster.";
    $("recovery-confirm").disabled = state.recoveryLoading || running || state.recoveryStatus?.available === false;
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
    const options = [...view.models];
    if (view.current && !view.currentMode) {
      options.unshift({ id: "", label: `${view.current} (current; no configured mode)`, switchable: false });
    }
    const signature = JSON.stringify(options);
    for (const [selectId, statusId] of [["agent-model", "model-status"], ["quick-agent-model", "quick-model-status"]]) {
      const select = $(selectId);
      const status = $(statusId);
      if (select.dataset.models !== signature) {
        select.replaceChildren(...(options.length
          ? options.map((model) => option(model.id, model.label, !model.switchable))
          : [option("", "No switchable models", true)]));
        select.dataset.models = signature;
      }
      if (view.currentMode && options.some((model) => model.id === view.currentMode)) {
        select.value = view.currentMode;
      }
      select.disabled = view.disabled;
      status.textContent = view.status;
      status.title = view.status;
    }
  }

  function renderClaudeResumeAction(session, controllable) {
    const button = $("quick-resume");
    const note = $("quick-resume-note");
    const view = claudeResumeState(
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
    const header = document.createElement("button");
    header.type = "button";
    header.className = "machine-header";
    header.addEventListener("click", () => selectMachine(machine.id));
    const dot = textSpan("", "machine-dot");
    dot.setAttribute("aria-hidden", "true");
    const label = textSpan("", "machine-label");
    const status = textSpan("", "machine-status");
    header.append(dot, label, status);
    li.append(header);
    return { li, header, dot, label, status, machineId: machine.id };
  }

  function updateMachineNode(node, machine) {
    const online = isMachineControllable(machine);
    node.header.className = `machine-header ${online ? "online" : "offline"}${state.selectedMachine === machine.id ? " selected" : ""}`;
    node.dot.textContent = online ? "◉" : "○";
    node.label.textContent = machine.label || machine.id;
    node.status.textContent = machineStatusLabel(machine, Date.now());
    node.li.setAttribute(
      "aria-label",
      `${machine.label || machine.id}, ${online ? "online" : "offline"}`,
    );
  }

  /// Reconciles machine headers and session buttons in place. Nodes are reused
  /// so streaming updates never destroy focus or scroll position.
  function reconcileRows(groups) {
    const desired = [];
    for (const group of groups) {
      desired.push({ kind: "machine", key: `m:${group.machine.id}`, machine: group.machine });
      for (const session of group.sessions) {
        desired.push({ kind: "session", key: `s:${session.id}`, session });
      }
    }
    const keys = new Set(desired.map((row) => row.key));
    const focusedId = document.activeElement?.dataset?.sessionId;
    const focusedAction = document.activeElement?.dataset?.sessionAction;
    let cursor = sessionList.firstElementChild;
    for (const row of desired) {
      let node;
      if (row.kind === "machine") {
        node = machineNodes.get(row.machine.id);
        if (!node) {
          node = createMachineNode(row.machine);
          machineNodes.set(row.machine.id, node);
        }
        updateMachineNode(node, row.machine);
      } else {
        node = sessionNodes.get(row.session.id);
        if (!node) {
          node = createSessionNode(row.session.id);
          sessionNodes.set(row.session.id, node);
        }
        updateSessionNode(node, row.session);
      }
      node.li.dataset.rowKey = row.key;
      if (node.li === cursor) cursor = cursor.nextElementSibling;
      else sessionList.insertBefore(node.li, cursor);
    }
    for (const child of [...sessionList.children]) {
      if (!keys.has(child.dataset.rowKey)) child.remove();
    }
    for (const id of sessionNodes.keys()) {
      if (!state.sessions.has(id)) sessionNodes.delete(id);
    }
    for (const id of machineNodes.keys()) {
      if (!state.machines.some((machine) => machine.id === id)) machineNodes.delete(id);
    }
    const focusedNode = focusedId && sessionNodes.get(focusedId);
    const focused = focusedAction === "delete" ? focusedNode?.deleteButton : focusedNode?.button;
    if (focused?.isConnected && document.activeElement !== focused) focused.focus({ preventScroll: true });
  }

  function renderCounts(sessions = [...state.sessions.values()]) {
    const counts = $("counts");
    if (state.overviewConnection !== "live") {
      counts.textContent = state.overviewConnection === "connecting" ? "Connecting…" : "Reconnecting…";
      return;
    }
    const working = sessions.filter((item) => item.status === "working").length;
    const waiting = sessions.filter((item) => item.status === "waiting").length;
    counts.replaceChildren(
      textSpan(`● ${working} working`, "count-working"),
      textSpan(`◆ ${waiting} waiting`, "count-waiting"),
    );
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
    const sub = textSpan("", "session-sub");
    copy.append(name, sub);
    button.append(dot, copy);
    const deleteButton = document.createElement("button");
    deleteButton.type = "button";
    deleteButton.className = "session-delete";
    deleteButton.dataset.sessionId = id;
    deleteButton.dataset.sessionAction = "delete";
    deleteButton.textContent = "🗑";
    deleteButton.title = "Kill this session";
    deleteButton.addEventListener("click", () => openKillDialog(id));
    li.append(button, deleteButton);
    return { li, button, deleteButton, dot, name, sub };
  }

  function updateSessionNode(node, session) {
    const selected = session.id === state.selected;
    node.button.className = `session-button ${session.status}${selected ? " selected" : ""}`;
    node.button.setAttribute("aria-current", selected ? "true" : "false");
    const folder = sessionFolderLabel(session);
    const profile = sessionProfileLabel(session);
    node.button.setAttribute("aria-label", [session.name, folder, profile, session.status, session.agent].filter(Boolean).join(", "));
    node.deleteButton.setAttribute("aria-label", `Kill ${session.name}`);
    node.deleteButton.disabled = !isMachineControllable(machineOf(session));
    node.dot.textContent = session.status === "working" ? "●" : session.status === "waiting" ? "◆" : "○";
    node.name.textContent = session.name;
    node.sub.textContent = [folder, profile, session.status, session.agent].filter(Boolean).join(" · ");
    node.sub.title = session.path || "";
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
    if (event.isComposing || event.ctrlKey || event.metaKey || event.altKey || event.shiftKey) return false;
    if (event.key !== "ArrowUp" && event.key !== "ArrowDown") return false;
    const input = $("message");
    if (!fromPane) {
      const start = input.selectionStart ?? input.value.length;
      const end = input.selectionEnd ?? start;
      if (start !== end) return false;
      if (event.key === "ArrowUp" && start !== 0) return false;
      if (event.key === "ArrowDown" && end !== input.value.length) return false;
    }
    const handled = browseMessageHistory(event.key === "ArrowUp" ? "up" : "down");
    if (handled && fromPane) input.focus({ preventScroll: true });
    return handled;
  }

  $("filter").addEventListener("input", (event) => { state.filter = event.target.value; render(); });
  $("rail-toggle").addEventListener("click", () => setRailCollapsed(!state.railCollapsed));
  $("pulse-open").addEventListener("click", () => selectPulse(!state.pulseOpen));
  function stopRecoveryPolling() {
    if (state.recoveryPoll !== null) clearTimeout(state.recoveryPoll);
    state.recoveryPoll = null;
  }
  async function refreshRecoveryStatus(showDialog = false) {
    const machine = state.machines.find((candidate) => candidate.id === "tron") || null;
    if (!machine || !isMachineControllable(machine)) {
      toast("Tron is offline");
      return null;
    }
    state.recoveryLoading = true;
    renderRecoveryControl();
    try {
      const status = await request("/api/v1/machines/tron/quick-resume");
      state.recoveryStatus = status;
      if (showDialog && !$("recovery-dialog").open) $("recovery-dialog").showModal();
      if (status.phase === "running") {
        stopRecoveryPolling();
        state.recoveryPoll = setTimeout(() => { void refreshRecoveryStatus(false); }, 2000);
      } else {
        stopRecoveryPolling();
      }
      return status;
    } catch (error) {
      stopRecoveryPolling();
      toast(error.message);
      return null;
    } finally {
      state.recoveryLoading = false;
      renderRecoveryControl();
    }
  }
  $("recovery-open").addEventListener("click", () => { void refreshRecoveryStatus(true); });
  $("recovery-confirm").addEventListener("click", async () => {
    if (state.recoveryLoading || state.recoveryStatus?.phase === "running") return;
    state.recoveryLoading = true;
    renderRecoveryControl();
    try {
      state.recoveryStatus = await request("/api/v1/machines/tron/quick-resume", {
        method: "POST",
        body: JSON.stringify({}),
      });
      toast("Tron recovery started");
      stopRecoveryPolling();
      state.recoveryPoll = setTimeout(() => { void refreshRecoveryStatus(false); }, 1000);
    } catch (error) {
      toast(error.message);
    } finally {
      state.recoveryLoading = false;
      renderRecoveryControl();
    }
  });
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
    state.messageHistory.delete(identity.key);
    if (state.messageHistoryNavigation?.draftKey === identity.key) {
      state.messageHistoryNavigation = null;
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
  async function switchAgentModel(modeId) {
    const paneId = state.selected;
    const sessionName = state.sessions.get(paneId)?.name || paneId;
    if (!paneId || !modeId || state.modelSwitchingPaneId) return;
    if (state.paneModels?.pane_id === paneId && state.paneModels.current_mode === modeId) return;
    state.modelSwitchingPaneId = paneId;
    render();
    try {
      await request(`/api/v1/panes/${encodeURIComponent(paneId)}/model`, {
        method: "POST",
        body: JSON.stringify({ mode_id: modeId }),
      });
      const choice = state.paneModels?.models?.find((item) => item.id === modeId);
      const warning = state.sessions.get(paneId)?.agent === "claude" && choice?.effort
        ? " Claude saves this effort as the profile default."
        : "";
      toast(`Switched ${sessionName} to ${choice?.label || modeId}.${warning}`);
    } catch (error) {
      toast(error.message);
    } finally {
      state.modelSwitchingPaneId = null;
      if (state.selected === paneId) await refreshModels(paneId);
      render();
    }
  }
  $("agent-model").addEventListener("change", (event) => { void switchAgentModel(event.currentTarget.value); });
  $("quick-agent-model").addEventListener("change", (event) => { void switchAgentModel(event.currentTarget.value); });
  $("quick-actions-open").addEventListener("click", () => {
    const dialog = $("quick-actions-dialog");
    if (!dialog.open) {
      dialog.showModal();
      $("quick-actions-open").setAttribute("aria-expanded", "true");
    }
  });
  $("quick-actions-dialog").addEventListener("close", () => {
    $("quick-actions-open").setAttribute("aria-expanded", "false");
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
  function openResumeDialog() {
    const session = state.sessions.get(state.selected);
    const view = claudeResumeState(
      session,
      state.paneModels,
      isMachineControllable(machineOf(session)),
      state.resumingPaneId,
      state.composerSending,
    );
    if (!session || !view.available || view.disabled) {
      toast(view.status || "Claude resume is unavailable");
      return;
    }
    state.pendingResumeId = session.id;
    $("quick-actions-dialog").close();
    $("resume-dialog").showModal();
  }
  $("quick-resume").addEventListener("click", openResumeDialog);
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
  $("tmux-prefix-twice").addEventListener("click", async () => {
    const paneId = state.selected;
    if (!paneId) return;
    const button = $("tmux-prefix-twice"); button.disabled = true;
    try {
      await request(`/api/v1/panes/${encodeURIComponent(paneId)}/special-keys`, { method: "POST", body: JSON.stringify({ action: "tmux_prefix_twice" }) });
      toast("Sent Ctrl+B twice");
    } catch (error) { toast(error.message); }
    finally { render(); }
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
  conversation.addEventListener("pointerdown", () => {
    state.transcriptPointerDown = true;
    state.transcriptFollowing = false;
  });
  pane.addEventListener("wheel", () => { state.paneFollowing = false; }, { passive: true });
  pane.addEventListener("touchstart", () => { state.paneFollowing = false; }, { passive: true });
  conversation.addEventListener("wheel", () => { state.transcriptFollowing = false; }, { passive: true });
  conversation.addEventListener("touchstart", () => { state.transcriptFollowing = false; }, { passive: true });
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
    if (scrollMatchesExpectedPosition(conversation, state.transcriptExpectedScrollTop)) {
      state.transcriptExpectedScrollTop = null;
      return;
    }
    state.transcriptExpectedScrollTop = null;
    state.transcriptFollowing = followsLiveTail(conversation, LIVE_TAIL_TOLERANCE);
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
    const target = state.pendingResumeId;
    if (!target || state.resumingPaneId) return;
    const button = $("resume-confirm");
    state.resumingPaneId = target;
    button.disabled = true;
    render();
    try {
      await request(`/api/v1/panes/${encodeURIComponent(target)}/resume`, {
        method: "POST",
        body: JSON.stringify({}),
      });
      $("resume-dialog").close();
      toast("Claude relaunched and resumed");
    } catch (error) {
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

  $("launch-machine").addEventListener("change", applyLaunchMachine);
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
    button.disabled = true;
    try {
      await request("/api/v1/sessions", { method: "POST", body: JSON.stringify(body) });
      persistLaunchDirectory(body.machine || currentLaunchMachine().id, body.directory);
      state.pendingSelectionName = { name: body.name, machine: body.machine };
      reconcileSelection();
      invalidateLaunchDialog();
      toast(`Launched ${body.name}${body.machine ? ` on ${body.machine}` : ""}`);
    } catch (error) { toast(error.message); }
    finally { button.disabled = false; }
  });
  document.querySelectorAll(".dialog-cancel").forEach((button) => button.addEventListener("click", () => {
    const dialog = button.closest("dialog");
    if (dialog?.id === "launch-dialog") invalidateLaunchDialog();
    else dialog?.close();
  }));
  $("kill-dialog").addEventListener("close", () => { state.pendingKillId = null; });
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
      state.overviewConnection = "paused";
      stopPulseRefresh();
      stopPulseEvents();
      stopRecoveryPolling();
    } else {
      connectOverview();
      connectPane(false);
      if (state.pulseOpen) {
        void loadPulseAccounts(true);
      }
      if (state.recoveryStatus?.phase === "running") void refreshRecoveryStatus(false);
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

  render();
  connectOverview();
  if (state.selected) connectPane();
  if (state.pulseOpen) void loadPulseAccounts();
}
