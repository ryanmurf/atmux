# AtmuxController for iOS

AtmuxController is a native SwiftUI client for the existing atmux JSON API. It does not embed the web UI.

## MVP capabilities

- Named HTTPS connection profiles.
- Bearer tokens stored with `kSecAttrAccessibleWhenUnlockedThisDeviceOnly` in the iOS Keychain.
- Optional PKCS#12 client identities, also stored in Keychain and presented only for a TLS client-certificate challenge.
- Sessions grouped by their owning machine, including agent, profile, and status, with native CPU, memory, GPU/VRAM, and temperature detail.
- Conversation transcript and bounded raw-pane views.
- Read the current pane model, switch among owner-reported switchable choices, send text, interrupt an agent, and kill a session after confirmation.
- Pulse account discovery and provider quota cards with reset and machine provenance.
- Bounded response/error handling plus loading and offline states.

Connection metadata (name, URL, selected profile) is the only connection data stored in `UserDefaults`. Tokens, PKCS#12 bytes, and PKCS#12 passwords are never placed in preferences or logs. Server certificate validation uses the platform trust store; the app has no trust-all or self-signed-certificate bypass.

## Build

1. Open `AtmuxController.xcodeproj` in Xcode 16 or newer.
2. Select the `AtmuxController` target and choose your Apple Development team.
3. Build for an iOS 17+ device or simulator.
4. Add an HTTPS atmux endpoint and its node/proxy bearer token. If the endpoint requires direct mTLS, import its `.p12`/`.pfx` client identity.

The repository also contains a small Swift package manifest for testing the transport/model layer outside the app target. On a macOS development machine, run either:

```sh
xcodebuild test -project AtmuxController.xcodeproj -scheme AtmuxController -destination 'platform=iOS Simulator,name=iPhone 16'
swift test
```

## Authentication boundary

This MVP targets a direct atmux API endpoint protected by the existing bearer-token policy, optionally combined with mTLS. It deliberately does not attempt to automate the browser-oriented Keycloak/oauth2-proxy flow at `atmux.murphytek.com`.

Public OAuth support needs a separately registered native Keycloak client using Authorization Code + PKCE, an iOS callback URI/universal link, and a defined token-exchange contract at the atmux gateway. Do not embed a client secret in the app. Until that registration and gateway contract exist, use a trusted direct HTTPS API endpoint and keep public ingress policy under the existing operator-controlled process.
