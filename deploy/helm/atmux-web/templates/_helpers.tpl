{{- define "atmux-web.serverConfig" -}}
{{- $server := .Values.server | default dict -}}
{{- $node := get $server "node" | default dict -}}
{{- $pulse := get $server "pulse" | default dict -}}
{{- $accounts := get $pulse "accounts" | default list -}}
{{- $machines := get $server "machines" | default list -}}
{{- $events := get $server "events" | default dict -}}
{{- $redpanda := get $events "redpanda" | default dict -}}
{{- $summaries := get $server "summaries" | default dict -}}
{{- $registry := get $server "registry" | default dict -}}
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

{{- if get $events "enabled" }}

[events]
inject_hooks = false
directory = "/var/lib/atmux/data/events"
max_bytes = {{ get $events "maxBytes" | int64 }}
segment_bytes = {{ get $events "segmentBytes" | int64 }}
retention_seconds = {{ get $events "retentionSeconds" | int64 }}
{{- if get $redpanda "enabled" }}

[events.redpanda]
brokers = {{ get $redpanda "brokers" | toJson }}
topic = {{ get $redpanda "topic" | quote }}
tenant_id = {{ get $redpanda "tenantId" | quote }}
{{- end }}
{{- end }}

{{- if get $summaries "enabled" }}

[summaries]
enabled = true
endpoint = {{ get $summaries "endpoint" | quote }}
model = {{ get $summaries "model" | quote }}
allow_http_hosts = {{ get $summaries "allowHttpHosts" | default list | toJson }}
store_dir = "/var/lib/atmux/data/summaries"
min_interval_seconds = {{ get $summaries "minIntervalSeconds" | default 300 | int64 }}
daily_request_budget = {{ get $summaries "dailyRequestBudget" | default 500 | int64 }}
{{- with get $summaries "searchTenantId" }}
search_tenant_id = {{ . | quote }}
{{- end }}
{{- end }}
{{- if get $registry "enabled" }}

[registry]
enabled = true
directory = "/var/lib/atmux/data/registry"
bundle_quota_bytes = {{ get $registry "bundleQuotaBytes" | default 21474836480 | int64 }}
restore_on_start = {{ get $registry "restoreOnStart" | default false }}
restore_machines = {{ get $registry "restoreMachines" | default list | toJson }}
{{- end }}

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
