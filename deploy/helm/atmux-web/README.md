# atmux public gateway and coordinator

The safe default keeps atmux on the host, where it can control the host tmux
server, and exposes it only through an OAuth2 Proxy → token-injecting gateway
chain. Setting `server.enabled=true` instead runs a coordinator-only atmux
server in the existing Pod. The coordinator federates the host nodes over
native mutual TLS; tmux servers, PTYs, agent processes, projects, and provider
credentials never move into Kubernetes.

Both proxies share one Pod: the credential-bearing gateway listens only on
`127.0.0.1`, while OAuth2 Proxy is the only container reachable over the Pod
network. This prevents a cluster-node request from using the gateway to bypass
OAuth. In coordinator mode, atmux also listens only on `127.0.0.1`; nginx
injects its dedicated web token over loopback. Atmux itself presents the
private-CA client certificate and each machine's node bearer on direct HTTPS
connections to the reviewed private IPs.

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

## In-cluster coordinator mode

Build the repository's multi-stage image with a new immutable tag, push it to
the local MicroK8s registry, and record the registry digest. Never reuse a tag
for a different build:

```bash
revision="$(git rev-parse --verify HEAD^{commit})"
image="localhost:32000/atmux:${revision:0:12}"
bash scripts/build-container.sh localhost:32000/atmux
docker push "$image"
digest="$(docker inspect --format '{{ index .RepoDigests 0 }}' "$image")"
test -n "$digest"
```

The helper requires its Dockerfile, ignore rules, lockfiles, and helper itself
to be committed and unchanged. It streams `git archive HEAD` directly to
Docker, so the OCI revision label, tag, and source bytes identify the exact
same full commit; dirty tracked files and untracked files never enter the
build context. Commit the reviewed implementation before producing the live
image. The archive is streamed and does not use `/tmp`.
The Docker build also uses `--no-cache`, matching the deployment build pattern
for a new immutable application tag.

Coordinator mode reuses `atmux-gateway-tls` as its outbound client identity.
The certificate must have clientAuth usage and chain to the CA trusted by
Tron, Max, and Midnight. Provision a separate Secret containing the existing
node bearer for each host. The keys, not their values, are chart configuration:

```bash
kubectl -n murphytek create secret generic atmux-node-tokens \
  --from-file=tron=/secure/path/tron.node-token \
  --from-file=max=/secure/path/max.node-token \
  --from-file=midnight=/secure/path/midnight.node-token \
  --dry-run=client -o yaml | kubectl apply -f -
```

Do not place token values in a Helm values file. The host services must remain
bound with mTLS, require their node token, and allow the exact IP authority the
coordinator sends (for example `192.168.0.109:7345`). Their current tmux
servers remain the session owners and must not be stopped during this rollout.

Create a reviewed override file outside Git. This is an illustrative shape;
the addresses must be rechecked against the certificate IP SANs and current
LAN assignments before every cutover:

```yaml
server:
  enabled: true
  image: localhost:32000/atmux@sha256:<64-lowercase-hex-characters>
  proxyTokenSecret: atmux-proxy-token
  tlsSecret: atmux-gateway-tls
  federationTokenSecret: atmux-node-tokens
  secretRevision: v1
  machines:
    - {id: tron, label: Tron, address: 192.168.0.109, port: 7345, tokenKey: tron}
    - {id: max, label: Max, address: 192.168.0.124, port: 7345, tokenKey: max}
    - {id: midnight, label: Midnight, address: 192.168.0.127, port: 7345, tokenKey: midnight}
  pulse:
    serve: true
    accounts:
      - id: 4
        identity: ryanmurf@gmail.com
        displayName: Ryan
        profiles:
          - {name: claude-hd, vendor: anthropic-oauth}
          - {name: claude-max, vendor: anthropic-oauth}
          - {name: codex, vendor: openai-codex}
          - {name: grok, vendor: xai-grok}
          - {name: gemini, vendor: gemini}
          - {name: antigravity, vendor: antigravity}
  persistence:
    storageClass: microk8s-hostpath
    size: 2Gi
```

The generated coordinator configuration sets `[node].coordinator_only = true`.
Atmux therefore never opens tmux, samples or presents the Pod as a machine,
publishes Pod-local sessions/metrics/launch inputs, or accepts Pod-local owner
mutations. Validation also requires no **agent-launch** profiles or project
roots and disables discovery, local collection, Pulse receive/push reporting,
maintenance, auto-compaction, and local agent resource scopes. It is not an
agent runtime. The PVC stores
Pulse SQLite data and coordinator state; it does not and cannot persist a PTY
or process through Pod replacement. Its secret-free Pulse profile names and
vendors match account 4 on the owner nodes so pull federation can attach those
rows; owner-local credential directories, polling, and refresh settings are
not copied into Kubernetes.

Save the current computed values, then render and inspect the exact release
before changing live state:

```bash
helm get values atmux-web -n murphytek -a > /secure/path/atmux-before.values.yaml
helm template atmux-web ./deploy/helm/atmux-web \
  --namespace murphytek \
  -f /secure/path/atmux-before.values.yaml \
  -f /secure/path/atmux-home.values.yaml > /mnt/data/herodevs-agents/atmux-rendered.yaml
```

For the live release, use the same reviewed override; never run a bare
`helm upgrade`:

```bash
helm upgrade atmux-web ./deploy/helm/atmux-web \
  --namespace murphytek --reset-then-reuse-values \
  -f /secure/path/atmux-home.values.yaml \
  --rollback-on-failure --cleanup-on-fail --wait --timeout 5m
```

`--reset-then-reuse-values` is required when moving from the gateway-only
0.3.x chart: it loads this chart's new safe defaults first, reapplies the live
release values, and then applies the reviewed coordinator override. Plain
`--reuse-values` omits newly introduced defaults; `--reset-values` alone drops
the release's reviewed OAuth and gateway values. `--rollback-on-failure`
restores the previous successful release after a failed cutover; explicit
`--wait` watches the Recreate rollout up to the bounded timeout, while
`--cleanup-on-fail` removes newly created non-retained resources from that
failed revision. The Pulse PVC is deliberately exempt:
`helm.sh/resource-policy: keep` retains it through failure, rollback, and
uninstall until an operator explicitly reuses or retires it.

Verify the running atmux container's `imageID` digest, authenticated health,
all three machine states, one read, and one disposable owner-routed mutation.
Also prove anonymous public access still redirects to Keycloak and an
unapproved/non-Google identity still fails. Host services stay running so a
rollback returns immediately to the gateway-only path:

```bash
helm rollback atmux-web <last-gateway-only-revision> \
  --namespace murphytek --wait --timeout 5m
```

The chart marks its generated PVC with Helm's `resource-policy: keep`, so a
gateway-only rollback or uninstall does not delete Pulse data. A retained
claim is intentionally orphaned storage: reuse it through
`server.persistence.existingClaim` on the next coordinator rollout, and delete
it only as a separate, explicit data-retirement operation.

Mounted Secret contents are resolved only at process startup. Increment
`server.secretRevision` and perform a reviewed Helm upgrade whenever a node,
proxy, or TLS credential rotates.

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
