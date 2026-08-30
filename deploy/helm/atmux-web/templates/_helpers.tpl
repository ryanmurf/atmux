{{- define "atmux-web.serverConfig" -}}
{{- $server := .Values.server | default dict -}}
{{- $node := get $server "node" | default dict -}}
{{- $pulse := get $server "pulse" | default dict -}}
{{- $accounts := get $pulse "accounts" | default list -}}
{{- $machines := get $server "machines" | default list -}}
[general]
project_roots = []
favorite_dirs = []
refresh_ms = 750
preview_lines = 160
switch_on_launch = false

[auto_compact]
enabled = false
inactivity_minutes = 15
input_tokens = 200000
poll_seconds = 30

[maintenance]
enabled = false
interval_minutes = 30
update_timeout_seconds = 180
relaunch_limit = 4

[node]
id = {{ get $node "id" | quote }}
label = {{ get $node "label" | quote }}
coordinator_only = true

[node.tls]
cert_file = "/etc/atmux/tls/tls.crt"
key_file = "/etc/atmux/tls/tls.key"
ca_file = "/etc/atmux/tls/ca.crt"

[discovery]
enabled = false

[web]
allow_unauthenticated_loopback = false
proxy_token_file = "/etc/atmux/proxy-token/token"

[pulse]
collect = false
serve = {{ get $pulse "serve" | default false }}
receive = false

[pulse.database]
sqlite_path = "/var/lib/atmux/data/pulse.sqlite3"
{{- range $account := $accounts }}

[[pulse.accounts]]
id = {{ get $account "id" }}
identity = {{ get $account "identity" | quote }}
{{- with get $account "displayName" }}
display_name = {{ . | quote }}
{{- end }}
{{- range $profile := get $account "profiles" | default list }}

[[pulse.accounts.profiles]]
name = {{ get $profile "name" | quote }}
vendor = {{ get $profile "vendor" | quote }}
{{- end }}
{{- end }}
{{- range $machine := $machines }}

[[machines]]
id = {{ get $machine "id" | quote }}
label = {{ get $machine "label" | quote }}
url = {{ printf "https://%s:%v" (get $machine "address") (get $machine "port") | quote }}
token_file = {{ printf "/etc/atmux/federation-tokens/%s.token" (get $machine "id") | quote }}
{{- end }}
{{- end -}}
