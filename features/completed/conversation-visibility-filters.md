# Conversation visibility filters

## Outcome

- [x] Conversation keeps Agent prose visible while Human messages and Internal
  tool/status/coordination activity can be hidden independently.
- [x] A compact `Show` control shares the existing view-tab row and indicates
  whether one or two message types are hidden; `Show all` is always available.
- [x] Filters apply before `exec ×N` grouping, persist across pane changes,
  reconnects, and reloads, and tolerate unavailable browser storage.
- [x] Filtered-empty conversations explain how to recover their hidden content.
- [x] Mobile controls have touch-sized targets without a second header row or
  horizontal document overflow.

## Verification

Node unit coverage exercises defaults, both independent toggles, agent-only
mode, malformed and failing storage, classification, and grouping counts. The
390×844 Chrome integration covers the live disclosure, accessibility geometry,
reading anchors, incoming messages, tool errors, XSS-safe rendering, reconnect,
reload persistence, pane changes, the empty state, and reset.
