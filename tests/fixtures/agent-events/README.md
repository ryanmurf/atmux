Hook fixtures use the documented native input shapes, with illustrative ids and
message bodies. No live agent was started or intercepted to create them.

- Claude Code 2.1.286: `claude --help` verifies inline `--settings`;
  https://code.claude.com/docs/en/hooks#notification-input
- Codex 0.159.3: `codex features list` reports `hooks stable true`;
  https://learn.chatgpt.com/docs/hooks#permissionrequest and
  https://learn.chatgpt.com/docs/hooks#stop define these fields.

The tests prove that prompt, message, transcript and tool-input bodies never
enter an event. Codex does not have Claude's Notification event. Its native
PermissionRequest/Stop hooks and the status fallback cover that distinction.
