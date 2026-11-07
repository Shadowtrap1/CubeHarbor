{{- define "cubeharbor.fullname" -}}
{{- if contains .Chart.Name .Release.Name -}}{{ .Release.Name | trunc 50 | trimSuffix "-" }}{{- else -}}{{ printf "%s-%s" .Release.Name .Chart.Name | trunc 50 | trimSuffix "-" }}{{- end -}}
{{- end -}}

{{- define "cubeharbor.selector" -}}
app.kubernetes.io/name: cubeharbor
app.kubernetes.io/instance: {{ .Release.Name }}
{{- end -}}

{{- define "cubeharbor.labels" -}}
{{ include "cubeharbor.selector" . }}
app.kubernetes.io/version: {{ .Chart.AppVersion | quote }}
app.kubernetes.io/managed-by: {{ .Release.Service }}
helm.sh/chart: {{ printf "%s-%s" .Chart.Name .Chart.Version }}
{{- with .Values.commonLabels }}
{{ toYaml . }}
{{- end }}
{{- end -}}

{{/* Pod template metadata: (dict "root" $ "component" "gateway" "values" .Values.gateway
     ["annotations" (dict ...)]). Labels: the selector labels, which always win, then
     commonLabels, then podLabels. Annotations: the chart's own checksums plus podAnnotations. */}}
{{- define "cubeharbor.podMetadata" -}}
{{- $labels := merge (dict "app.kubernetes.io/component" .component) (fromYaml (include "cubeharbor.selector" .root)) (.root.Values.commonLabels | default dict) (.values.podLabels | default dict) -}}
labels: {{- toYaml $labels | nindent 2 }}
{{- with merge (dict) (.annotations | default dict) (.values.podAnnotations | default dict) }}
annotations: {{- toYaml . | nindent 2 }}
{{- end }}
{{- end -}}

{{- define "cubeharbor.tag" -}}{{ .Values.image.tag | default .Chart.AppVersion }}{{- end -}}

{{- define "cubeharbor.image" -}}
{{ .root.Values.image.registry }}/cubeharbor-{{ .name }}:{{ include "cubeharbor.tag" .root }}
{{- end -}}

{{- define "cubeharbor.curseforgeSecret" -}}
{{ .Values.curseforge.existingSecret | default (printf "%s-curseforge" (include "cubeharbor.fullname" .)) }}
{{- end -}}

{{- define "cubeharbor.curseforgeEnv" -}}
{{- if or .Values.curseforge.apiKey .Values.curseforge.existingSecret }}
- name: CURSEFORGE_API_KEY
  valueFrom:
    secretKeyRef:
      name: {{ include "cubeharbor.curseforgeSecret" . }}
      key: apiKey
{{- end }}
{{- end -}}

{{- define "cubeharbor.pod" -}}
{{- with .Values.imagePullSecrets }}
imagePullSecrets: {{ toYaml . | nindent 2 }}
{{- end }}
securityContext: {{- toYaml (.Values.podSecurityContext | default dict) | nindent 2 }}
{{- end -}}

{{/* host:port for listeners, with brackets for IPv6 binds: (dict "root" . "port" 8080) */}}
{{- define "cubeharbor.listen" -}}
{{- $h := .root.Values.network.bindAddress | default "0.0.0.0" -}}
{{- if and (contains ":" $h) (not (hasPrefix "[" $h)) }}[{{ $h }}]{{ else }}{{ $h }}{{ end }}:{{ .port }}
{{- end -}}

{{/* ipFamilyPolicy / ipFamilies lines for a Service spec. */}}
{{- define "cubeharbor.ipFamily" -}}
{{- with .Values.network.ipFamilyPolicy }}
ipFamilyPolicy: {{ . }}
{{- end }}
{{- with .Values.network.ipFamilies }}
ipFamilies: {{ toYaml . | nindent 2 }}
{{- end }}
{{- end -}}

{{/* SERVER_POD_DEFAULTS for the operator: serverPods plus the global pull secrets, as JSON. */}}
{{- define "cubeharbor.serverPodDefaults" -}}
{{- $s := .Values.serverPods | default dict -}}
{{- $d := dict
  "imagePullSecrets" (concat (.Values.imagePullSecrets | default list) ($s.imagePullSecrets | default list) | default list)
  "nodeSelector" ($s.nodeSelector | default dict)
  "tolerations" ($s.tolerations | default list)
  "topologySpreadConstraints" ($s.topologySpreadConstraints | default list)
  "labels" ($s.labels | default dict)
  "annotations" ($s.annotations | default dict) -}}
{{- with $s.affinity }}{{ $_ := set $d "affinity" . }}{{ end -}}
{{- with $s.priorityClassName }}{{ $_ := set $d "priorityClassName" . }}{{ end -}}
{{- with $s.runtimeClassName }}{{ $_ := set $d "runtimeClassName" . }}{{ end -}}
{{- if kindIs "map" $s.securityContext }}{{ $_ := set $d "securityContext" $s.securityContext }}{{ end -}}
{{ toJson $d }}
{{- end -}}

