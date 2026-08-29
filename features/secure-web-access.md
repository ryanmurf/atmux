# Secure web access through Kubernetes and Keycloak

Status: reviewed remediation deployed; controlled live access

## Implemented

- Dedicated confidential Keycloak client and Google-only identity-provider enforcement.
- Exact allowlist for Ryan's explicitly approved Google identities.
- OAuth2 Proxy and loopback-only credential-injecting gateway in one Pod.
- mTLS/bearer upstream authentication, restrictive NetworkPolicy, pinned images, resource limits,
  and safe-disabled Helm defaults.
- Ingress template is conditional and disabled by default.
- API and MCP access no longer treats loopback as an ambient authentication boundary. The secure
  default requires a node or proxy bearer; unauthenticated loopback is an explicit development-only
  opt-in.

## Live evidence

- Helm revision 9 has `ingress.enabled=true`, `oauth.sessionVersion=v2`, and exactly one allowlisted
  identity: `ryanmurf@gmail.com`.
- The live `murphytek/atmux` Ingress routes only `atmux.murphytek.com` to OAuth2 Proxy port 4180.
- The single ready Pod contains only the digest-pinned OAuth2 Proxy and loopback gateway containers;
  the ClusterIP Service exposes only OAuth2 Proxy port 4180.
- Unauthenticated OAuth returns 302; the gateway's loopback health request reaches Tron through
  mTLS/bearer authentication; Pod-IP port 8080 is closed; an unrelated Pod is denied by policy.
- OIDC discovery reaches Keycloak. Omitted or explicit `google` hints route to the Google broker;
  empty and unknown provider hints return HTTP 400.
- A fresh private port-forward check on 2026-08-08 confirmed that an unauthenticated root request
  redirects to the dedicated `atmux-web` OIDC client, `/oauth2/auth` returns HTTP 401 without a
  session, and the loopback gateway remains unreachable through the Pod IP. The temporary
  localhost-only port-forward was closed after the check.
- The live Keycloak database confirms direct-access grants are disabled, the client browser binding
  points to `atmux-google-only`, that flow contains one required Identity Provider Redirector for
  `google`, and the signed `identity_provider` mapper is present in both ID and access tokens.
- The public TLS chain validates for `atmux.murphytek.com`; an unauthenticated root or API request
  redirects to the dedicated Keycloak client, while `/oauth2/auth` returns HTTP 401 without a
  session.
- The first browser request did traverse OAuth and produced an `AuthSuccess` for
  `ryanmurf@gmail.com` with signed `groups:[google]`, but Keycloak silently reused an existing SSO
  session. Ryan correctly rejected that invisible authentication experience as appearing unauthenticated,
  and the Ingress was removed immediately. The next test uses OIDC
  `prompt=login select_account` to force Keycloak reauthentication and a visible Google account
  choice, and invalidates the revision-6 OAuth2 Proxy cookie first.
- The fixed public redirect contains `prompt=login+select_account`, creates only the new
  `__Host-atmux-v2` CSRF cookie, returns HTTP 401 from `/oauth2/auth` without a session, and passes
  trusted TLS validation.
- Independent Sol and Claude Max Opus 5 adversarial reviews returned `SAFE` after public rendering
  was pinned to exactly `ryanmurf@gmail.com` and the revoked v1 cookie name was made unrenderable.
- Ryan completed the visible interactive login on 2026-08-08. The live OAuth callback recorded
  `AuthSuccess` for exactly `ryanmurf@gmail.com` with the signed `groups:[google]` claim before any
  application asset or API response was served.
- The 2026-08-08 Fable/independent audit found that the host listener still allowed anonymous
  loopback API and MCP access. A local status-only reproduction returned HTTP 200 for
  `/api/v1/sessions`. The replacement policy is fail-closed by default and has middleware plus real
  socket regressions covering sessions, panes, transcripts, events, mutations, and MCP.
- Fable on the `claude-hd` account and the independent reviewer both returned `SAFE` for the frozen
  replacement. Fable ran 370 tests across 12 suites; the independent reviewer ran the Rust 1.88
  309-test library gate plus the focused loopback/socket regressions.
- The reviewed release replaced only Tron's dedicated `atmux-web` pane. Post-deploy anonymous
  loopback sessions, transcript, SSE, mutation, and MCP probes all return HTTP 401; the configured
  proxy bearer and the Pod's loopback gateway/mTLS path return HTTP 200. Public anonymous access
  still returns HTTP 302 to Keycloak and the Deployment remains 1/1 Ready.

## Remaining gates

- [x] Interactive initial Google login as Ryan.
- [x] Visible Google account selection or reauthentication when starting a new atmux session.
- [ ] Post-refresh session check.
- [ ] Negative login as any other Google user.
- [x] Fable and independent review of the loopback-auth fix.
- [x] Deploy the reviewed host binary and verify anonymous loopback API/MCP requests return 401
  while the authenticated gateway still reaches the application.
- [x] Explicit instruction from Ryan to re-create/enable Ingress for controlled testing.

The route is live only for Ryan's controlled interactive retest. Disable it immediately if the
visible account-selection or any authentication boundary check fails.
