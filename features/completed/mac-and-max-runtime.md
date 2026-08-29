# Midnight and Max runtime

Status: completed

- [x] Native builds and tmux integration tests pass on Midnight and Max.
- [x] Midnight restarts use the Aqua LaunchAgent kickstart path, preserving Keychain access and the
  existing tmux server.
- [x] The restored Midnight tmux server is a standing protected resource: future service work must
  not kill/rebuild the server or its user sessions.
- [x] Exact `atmux-web:0.0` pane management leaves user sessions untouched.
- [x] Claude/Codex profiles and launch commands are reported on both hosts.
- [x] Runtime audits and independent reviews passed.
