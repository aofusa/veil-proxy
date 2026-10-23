{{- define "veil.name" -}}
{{- .Chart.Name | trunc 63 | trimSuffix "-" -}}
{{- end -}}

{{- define "veil.fullname" -}}
{{- if contains .Chart.Name .Release.Name -}}
{{- .Release.Name | trunc 63 | trimSuffix "-" -}}
{{- else -}}
{{- printf "%s-%s" .Release.Name .Chart.Name | trunc 63 | trimSuffix "-" -}}
{{- end -}}
{{- end -}}

{{- define "veil.labels" -}}
helm.sh/chart: {{ printf "%s-%s" .Chart.Name .Chart.Version | replace "+" "_" }}
{{ include "veil.selectorLabels" . }}
app.kubernetes.io/version: {{ .Chart.AppVersion | quote }}
app.kubernetes.io/managed-by: {{ .Release.Service }}
{{- end -}}

{{- define "veil.selectorLabels" -}}
app.kubernetes.io/name: {{ include "veil.name" . }}
app.kubernetes.io/instance: {{ .Release.Name }}
{{- end -}}

{{- define "veil.serviceAccountName" -}}
{{- if .Values.serviceAccount.create -}}
{{- default (include "veil.fullname" .) .Values.serviceAccount.name -}}
{{- else -}}
{{- default "default" .Values.serviceAccount.name -}}
{{- end -}}
{{- end -}}

{{- define "veil.image" -}}
{{- if .Values.image.digest -}}
{{ .Values.image.repository }}@{{ .Values.image.digest }}
{{- else -}}
{{ .Values.image.repository }}:{{ default (printf "musl-v%s" .Chart.AppVersion) .Values.image.tag }}
{{- end -}}
{{- end -}}

{{- define "veil.configMapName" -}}
{{- default (include "veil.fullname" .) .Values.existingConfigMap -}}
{{- end -}}

{{/*
The default configuration (F-254): the same as the container image's default (packaging/defaults), adapted to a
pod that runs as non-root from the start (no privilege dropping) with a read-only root filesystem.
*/}}
{{- define "veil.defaultConfig" -}}
# Default configuration of the veil Helm chart. Replace it with the `config` value.
# Every key: https://github.com/aofusa/veil-proxy/blob/main/examples/config.toml

[server]
# Plaintext HTTP/1.1 + h2c (prior knowledge).
listen_cleartext = "0.0.0.0:8080"
{{- if .Values.tls.secretName }}
# TLS (HTTP/1.1 and HTTP/2 via ALPN).
listen = "0.0.0.0:8443"
{{- end }}
enable_secure_redirect = false
http2_enabled = true
threads = {{ .Values.defaults.threads }}
graceful_shutdown_timeout_secs = {{ sub .Values.terminationGracePeriodSeconds 5 | max 1 }}

[logging]
level = {{ .Values.defaults.logLevel | quote }}

[access_log]
enabled = {{ .Values.defaults.accessLog }}
format = "text"

[security]
# The pod runs as non-root (65532) from the start; veil adds its own seccomp filter and Landlock.
allow_security_failures = false
enable_seccomp = true
seccomp_mode = "filter"
enable_landlock = true
enable_sandbox = false
landlock_read_paths = [
  "/var/www",
  "/etc/veil",
  "/etc/resolv.conf",
  "/etc/hosts",
  "/etc/nsswitch.conf",
  "/etc/ssl",
  "/lib",
  "/usr",
]
landlock_write_paths = []

[performance]
huge_pages_enabled = false
reuseport_balancing = "cbpf"
{{- if .Values.tls.secretName }}

[tls]
cert_path = "/etc/veil/tls/tls.crt"
key_path = "/etc/veil/tls/tls.key"
ktls_enabled = false
auto_reload = true
reload_interval_secs = 5
{{- end }}
{{- if and .Values.http3.enabled .Values.tls.secretName }}

[http3]
enabled = true
listen = "0.0.0.0:8443"
{{- end }}
{{- if .Values.metrics.enabled }}

[prometheus]
enabled = true
path = "/__metrics"
allowed_ips = [{{ range $i, $ip := .Values.metrics.allowedIps }}{{ if $i }}, {{ end }}{{ $ip | quote }}{{ end }}]
{{- end }}

[[route]]
[route.conditions]
path = "/"
[route.action]
type = "File"
path = "/var/www/"
[route.security]
allowed_methods = ["HEAD", "GET"]
{{- end -}}

{{- define "veil.config" -}}
{{- if .Values.config -}}
{{ .Values.config }}
{{- else -}}
{{ include "veil.defaultConfig" . }}
{{- end -}}
{{- end -}}
