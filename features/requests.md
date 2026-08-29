# Ryan's request index

This index keeps each explicit product request visible even when several requests share one
implementation record. Status is governed by the gates in [README.md](README.md).

## Runtime and machines

| Request | Feature record | Status |
| --- | --- | --- |
| Install and run atmux from `/home/ryan` on Max | `completed/mac-and-max-runtime.md` | Completed |
| Make the Mac runtime work and test natively on Midnight and Max | `completed/mac-and-max-runtime.md` | Completed |
| Restore Max as a persistent user service without a visible service tmux session | `max-persistent-service.md` | Runtime restored; review pending |
| Keep Max's local web UI in sync with its one live `ds-speed` agent | `max-persistent-service.md` | Runtime fixed; review pending |
| Discover other atmux machines on the same LAN | `completed/machine-federation-and-metrics.md` | Completed |
| Present one session list grouped by machine, with a machine picker when needed | `completed/machine-federation-and-metrics.md` | Completed |
| Select a machine and show CPU, memory, GPU, and temperatures | `completed/machine-federation-and-metrics.md` | Completed |
| Collect comprehensive GPU statistics from every box, including Macs | `comprehensive-gpu-telemetry.md` | Implementation active |
| Hide the `atmux-web` service tmux session from the user session list | `completed/agent-interaction-controls.md` | Completed |
| Keep Midnight on its Aqua tmux server so Claude can read the login Keychain | `completed/mac-and-max-runtime.md` | Completed and standing constraint |
| Use only the LaunchAgent kickstart for future Midnight web restarts; never rebuild its tmux server | `completed/mac-and-max-runtime.md` | Completed and standing constraint |

## Usage intelligence

| Request | Feature record | Status |
| --- | --- | --- |
| Merge every supported Claude Pulse feature into atmux as native Rust functionality | `claude-pulse-rust-merge.md` | Management/UI slice implemented and tested; overall merge active |
| Open Usage as a Pulse-style dashboard without requiring a numeric account ID | `pulse-dashboard-experience.md` | Implementation active |
| Collect and report Pulse data from the one atmux binary on each box | `single-binary-pulse-rollout.md` | Implementation active |

## Projects, agents, and profiles

| Request | Feature record | Status |
| --- | --- | --- |
| Replace the separate project finder with one typedown that filters as characters are entered | `completed/project-and-profile-launching.md` | Completed |
| Allow a validated folder to be entered manually even when it is not in discovery results | `completed/project-and-profile-launching.md` | Completed |
| Browse to an undiscovered launch folder and remember it per machine | `launch-folder-browser.md` | Implemented and live-tested; review pending |
| Treat folders containing Claude or Codex agent-instruction files as projects | `completed/project-and-profile-launching.md` | Completed |
| Descend through non-git grouping folders such as `nes-spring` and `nes-experimental` | `completed/project-and-profile-launching.md` | Completed |
| Update the proposed session name when a project is selected | `completed/project-and-profile-launching.md` | Completed |
| Store remembered session/profile settings in a safe project `.atmux.toml` | `completed/project-and-profile-launching.md` | Completed |
| Provide a top-level Claude/Codex agent-family choice before launch | `completed/project-and-profile-launching.md` | Completed |
| Discover and report matching Claude/Codex profiles independently on every computer | `completed/project-and-profile-launching.md` | Completed |
| Make every reported default Claude/Codex profile launchable under a reduced service `PATH` | `completed/profile-launch-reliability.md` | Completed |
| Make the Midnight profile picker show more than only `default` | `completed/project-and-profile-launching.md` | Completed |
| Launch profile-specific Claude sessions with the correct `CLAUDE_CONFIG_DIR`, including `~/.claude-max` | `completed/project-and-profile-launching.md` | Completed |
| Show the exact tmux/agent launch command beside the session name | `completed/project-and-profile-launching.md` | Completed |
| Let the web UI discover the same projects, instruction files, agents, and profiles as the host | `completed/project-and-profile-launching.md` | Completed |

## Agent interaction

