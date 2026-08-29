# Mobile browser Back navigation

Status: completed

## Request

When viewing an agent on the authenticated mobile website, the browser Back action should return to the atmux agent menu instead of navigating out to the login page.

## Acceptance

- Selecting an agent, machine, or Usage view creates an in-app history entry.
- A directly loaded or refreshed detail URL receives an agent-menu history entry behind it exactly once.
- Browser Back and the visible mobile Back control restore the agent menu without creating a history loop.
- Popstate restoration does not create another history entry.
- Existing deep links and desktop selection remain functional.

## Completion gate

- [x] Implemented
- [x] Unit/browser tested
- [x] Integration tested
- [x] Independently reviewed

## Verification

- Browser unit coverage checks menu/detail history replacement and the explicit Agents action.
- A real headless 390x844 browser proves `session -> Usage -> browser Back` returns to Agents.
- Two independent read-only reviews returned SAFE on 2026-08-09.
