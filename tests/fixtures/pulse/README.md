# Pulse collector fixtures

These sanitized fixtures were independently reduced from response shapes and
tests in Claude Pulse (Apache-2.0, copyright Ryan Murphy), frozen at commit
`00ef42a9816e680bc35707bdc93426a97bc4902d`. They contain invented timestamps,
percentages, plan names, and identifiers. No credential, account identity, raw
production response, or transcript content is included.

Relevant behavioral provenance:

- `src/usage.ts`: Anthropic OAuth usage parsing and Codex live/rollout parsing.
- `src/auth.ts`: Claude Code credential shape and cooperative refresh behavior.