{{- define "cubeharbor.containerSecurity" -}}
securityContext:
  allowPrivilegeEscalation: false
  readOnlyRootFilesystem: true
  capabilities: { drop: [ALL] }
{{- end -}}

{{- define "cubeharbor.scheduling" -}}
{{- with .nodeSelector }}
nodeSelector: {{ toYaml . | nindent 2 }}
{{- end }}
{{- with .tolerations }}
tolerations: {{ toYaml . | nindent 2 }}
{{- end }}
{{- with .affinity }}
affinity: {{ toYaml . | nindent 2 }}
{{- end }}
{{- with .topologySpreadConstraints }}
topologySpreadConstraints: {{ toYaml . | nindent 2 }}
{{- end }}
{{- with .priorityClassName }}
priorityClassName: {{ . }}
{{- end }}
{{- end -}}

{{- define "cubeharbor.remotes" -}}
{{- $out := list -}}
{{- range .Values.dashboard.remotes }}{{ $out = append $out (printf "%s=%s" .name .url) }}{{ end -}}
{{ join "," $out }}
{{- end -}}

{{/* Objects the operator creates for each server. */}}
{{- define "cubeharbor.operatorWorkloadRules" }}
  - apiGroups: [apps]
    resources: [statefulsets]
    verbs: [get, list, watch, create, patch, update, delete]
  - apiGroups: [""]
    resources: [services, configmaps, secrets]
    verbs: [get, list, watch, create, patch, update, delete]
  - apiGroups: [""]
    resources: [pods]
    verbs: [get, list, watch]
  - apiGroups: [networking.k8s.io]
    resources: [ingresses, networkpolicies]
    verbs: [get, list, watch, create, patch, update, delete]
  - apiGroups: ["", events.k8s.io]
    resources: [events]
    verbs: [create, patch]
{{- end -}}

{{/* Namespaces the dashboard manages: its default namespace plus dashboard.allowedNamespaces. */}}
{{- define "cubeharbor.dashboardNamespaces" -}}
{{- $default := .Values.dashboard.defaultNamespace | default .Release.Namespace -}}
{{- concat (list $default) .Values.dashboard.allowedNamespaces | uniq | join "," -}}
{{- end -}}

{{/* What the dashboard may do in a namespace it manages. */}}
{{- define "cubeharbor.dashboardRules" }}
  - apiGroups: [cubeharbor.dev]
    resources: [minecraftservers]
    verbs: [get, list, watch, create, update, patch, delete]
  - apiGroups: [""]
    resources: [pods]
    verbs: [get, list, watch]
  - apiGroups: [""]
    resources: [pods/log]
    verbs: [get]
  # RCON passwords live in each server's <name>-cubeharbor secret.
  - apiGroups: [""]
    resources: [secrets]
    verbs: [get]
  - apiGroups: [""]
    resources: [persistentvolumeclaims]
    verbs: [get, delete]
{{- end -}}

{{/* In-cluster URL of the dashboard, for the tunnel to forward to. */}}
{{- define "cubeharbor.dashboardUrl" -}}
http://{{ include "cubeharbor.fullname" . }}-dashboard.{{ .Release.Namespace }}.svc:{{ .Values.dashboard.service.port }}
{{- end -}}

{{/* cloudflared config.yaml for a locally-managed tunnel (cloudflareTunnel.mode=local). */}}
{{- define "cubeharbor.cloudflaredConfig" -}}
{{- $t := .Values.cloudflareTunnel -}}
tunnel: {{ $t.tunnelId | quote }}
credentials-file: /etc/cloudflared/credentials/credentials.json
{{- with $t.originRequest }}
originRequest: {{- toYaml . | nindent 2 }}
{{- end }}
ingress:
{{- if .Values.dashboard.enabled }}
  - hostname: {{ $t.hostname | quote }}
    service: {{ include "cubeharbor.dashboardUrl" . | quote }}
{{- end }}
{{- with $t.extraIngress }}
{{- toYaml . | nindent 2 }}
{{- end }}
  - service: http_status:404
{{- end -}}
