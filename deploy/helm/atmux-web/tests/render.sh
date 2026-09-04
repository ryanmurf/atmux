#!/usr/bin/env bash
# Manifest assertions intentionally match literal shell variables.
# shellcheck disable=SC2016
set -euo pipefail

chart="${1:-$(cd "$(dirname "$0")/.." && pwd)}"
helm_bin="${HELM_BIN:-helm}"
work="$(mktemp -d)"
trap 'rm -rf "$work"' EXIT

digest="sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
enabled_values=(
  --set ingress.enabled=true
  --set-json 'oauth.allowedEmails=["ryanmurf@gmail.com","ryanm@herodevs.com"]'
  --set server.enabled=true
  --set "server.image=localhost:32000/atmux@${digest}"
  --set-json 'server.machines=[{"id":"tron","label":"Tron","address":"192.168.0.109","port":7345,"tokenKey":"tron"},{"id":"max","label":"Max","address":"192.168.0.124","port":7345,"tokenKey":"max"},{"id":"midnight","label":"Midnight","address":"192.168.0.127","port":7345,"tokenKey":"midnight"}]'
)

assert_unique_top_level_keys() {
  awk '
    /^---[[:space:]]*$/ { delete seen; next }
    /^[A-Za-z0-9_.-]+:[[:space:]]*/ {
      key = $0
      sub(/:.*/, "", key)
      if (seen[key]++) {
        printf "duplicate top-level YAML key %s in %s\n", key, FILENAME > "/dev/stderr"
        exit 1
      }
    }
  ' "$1"
}

"$helm_bin" lint "$chart"
grep -Eq '^version: 0\.4\.9$' "$chart/Chart.yaml"
"$helm_bin" template atmux-web "$chart" --namespace murphytek >"$work/default.yaml"
assert_unique_top_level_keys "$work/default.yaml"

if grep -Eq '^kind: Ingress$' "$work/default.yaml"; then
  echo "safe default unexpectedly rendered an Ingress" >&2
  exit 1
fi
if grep -Eq '^kind: PersistentVolumeClaim$' "$work/default.yaml"; then
  echo "gateway-only default unexpectedly rendered coordinator storage" >&2
  exit 1
fi
if grep -Fq 'name: atmux-server-config' "$work/default.yaml"; then
  echo "gateway-only default unexpectedly rendered a coordinator" >&2
  exit 1
fi
grep -Fq 'proxy_pass https://192.168.0.109:7345;' "$work/default.yaml"
grep -Fq 'proxy_ssl_name "tron";' "$work/default.yaml"
test "$(grep -Ec '^        - name: (gateway|oauth2-proxy)$' "$work/default.yaml")" -eq 2
test "$(grep -Ec '^kind: Deployment$' "$work/default.yaml")" -eq 1
test "$(grep -Ec '^kind: NetworkPolicy$' "$work/default.yaml")" -eq 1

"$helm_bin" template atmux-web "$chart" --namespace murphytek \
  "${enabled_values[@]}" >"$work/enabled.yaml"
assert_unique_top_level_keys "$work/enabled.yaml"

# The rollout checksum must follow the rendered TOML itself, including a
# template-only change made with identical release values.
mutated_chart="$work/chart-template-change"
cp -R "$chart" "$mutated_chart"
sed -i 's/coordinator_only = true/coordinator_only = false/' \
  "$mutated_chart/templates/_helpers.tpl"
"$helm_bin" template atmux-web "$mutated_chart" --namespace murphytek \
  "${enabled_values[@]}" >"$work/template-change.yaml"
baseline_checksum="$(sed -n 's/.*checksum\/server-config: "\([a-f0-9]\{64\}\)".*/\1/p' "$work/enabled.yaml")"
changed_checksum="$(sed -n 's/.*checksum\/server-config: "\([a-f0-9]\{64\}\)".*/\1/p' "$work/template-change.yaml")"
test -n "$baseline_checksum"
test -n "$changed_checksum"
if test "$baseline_checksum" = "$changed_checksum"; then
  echo "server config checksum ignored a template-only TOML change" >&2
  exit 1
fi
grep -Fq 'coordinator_only = false' "$work/template-change.yaml"

