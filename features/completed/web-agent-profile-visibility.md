# Web agent profile visibility

Status: completed

## Request

Show the Claude or Codex profile used by each running agent on the web UI.

## Acceptance

- Sessions launched by atmux persist their configured profile name as bounded tmux metadata.
- Existing Claude/Codex sessions derive a safe conventional profile label from their launch
  command when explicit metadata is unavailable.
- Session rows, filtering, accessibility labels, and the selected-agent header show meaningful
  configured profiles alongside the project folder.
- The generic inferred `Default` label is suppressed in favor of the last two project-folder
  components, which are placed first so they remain visible on narrow mobile screens. The complete
  path remains available as the element title.
- Federated session summaries carry the profile without exposing arbitrary environment values.

## Completion gate

- [x] Implemented
- [x] Unit/browser tested
- [x] Integration tested
- [x] Independently reviewed

## Verification

- Unit/browser tests cover explicit metadata, safe legacy inference, rendering, filtering, and
  federation compatibility.
- Focused browser units prove a default Claude session renders `104-blue-mountain / solar` rather
  than `Default`, while `claude-max` and other meaningful labels remain visible.
- Two independent read-only reviews returned SAFE on 2026-08-09.
