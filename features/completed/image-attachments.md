# Image attachments for Claude and Codex

Status: completed

## Acceptance criteria

- The web composer accepts image paste, drag/drop, and an explicit file picker on desktop and
  mobile, with compact previews that can be removed before sending.
- One message can contain text, images, or both; sending targets the machine and pane captured when
  the attachments were selected, not a later selection.
- The selected machine validates image type, signature, count, individual size, and aggregate size
  before storing anything; filenames and request data never reach a shell command.
- Images are delivered using behavior supported by both Claude Code and Codex CLI, including remote
  machines, without relying on the browser and agent host sharing a clipboard.
- Temporary images use owner-only permissions, are outside the project repository, remain readable
  to the selected agent, and are removed on failure and by bounded expiry cleanup.
- Existing text-only sending, Enter/newline behavior, Quick Talk, history, and mobile Send continue
  to work.
- The browser header and icon use Ryan's atmux logo artwork from Midnight's Downloads folder.

## Gates

- [x] Implementation
- [x] Focused unit and browser tests
- [x] Linux tmux integration test with Claude and Codex-shaped panes
- [x] Midnight and Max native/live integration tests
- [x] Fable/Claude Max review
- [x] Independent security review

## Source notes

- Official OpenAI documentation: Codex interactive mode accepts pasted images and command-line
  `-i`/`--image` inputs.
- Official Anthropic documentation: Claude Code accepts Ctrl+V image paste and explicit image paths.

## Verification evidence

- Local strict Clippy and all 165 Rust tests pass, including validation, private-cache, cleanup,
  HTTP body-limit, federation, and real tmux delivery coverage.
- All 54 browser tests pass, including paste/file/drop selection, byte/count limits, captured-pane
  routing, byte-safe base64, compact previews, and Midnight logo wiring.
- Rust 1.88 locked dependency compatibility, Helm safe-disabled render checks, and diff checks pass.
- A native Claude Max session on Midnight received the logo as Claude's native `[Image #1]`,
  auto-submitted it after conversion, and returned the three visible logo words. A native Codex
  session on Max invoked its image-view tool and returned the same words. Temporary sessions were
  deleted afterward without touching existing user sessions.
- Owner-node text/image/special-key mutations are serialized per pane inside their blocking tmux
  work, so concurrent API callers cannot interleave paste conversion and Enter. Cache capacity is
  serialized separately, and Claude waits for every expected new image marker when a baseline is
  available.
- A production-UI Chrome race test delays an image request, attempts a second paste, and verifies
  the immutable delivered snapshot is removed exactly once without retaining or duplicating it.
- Fable/Claude Max and the independent security reviewer both returned `SAFE` on the final frozen
  snapshot after reviewing validation, storage, cleanup, remote routing, prompt serialization, and
  the Claude/Codex delivery behavior.
