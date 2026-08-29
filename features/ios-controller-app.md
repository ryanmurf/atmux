# iOS controller app

Status: implementation active; native API MVP built and tested

## Request

Provide an iOS app for securely controlling atmux from an iPhone.

## Acceptance

- The app is a native SwiftUI client that uses typed REST/SSE calls to the existing authenticated
  `atmux web` service; it is not a `WKWebView` wrapper and never starts a second backend.
- Direct API authentication uses a bearer credential and optional client identity stored only in
  the iOS Keychain. Public Keycloak sign-in will use a separately registered native Authorization
  Code + PKCE client; the app never embeds a client secret or browser cookie.
- The app can browse machines and agents, open Conversation/Raw views, send text and images,
  interrupt or kill with confirmation, switch supported models, use Quick Talk, and open Usage.
- Mobile navigation stays inside the app and returning to Agents never signs the user out.
- Network failures, offline machines, expired authentication, and destructive actions have explicit
  states and safe recovery.
- The app is tested on a native iOS simulator/device build and independently security reviewed.

## Completion gate

- [x] UX/API contract reviewed
- [x] Native REST control MVP implemented
- [x] Core transport/model unit tested on macOS
- [x] Native iOS simulator SDK build succeeds
- [ ] Integration tested against authenticated atmux web
- [ ] Public Keycloak native PKCE client registered and integrated
- [ ] Image sending, Quick Talk, and bounded SSE refresh implemented
- [ ] Native iOS simulator/device interaction tested
- [ ] Two independent security reviews complete

## Architecture constraint

The iOS app is an API client only. Collection, reporting, tmux ownership, authentication
enforcement, and all agent mutations remain in each host's single Rust `atmux` binary. The current
MVP uses Keychain-backed bearer/client-certificate material and `URLSession`; no browser DOM is
embedded. Public OAuth and SSE are explicit follow-up gates rather than simulated with a web view.

## Current evidence

- `swift test`: 16/16 transport, route, decoding, model-switch, and secret-storage tests pass on
  Midnight.
- The full SwiftUI target builds successfully against the iOS Simulator 26.5 SDK with code signing
  disabled. Midnight has no installed simulator runtime, so interactive simulator testing remains
  pending.
- The app includes Conversation/Raw, send, interrupt, kill confirmation, model switching, Pulse
  quota cards, and CPU/RAM/GPU/sensor machine detail.
