# Full-width stable agent pane

Status: completed

## Acceptance criteria

- Conversation messages and tool calls extend across the available agent pane instead of sitting
  in centered maximum-width boxes.
- Expanding or collapsing the left navigation keeps conversation content left-anchored; added
  width appears on the right rather than making cards jump sideways.
- Scrollbar and streaming updates do not create avoidable horizontal layout shifts.
- Desktop and mobile layouts remain usable.

## Gates

- [x] Implementation
- [x] Focused browser regression test
- [x] Desktop/mobile live verification
- [x] Fable/Claude Max review
- [x] Independent security review

## Verification evidence

- Headless Chrome at 1440×1000 shows full-width, left-anchored agent content with the rail open.
- Headless Chrome at 390×844 shows the constrained mobile header, full-width conversation and
  composer, wrapped image/compact/talk controls, and an on-screen Send button.
- The browser suite includes regression assertions for full-width rows, stable scroll geometry,
  the mobile grid header, and viewport-contained wrapping controls.
- Fable/Claude Max and the independent security reviewer both returned `SAFE` on the final frozen
  snapshot; the independent reviewer also repeated the 390×844 mobile rendering check.
