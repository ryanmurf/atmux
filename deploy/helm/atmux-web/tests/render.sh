#!/usr/bin/env bash
set -euo pipefail

chart="${1:-$(cd "$(dirname "$0")/.." && pwd)}"
helm_bin="${HELM_BIN:-helm}"
work="$(mktemp -d)"
trap 'rm -rf "$work"' EXIT

"$helm_bin" lint "$chart"
"$helm_bin" template atmux-web "$chart" --namespace murphytek >"$work/default.yaml"

if grep -Eq '^kind: Ingress$' "$work/default.yaml"; then
  echo "safe default unexpectedly rendered an Ingress" >&2
  exit 1
fi

"$helm_bin" template atmux-web "$chart" --namespace murphytek \
  --set ingress.enabled=true \
  --set-json 'oauth.allowedEmails=["ryanmurf@gmail.com","ryanm@herodevs.com"]' >"$work/enabled.yaml"

test "$(grep -Ec '^kind: Ingress$' "$work/enabled.yaml")" -eq 1
grep -Fq -- '--trusted-proxy-ip=10.1.112.0/24' "$work/enabled.yaml"
grep -Fq -- '--prompt=login select_account' "$work/enabled.yaml"
grep -Fq -- '--cookie-name=__Host-atmux-v2' "$work/enabled.yaml"
grep -Fq -- '--oidc-groups-claim=identity_provider' "$work/enabled.yaml"
grep -Fq -- '--allowed-group=google' "$work/enabled.yaml"
grep -Fq 'ryanmurf@gmail.com' "$work/enabled.yaml"
grep -Fq 'ryanm@herodevs.com' "$work/enabled.yaml"
grep -Fq 'checksum/allowed-emails:' "$work/enabled.yaml"
grep -Fq 'oauth2-proxy:v7.15.3@sha256:' "$work/enabled.yaml"
grep -Fq 'proxy_ssl_verify on;' "$work/enabled.yaml"
grep -Fq 'listen 127.0.0.1:8080;' "$work/enabled.yaml"
grep -Fq 'client_max_body_size 18m;' "$work/enabled.yaml"
grep -Fq 'proxy_request_buffering off;' "$work/enabled.yaml"
grep -Fq 'tr -d '\''\r\n'\''' "$work/enabled.yaml"
grep -Fq 'test "$cleaned" = "$ATMUX_PROXY_TOKEN"' "$work/enabled.yaml"
grep -Fq 'invalid ATMUX_PROXY_TOKEN' "$work/enabled.yaml"
grep -Fq "exec /docker-entrypoint.sh nginx -g 'daemon off;'" "$work/enabled.yaml"
grep -Fq -- '--upstream=http://127.0.0.1:8080' "$work/enabled.yaml"
grep -Fq 'http://127.0.0.1:8080/healthz' "$work/enabled.yaml"
grep -Fq 'nginx.ingress.kubernetes.io/limit-connections: "10"' "$work/enabled.yaml"
grep -Fq 'nginx.ingress.kubernetes.io/limit-rps: "10"' "$work/enabled.yaml"
grep -Fq 'nginx.ingress.kubernetes.io/limit-burst-multiplier: "3"' "$work/enabled.yaml"
grep -Fq 'nginx.ingress.kubernetes.io/proxy-body-size: "18m"' "$work/enabled.yaml"
grep -Fq 'nginx.ingress.kubernetes.io/proxy-buffering: "off"' "$work/enabled.yaml"
test "$(grep -Ec '^          resources:$' "$work/enabled.yaml")" -eq 2
grep -Fq 'memory: 256Mi' "$work/enabled.yaml"
grep -Fq 'memory: 128Mi' "$work/enabled.yaml"
test "$(grep -Ec '^kind: Deployment$' "$work/enabled.yaml")" -eq 1
test "$(grep -Ec '^kind: NetworkPolicy$' "$work/enabled.yaml")" -eq 1
if grep -Fq 'selector: {app: atmux-gateway}' "$work/enabled.yaml"; then
  echo "credential-bearing gateway unexpectedly has a cluster-network identity" >&2
  exit 1
fi

# Either reviewed identity may remain independently authorized during a
# deliberate allowlist rollout; public access still rejects every other user.
"$helm_bin" template atmux-web "$chart" --namespace murphytek \
  --set ingress.enabled=true \
  --set-json 'oauth.allowedEmails=["ryanmurf@gmail.com"]' >/dev/null
"$helm_bin" template atmux-web "$chart" --namespace murphytek \
  --set ingress.enabled=true \
  --set-json 'oauth.allowedEmails=["ryanm@herodevs.com"]' >/dev/null

must_fail() {
  if "$helm_bin" template atmux-web "$chart" --namespace murphytek "$@" >"$work/rejected.yaml" 2>&1; then
    echo "unsafe chart values unexpectedly rendered: $*" >&2
    exit 1
  fi
}

must_fail --set ingress.enabled=true
must_fail --set ingress.enabled=true --set-json 'oauth.allowedEmails=["attacker@example.com"]'
must_fail --set ingress.enabled=true --set-json 'oauth.allowedEmails=["ryanmurf@gmail.com","attacker@example.com"]'
must_fail --set ingress.enabled=true --set-json 'oauth.allowedEmails=["ryanmurf@gmail.com","ryanm@herodevs.com","attacker@example.com"]'
must_fail --set ingress.enabled=true --set-json 'oauth.allowedEmails=["*"]'
must_fail --set ingress.enabled=true --set-json 'oauth.allowedEmails=["not-an-email"]'
must_fail --set-json 'oauth.keycloakEgressCidrs=["0.0.0.0/0"]'
must_fail --set-json 'oauth.keycloakEgressCidrs=["66.7.119.0/24"]'
must_fail --set oauth.sessionVersion=latest
must_fail --set oauth.sessionVersion=v1
must_fail --set-json 'oauth.trustedProxyCidrs=["0.0.0.0/0"]'

echo "Helm render security tests passed"