test "$(grep -Ec '^kind: Ingress$' "$work/enabled.yaml")" -eq 1
test "$(grep -Ec '^kind: PersistentVolumeClaim$' "$work/enabled.yaml")" -eq 1
test "$(grep -Ec '^kind: Deployment$' "$work/enabled.yaml")" -eq 1
test "$(grep -Ec '^kind: NetworkPolicy$' "$work/enabled.yaml")" -eq 1
test "$(grep -Ec '^kind: Service$' "$work/enabled.yaml")" -eq 1
test "$(grep -Ec '^        - name: (atmux|gateway|oauth2-proxy)$' "$work/enabled.yaml")" -eq 3
grep -Fq -- '--trusted-proxy-ip=10.1.112.0/24' "$work/enabled.yaml"
grep -Fq -- '--prompt=login select_account' "$work/enabled.yaml"
grep -Fq -- '--cookie-name=__Host-atmux-v2' "$work/enabled.yaml"
grep -Fq -- '--oidc-groups-claim=identity_provider' "$work/enabled.yaml"
grep -Fq -- '--allowed-group=google' "$work/enabled.yaml"
grep -Fq 'ryanmurf@gmail.com' "$work/enabled.yaml"
grep -Fq 'ryanm@herodevs.com' "$work/enabled.yaml"
grep -Fq 'checksum/allowed-emails:' "$work/enabled.yaml"
grep -Fq 'checksum/server-config:' "$work/enabled.yaml"
grep -Fq 'atmux.dev/secret-revision: "v1"' "$work/enabled.yaml"
grep -Fq 'oauth2-proxy:v7.15.3@sha256:' "$work/enabled.yaml"
grep -Fq 'listen 127.0.0.1:8080;' "$work/enabled.yaml"
grep -Fq 'proxy_pass http://127.0.0.1:7345;' "$work/enabled.yaml"
if grep -Fq 'proxy_pass https://192.168.0.109:7345;' "$work/enabled.yaml"; then
  echo "coordinator mode still routes the public gateway directly to Tron" >&2
  exit 1
fi
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

# The coordinator is a full atmux server but deliberately has no local agent
# profiles, projects, maintenance, compaction, collection, receiver, or mDNS.
grep -Fq 'name: atmux-server-config' "$work/enabled.yaml"
grep -Fq 'project_roots = []' "$work/enabled.yaml"
grep -Fq 'switch_on_launch = false' "$work/enabled.yaml"
grep -Fq 'id = "home"' "$work/enabled.yaml"
grep -Fq 'coordinator_only = true' "$work/enabled.yaml"
grep -Fq 'cert_file = "/etc/atmux/tls/tls.crt"' "$work/enabled.yaml"
grep -Fq 'allow_unauthenticated_loopback = false' "$work/enabled.yaml"
grep -Fq 'proxy_token_file = "/etc/atmux/proxy-token/token"' "$work/enabled.yaml"
grep -Fq 'collect = false' "$work/enabled.yaml"
grep -Fq 'serve = true' "$work/enabled.yaml"
grep -Fq 'receive = false' "$work/enabled.yaml"
grep -Fq 'sqlite_path = "/var/lib/atmux/data/pulse.sqlite3"' "$work/enabled.yaml"
grep -Fq 'id = 4' "$work/enabled.yaml"
grep -Fq 'identity = "ryanmurf@gmail.com"' "$work/enabled.yaml"
grep -Fq 'display_name = "Ryan"' "$work/enabled.yaml"
grep -Fq 'name = "claude-hd"' "$work/enabled.yaml"
grep -Fq 'vendor = "anthropic-oauth"' "$work/enabled.yaml"
grep -Fq 'name = "codex"' "$work/enabled.yaml"
grep -Fq 'vendor = "openai-codex"' "$work/enabled.yaml"
grep -Fq 'name = "grok"' "$work/enabled.yaml"
grep -Fq 'vendor = "xai-grok"' "$work/enabled.yaml"
grep -Fq 'name = "gemini"' "$work/enabled.yaml"
grep -Fq 'vendor = "gemini"' "$work/enabled.yaml"
grep -Fq 'name = "antigravity"' "$work/enabled.yaml"
grep -Fq 'vendor = "antigravity"' "$work/enabled.yaml"
if grep -Fq 'config_dir =' "$work/enabled.yaml"; then
  echo "coordinator unexpectedly rendered an owner-local Pulse config directory" >&2
  exit 1
fi
grep -Fq 'url = "https://192.168.0.109:7345"' "$work/enabled.yaml"
grep -Fq 'url = "https://192.168.0.124:7345"' "$work/enabled.yaml"
grep -Fq 'url = "https://192.168.0.127:7345"' "$work/enabled.yaml"
grep -Fq 'token_file = "/etc/atmux/federation-tokens/tron.token"' "$work/enabled.yaml"
if grep -Fq '[[profiles]]' "$work/enabled.yaml"; then
  echo "coordinator unexpectedly rendered a local launch profile" >&2
  exit 1
fi

grep -Fq "image: \"localhost:32000/atmux@${digest}\"" "$work/enabled.yaml"
grep -Fq 'strategy: {type: Recreate}' "$work/enabled.yaml"
grep -Fq 'automountServiceAccountToken: false' "$work/enabled.yaml"
grep -Fq 'readOnlyRootFilesystem: true' "$work/enabled.yaml"
grep -Fq 'runAsUser: 10001' "$work/enabled.yaml"
grep -Fq 'command: [nc, -z, 127.0.0.1, "7345"]' "$work/enabled.yaml"
grep -Fq 'ATMUX_READINESS_TOKEN' "$work/enabled.yaml"
grep -Fq '"ok"[[:space:]]*:[[:space:]]*true' "$work/enabled.yaml"
grep -Fq 'storageClassName: "microk8s-hostpath"' "$work/enabled.yaml"
grep -Fq 'helm.sh/resource-policy: keep' "$work/enabled.yaml"
grep -Fq 'claimName: atmux-server-data' "$work/enabled.yaml"
grep -Fq 'name: federation-tokens' "$work/enabled.yaml"
grep -Fq 'key: "midnight"' "$work/enabled.yaml"
grep -Fq 'cidr: "192.168.0.109/32"' "$work/enabled.yaml"
grep -Fq 'cidr: "192.168.0.124/32"' "$work/enabled.yaml"
grep -Fq 'cidr: "192.168.0.127/32"' "$work/enabled.yaml"
test "$(grep -Ec '^          resources:$' "$work/enabled.yaml")" -eq 4
grep -Fq 'memory: 512Mi' "$work/enabled.yaml"
grep -Fq 'memory: 256Mi' "$work/enabled.yaml"
grep -Fq 'memory: 128Mi' "$work/enabled.yaml"

