# Codex conversation selection with subagents

Status: completed

## Request

The web Conversation view must show the active Codex CLI conversation as reliably as Claude,
including when the Codex process has spawned subagents.

## Acceptance

- A rollout file identifies a Codex session only when its own filename is bound to the UUID in
  its CLI/user `session_meta` record.
- A child rollout that embeds its parent's metadata cannot make the parent selection ambiguous.
- Ambiguous or unverifiable session mapping still fails closed instead of exposing another log.
- The authenticated public web route returns the same active Codex conversation as localhost.

## Completion gate

- [x] Implemented
- [x] Unit tested
- [x] Integration tested
- [x] Independently reviewed

## Verification

- Transcript unit coverage includes parent metadata embedded in a child rollout.
- Local and authenticated live routes select the exact open Codex UUID/CWD and return the bounded
  active transcript without exposing its content during verification.
- Two independent read-only reviews returned SAFE on 2026-08-09.
