# atmux public gateway

This chart keeps atmux on the host, where it can control the host tmux server,
and exposes it only through an OAuth2 Proxy → token-injecting gateway chain.
Both proxies share one Pod: the credential-bearing gateway listens only on
`127.0.0.1`, while OAuth2 Proxy is the only container reachable over the Pod
network. This prevents a cluster-node request from using the gateway to bypass
OAuth.

Before installing, provision the confidential `atmux-web` client separately in
Keycloak's existing `usage` realm. It must have exactly this redirect URI and
web origin:

- `https://atmux.murphytek.com/oauth2/callback`
- `https://atmux.murphytek.com`

The Keycloak administrator credential remains in `herodevs`; this chart never
mounts, copies, or references it. Use the platform's Keycloak administration
workflow from `herodevs.dev` to provision the client, then keep only the
resulting client secret in the application namespace.

Leave direct-access grants disabled. Bind this client’s `browser` flow to a
dedicated `atmux-google-only` flow that contains exactly one **required**
Identity Provider Redirector configured with `defaultProvider=google`; it must
not contain Cookie, Forms, Organization, or any local-login execution. This is
the first enforcement point. Add a client protocol mapper of type **User
Session Note** with user-session note `identity_provider`, token claim name
`identity_provider`, JSON type `String`, and **Add to ID token** enabled. Also
add it to the access token for consistent diagnostics and refresh behavior.
OAuth2 Proxy then requires that signed claim to equal `google`; a local realm
login or any alternate broker is denied even if a browser changes
`kc_idp_hint`. OAuth2 Proxy uses the client's OIDC discovery document and never
treats that browser-controlled hint as an authentication boundary.

Create these dedicated secrets in the `murphytek` namespace:

- `atmux-oauth`: `client-id=atmux-web`, a generated `client-secret`, and a
  32-byte base64url `cookie-secret`.
- `atmux-proxy-token`: a distinct high-entropy `token` value. Configure the
  same value in the host atmux `[web]` section as `proxy_token_file`. The
  Secret value must be one nonempty printable token with no CR/LF or trailing
  line terminator. The gateway rejects malformed values before nginx starts;
  do not create this Secret directly from a newline-terminated text file.
- `atmux-gateway-tls`: `ca.crt`, `tls.crt`, and `tls.key` for a dedicated
  gateway client identity signed by the private atmux CA. The host atmux
  listener must use its own CA-signed certificate and `[node.tls]` config.

The safe default is `ingress.enabled=false`, renders no Ingress, and may use an
empty email file (deny all). Public opt-in requires `oauth.allowedEmails` to
contain only the exact Google identities reviewed in the chart. The chart
rejects an empty, unapproved, wildcard, or malformed public allowlist, and
deliberately does not set OAuth2 Proxy’s broad `email-domain` option.

Install or update the internal backing services only after security review;
this command deliberately keeps public routing absent:

```bash
helm upgrade --install atmux-web ./deploy/helm/atmux-web \
  --namespace murphytek --create-namespace --wait --timeout 5m \
  --set ingress.enabled=false \
  --set-json 'oauth.allowedEmails=["ryanmurf@gmail.com","ryanm@herodevs.com"]'
```

Enabling the hostname is a separate reviewed change. Do not add
`--set ingress.enabled=true` until the Keycloak negative-test matrix (including
a non-Google `identity_provider` claim), exact-user login, image digests,
NetworkPolicies, TLS chain, and independent security reviews all pass.

The client is intentionally not provisioned by a Helm hook: the release has no
Keycloak admin credential, and no cross-namespace Secret copy is permitted.
The `usage` realm is the established Google-login realm used by the public
usage dashboard.

The OAuth2 Proxy and token-injecting gateway are intentionally trusted
sidecars: an authenticated user can perform shell-equivalent atmux actions,
and a compromise of either container can exercise that same authority. Their
images are digest-pinned, service-account tokens are disabled, the privileged
sidecar is loopback-only, and the chart limits the remaining path with
NetworkPolicy, least-privilege containers, and a distinct proxy credential.

Run the chart regression suite with:

```bash
bash deploy/helm/atmux-web/tests/render.sh
```