if grep -Fq 'selector: {app: atmux-gateway}' "$work/enabled.yaml"; then
  echo "credential-bearing gateway unexpectedly has a cluster-network identity" >&2
  exit 1
fi
if grep -Eq '^  ports:.*7345|^    - port: 7345' "$work/enabled.yaml"; then
  echo "coordinator port unexpectedly exposed by a Service" >&2
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

must_fail_with() {
  local expected="$1"
  shift
  must_fail "$@"
  if ! grep -Fq "$expected" "$work/rejected.yaml"; then
    echo "chart failure did not contain expected diagnostic: $expected" >&2
    cat "$work/rejected.yaml" >&2
    exit 1
  fi
}

# Helm's upgrade --reuse-values mode can present a new chart with no `server`
# key at all. Treat that legacy shape as the safe gateway-only topology rather
# than dereferencing a nil map.
"$helm_bin" template atmux-web "$chart" --namespace murphytek \
  --set-json server=null >"$work/legacy-no-server.yaml"
assert_unique_top_level_keys "$work/legacy-no-server.yaml"
if grep -Eq '^kind: PersistentVolumeClaim$|name: atmux-server-config' "$work/legacy-no-server.yaml"; then
  echo "legacy values without a server map unexpectedly enabled the coordinator" >&2
  exit 1
fi
grep -Fq 'proxy_pass https://192.168.0.109:7345;' "$work/legacy-no-server.yaml"

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

must_fail --set server.enabled=true
must_fail --set server.enabled=true --set "server.image=localhost:32000/atmux@${digest}"
must_fail_with 'server.node must be a map when server.enabled=true' \
  "${enabled_values[@]}" --set-json server.node=null
must_fail_with 'server.persistence must be a map when server.enabled=true' \
  "${enabled_values[@]}" --set-json server.persistence=null
must_fail_with 'server.pulse must be a map when server.enabled=true' \
  "${enabled_values[@]}" --set-json server.pulse=null
must_fail_with 'server.resources must be a map when server.enabled=true' \
  "${enabled_values[@]}" --set-json server.resources=null
must_fail_with 'server.federationTokenSecret must be distinct from the public gateway proxy token Secret' \
  "${enabled_values[@]}" --set server.federationTokenSecret=atmux-proxy-token
must_fail_with 'server.pulse.accounts must contain at least one account when server.pulse.serve=true' \
  "${enabled_values[@]}" --set-json 'server.pulse.accounts=[]'
must_fail "${enabled_values[@]}" --set server.image=localhost:32000/atmux:latest
must_fail "${enabled_values[@]}" --set server.persistence.size=
must_fail "${enabled_values[@]}" --set-json 'server.machines=[{"id":"tron","label":"Tron","address":"999.168.0.109","port":7345,"tokenKey":"tron"}]'
must_fail "${enabled_values[@]}" --set-json 'server.machines=[{"id":"home","label":"Collision","address":"192.168.0.109","port":7345,"tokenKey":"tron"}]'
must_fail "${enabled_values[@]}" --set-json 'server.machines=[{"id":"tron","label":"Tron","address":"192.168.0.109","port":7345,"tokenKey":"bad/key"}]'
must_fail "${enabled_values[@]}" --set-json 'server.machines=[{"id":"tron","label":"Tron","address":"192.168.0.109","port":7345,"tokenKey":"shared"},{"id":"max","label":"Max","address":"192.168.0.124","port":7345,"tokenKey":"shared"}]'
must_fail "${enabled_values[@]}" --set-json 'server.pulse.accounts=[{"id":1,"identity":"one@example.com"},{"id":1,"identity":"two@example.com"}]'
must_fail "${enabled_values[@]}" --set-json 'server.pulse.accounts=[{"id":4,"identity":"ryanmurf@gmail.com","profiles":[{"name":"codex","vendor":"unknown"}]}]'
must_fail "${enabled_values[@]}" --set-json 'server.pulse.accounts=[{"id":4,"identity":"ryanmurf@gmail.com","profiles":[{"name":"codex","vendor":"openai-codex"},{"name":"codex","vendor":"openai-codex"}]}]'

echo "Helm render security tests passed"