| Request | Feature record | Status |
| --- | --- | --- |
| Push and hold Quick Talk, transcribe speech, and send it to the agent selected when capture began | `completed/quick-talk-lifecycle-reliability.md` | Completed |
| Route typing from the focused live pane into the agent composer | `completed/agent-interaction-controls.md` | Completed |
| Keep browser selection stable while live-pane lines refresh | `completed/agent-interaction-controls.md` | Completed |
| Make web/mobile message submission work through the authenticated gateway | `completed/web-submit-reliability.md` | Completed |
| Ensure raw-pane web sends submit after bracketed paste instead of leaving text in the agent input | `raw-pane-submit-reliability.md` | Implemented and live-tested; review pending |
| Send special terminal input such as Ctrl+B twice | `completed/agent-interaction-controls.md` | Completed |
| Add a compact-command shortcut | `completed/agent-interaction-controls.md` | Completed |
| Use Up/Down to browse previously sent comments | `completed/agent-interaction-controls.md` | Completed |
| Plain Enter sends; Ctrl/Command+Enter inserts a newline | `completed/composer-keyboard-behavior.md` | Completed |
| Switch the model of a running Claude or Codex pane from the web UI | `model-switching.md` | Implementation active |
| Paste, drop, or choose images in the web composer and deliver them to either Claude or Codex | `completed/image-attachments.md` | Completed |
| Use the atmux logo saved in Midnight's Downloads folder as the browser branding | `completed/image-attachments.md` | Completed |

## Conversation and web UI

| Request | Feature record | Status |
| --- | --- | --- |
| Use Claude and Codex session logs as the conversation display | `completed/claude-codex-transcripts.md` | Completed |
| Support old/unlabeled Claude CLI storage and profile directories such as `~/.claude-max` | `completed/claude-codex-transcripts.md` | Completed |
| Render agent output as safe native Markdown with links and highlighted, expandable code blocks | `completed/claude-codex-transcripts.md` | Completed |
| Show compact, expandable Claude and Codex tool calls and results | `completed/claude-codex-transcripts.md` | Completed |
| Make agent boxes and the overall web view denser and more streamlined | `completed/claude-codex-transcripts.md` | Completed |
| Put a nearby trash-can action beside each session in the left navigation | `completed/left-rail-session-controls.md` | Completed |
| Collapse and restore the left navigation | `completed/left-rail-session-controls.md` | Completed |
| Keep the agent pane full-width and stable when left navigation sizing changes | `completed/full-width-stable-agent-pane.md` | Completed |
| Make mobile browser Back return from an agent to the agent menu instead of login | `completed/mobile-browser-back-navigation.md` | Completed |
| Make Codex Conversation work when the active CLI has spawned subagents | `completed/codex-conversation-subagent-selection.md` | Completed |
| Show each agent's useful profile and project folder on the web UI; replace generic Default labels with the folder | `completed/web-agent-profile-visibility.md` | Completed |
| Compact mobile controls, prevent iOS composer zoom, and keep the whole composer visible above the keyboard | `completed/mobile-conversation-space.md` | Completed |
| Make populated Pulse Usage/Pace/Context/Alerts load through the authenticated mobile dashboard | `completed/pulse-mobile-dashboard-query.md` | Completed |
| Provide a secure iOS app for controlling atmux | `ios-controller-app.md` | Native API MVP built; auth/SSE/device gates active |

## Secure web access and delivery process

| Request | Feature record | Status |
| --- | --- | --- |
| Serve atmux through Kubernetes at `atmux.murphytek.com` with a valid trusted certificate | `secure-web-access.md` | Live for controlled retest; TLS and unauthenticated boundary verified |
| Authenticate through a separate Keycloak client using Google login | `secure-web-access.md` | Visible interactive login passed with signed Google claim |
| Allow only `ryanmurf@gmail.com` to log in | `secure-web-access.md` | Exact positive login verified; negative Google login pending |
| Require Fable/Claude Max and an independent security review before rollout | `secure-web-access.md` | Completed for the deployed loopback-auth remediation |
| Keep external routing absent until Ryan explicitly authorizes it | `secure-web-access.md` | Testing authorization received 2026-08-08 |
| Record every feature request under `features/` and update its status while work proceeds | `README.md` | Active workflow |
| Require implementation, focused unit tests, integration/live tests, and two reviews before completion | `README.md` | Active workflow |
| Move fully gated feature records into `features/completed/` | `README.md` | Active workflow |

## Operational requests retained as constraints

- The restored Midnight tmux server and its user sessions must not be killed or rebuilt.
- A service restart on Midnight must use
  `launchctl kickstart -k gui/$(id -u)/dev.herodevs.atmux-web`.
- Ryan later authorized the controlled authenticated Ingress test recorded above. Pulse work must
  leave that existing authenticated Ingress unchanged; creating, disabling, re-enabling, or adding
  a route still requires a separate explicit authorization.
- One-time namespace inspection, restart-command documentation, and clipboard requests were handled
  operationally and do not create product acceptance criteria.
# Active

- [Single-binary Pulse rollout](single-binary-pulse-rollout.md) — implementation active
