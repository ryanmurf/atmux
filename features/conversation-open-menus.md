# Conversation: Claude's open menus

Status: implemented and tested 2026-10-13; not yet deployed.

## Why

When Claude asks a question, it draws a select list where the composer usually is. That covers
`AskUserQuestion`, permission prompts, plan approval, and the folder-trust dialog. Conversation
never showed these menus, so the only sign of a question was the "Needs input" badge.

Typing into Conversation while a menu was open made it worse. A probe on Claude Code 2.1.296
reproduced it: atmux pasted "please use the third option, fish" into a question with Cat
highlighted, then pressed Enter. The menu dropped the pasted text, and Enter answered "Cat".

The transcript can't help. Claude writes the `AskUserQuestion` and `ExitPlanMode` calls to its
log only after they're answered, so while the question is open the pane is the only place it
appears.

## What changed

- **Detection.** The owner reads an open menu off the pane it already captures each poll
  (`src/choice.rs`) and adds it to the session summary as `choice`. That includes the dialog
  text above the options, each option's number, label, and description or checkbox, the
  highlighted option, and the key hints. The parser needs a highlighted option (`❯`) with
  numbered siblings, and nothing below the options but Claude's key hints. Plan steps above the
  plan menu are numbered too, but they aren't treated as options. The `Next` stop in a
  multi-select and the `Chat about this` row below a separator are. The composer never counts:
  its status footer isn't a hint row, so a typed `1.`/`2.` draft stays a draft. A pane holding a
  menu now reads as waiting.
- **The card.** Conversation shows the menu above the composer: the question or command, then
  one button per option. Options keep their numbers, descriptions, and checkbox state, and the
  highlighted one is marked. Clicking a checkbox option ticks or unticks it. "Type something."
  and "Tell Claude what to change" open a one-line answer field. Cancel (Esc) appears when
  Claude offers Esc to cancel.
- **Answering.** `POST /api/v1/panes/{id}/choice` carries the menu's id, the option index, any
  typed text, and the pane generation. A coordinator forwards it to the owning machine like the
  other pane controls. The owner reads the menu again and refuses with 409 if it has closed or
  changed. The id hashes the question and the options, so a later permission prompt with the same
  labels for a different command has a different id. The owner then presses Up or Down one key
  at a time, re-reading the pane after each, and presses Enter only once the highlight is on the
  chosen option. If the highlight never gets there, nothing is picked. A typed answer is pasted
  into the field first. In a multi-select, typing already ticks the box, so Enter is skipped.
  Cancel sends Esc only while that same menu is open, so a late click can't interrupt an agent
  that has gone back to work.
- **The guard.** While a menu is open, the owner refuses composer text, image messages, and MCP
  `agent_send` with 409, and nothing reaches the pane. In the browser, pressing Enter in the
  composer keeps the draft and explains why it wasn't sent. When the menu has a typed-answer
  option, the draft moves into that field, so one click sends it as the answer. The composer is
  cleared once that answer lands.

Only Claude panes are read. Codex approval menus are a possible follow-up.

## Gate

fmt and zero-warning clippy (pedantic); `cargo +1.88 check --locked`. New tests:

- `choice::tests` parses real `tmux capture-pane` tails from Claude Code 2.1.296: a
  single-select question, a multi-select with the highlight on `Next`, a permission prompt whose
  option wraps, plan approval with its numbered plan, the review step (which has no hint row), a
  typed answer, and the trust dialog. They also check that the composer is never read as a menu,
  even with a numbered draft. The id ignores the highlight and checkbox marks but changes with
  the command.
- A fake pane that redraws the real capture checks answering. The tests check the keys pressed
  in each direction and typing before Enter. They check that no Enter goes to a menu that
  changed mid-way or ignored the keys, and that bad text is refused before any key is pressed.
  Cancel presses Esc only while the menu is open.
- `tests/federation.rs` checks that a coordinator forwards the answer body unchanged and refuses
  another pane generation or multi-line text before forwarding. An owner's 409 stays a conflict
  without leaking its detail.
- JS tests cover the card view, the request binding, and the one-line check.

The full Rust suite passed (970 tests) on a disposable `ATMUX_TMUX_SOCKET_NAME` at
`--test-threads=8`, and 212 JS tests passed.

Live, against a disposable Claude (Haiku 5.5, Claude Code 2.1.296) on its own tmux socket with a
dev `atmux web`:

- Every endpoint case landed as intended: a single-select pick, a typed answer, ticking and
  typing in a multi-select, `Next`, a second question, Submit, "No" on a permission prompt (the
  file wasn't created), plan feedback (Claude revised the plan), and Cancel. A repeated Cancel
  after the menu closed was refused.
- A composer message during a question got 409, and the menu stayed where it was.
- In headless Chrome at 1280 and 390 px, the card rendered and Enter in the composer was held.
  The draft moved into "Type something." and was sent as the answer, and the composer was cleared.
  Clicking an option answered it.

The existing browser suites pass except `mobile browser Back stays inside atmux…`, which failed
1 of 3 runs here and 1 of 3 on unchanged HEAD ("Inspected target navigated or closed").
