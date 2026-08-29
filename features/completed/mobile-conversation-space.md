# Mobile conversation space

Status: completed

## Request

On mobile web, compact the surrounding agent controls so Conversation or Raw pane receives most
of the usable viewport.

## Acceptance

- The selected-agent top bar, profile/model metadata, actions, mode switch, and composer use compact
  mobile-only sizing without changing desktop layout.
- Agent actions remain horizontally reachable and do not wrap into several tall rows.
- The composer remains usable for text, images, speech, and Send while consuming less idle height.
- Focusing the composer on iOS does not trigger Safari's sub-16px form-control zoom, and the layout
  follows the visual viewport while the software keyboard is open.
- The entire composer remains inside the visible viewport without changing header/action layout at
  focus time; this prevents Safari from first panning the box to the top and then moving it again.
- At a 390x844 mobile viewport, the Conversation/Raw shell receives at least 45% of the usable
  viewport when no attachment tray or error is open.

## Completion gate

- [x] Implemented
- [x] Unit/browser tested
- [x] Integration tested
- [x] Prior compact-layout delta independently reviewed

## Verification

- Browser CSS tests cover compact controls and horizontally reachable actions.
- A real headless 390x844 browser proves the Conversation/Raw shell receives at least 45% of the
  viewport.
- The browser regression proves focusing the 16px composer causes zero initial movement, then
  shrinks the visual viewport to a keyboard-sized 390x430 and proves the complete composer remains
  on-screen.
- Focus itself performs no viewport write; only `resize`/orientation/visual-viewport events update
  the app height, avoiding Safari's intermediate focus geometry.
- Two independent read-only reviews returned SAFE for the prior compact-layout delta on 2026-08-09;
  the keyboard/visual-viewport follow-up is covered by the focused browser regression above.
