# Web submit reliability

Status: completed

## Request

Ryan reported that submitting a message from the authenticated atmux website did not work. Treat the live web path as the priority, reproduce the browser action, find the failing layer, fix it, and verify real pane delivery without touching an existing agent.

## Implementation

- Confirmed the browser click and Enter paths send JSON with `Content-Type: application/json`.
- Traced Ryan's authenticated mobile requests through OAuth2 Proxy and the privileged gateway; every JSON mutation returned HTTP 415 before reaching the handler.
- Found a newline-terminated proxy bearer embedded by nginx `envsubst`. The malformed upstream `Authorization` header terminated the header block and removed later headers, including `Content-Type`.
- Added fail-closed gateway startup validation that rejects CR/LF, empty, or unsafe token values before nginx starts.
- Rotated the exposed proxy credential on Tron and in Kubernetes, restarted only the dedicated `atmux-web` pane, and rolled the existing authenticated Deployment without changing its Ingress route.

## Verification

- Browser unit suite: 63/63 passed.
- Helm lint/render security suite passed.
- Direct host JSON submit reached and executed in a disposable tmux pane.
- Kubernetes gateway JSON submit returned 200 and executed in the disposable pane after remediation.
- A gateway JSON POST to a nonexistent pane now returns route-level 404 instead of extractor-level 415.
- Anonymous public access still returns 302 to the Keycloak login flow, and the Ingress still targets only `atmux-oauth2-proxy`.
- Disposable pane and captured live assets were removed after testing.

## Completion gate

- [x] Unit tested
- [x] Integration tested
- [x] Independently reviewed

Independent verdict: SAFE. The review verified the live JSON route, hostile
Origin rejection, public/direct unauthenticated denial, exact-user Google OIDC
constraints, single-line credential parity, pinned healthy containers, and the
Helm render suite without changing live state.
