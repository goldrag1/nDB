{{- define "ndb.name" -}}
{{- default .Chart.Name .Values.nameOverride | trunc 63 | trimSuffix "-" -}}
{{- end -}}

{{- define "ndb.fullname" -}}
{{- printf "%s-%s" .Release.Name (include "ndb.name" .) | trunc 63 | trimSuffix "-" -}}
{{- end -}}

{{- define "ndb.labels" -}}
app.kubernetes.io/name: {{ include "ndb.name" . }}
app.kubernetes.io/instance: {{ .Release.Name }}
app.kubernetes.io/managed-by: {{ .Release.Service }}
helm.sh/chart: {{ printf "%s-%s" .Chart.Name .Chart.Version }}
{{- end -}}

{{- define "ndb.selectorLabels" -}}
app.kubernetes.io/name: {{ include "ndb.name" . }}
app.kubernetes.io/instance: {{ .Release.Name }}
{{- end -}}

{{- define "ndb.image" -}}
{{- printf "%s:%s" .Values.image.repository (.Values.image.tag | default .Chart.AppVersion) -}}
{{- end -}}
