# Monitoring — synapse-gateway Grafana dashboard

This directory contains an example **Grafana dashboard** for synapse-gateway:

- [`synapse-gateway-dashboard.json`](synapse-gateway-dashboard.json) — a portable dashboard you can import into any Grafana, or provision via a ConfigMap.

It visualises traffic, latency, token usage, resilience (retries / circuit breakers) and cost-ledger health, using the Prometheus metrics the gateway exposes on its metrics port.

## What it shows

| Row | Panels |
|-----|--------|
| **Diagnostics** (collapsed) | `count by (__name__)({job=$job})` — lists every series scraped for the job; if it's empty, the scrape/ServiceMonitor is misconfigured. |
| **Traffic** | request rate by `route`, by `lane` (standard vs native Vertex), and by upstream `model`. |
| **Latency** | request duration p50 / p95 / p99 overall, and p95 by `route`. |
| **Tokens** | input & output token rate by `model`, and total tokens consumed (last 1h). |
| **Resilience** | per-leg circuit-breaker state (closed / open / half-open), leg call rate by `outcome`, retry attempts, and breaker transitions. |
| **Cost ledger** | ledger error rate by `backend`, and dropped-event rate (queue full). |
| **Embeddings** | embedding request rate by `route` and by `provider` (fallback usage), and embedding latency p95 by `route`. |

## Metrics it depends on

All are emitted by `metrics-exporter-prometheus` on the gateway's metrics endpoint (`SYNAPSE_METRICS_ADDR`, default `0.0.0.0:9090`, path `/metrics`):

| Metric | Type | Labels |
|--------|------|--------|
| `synapse_requests_total` | counter | `route`, `model`, `system`, `lane` |
| `synapse_request_duration_seconds` | histogram | `route`, `model`, `system`, `lane` |
| `synapse_input_tokens_total` / `synapse_output_tokens_total` | counter | `route`, `model`, `system`, `lane` |
| `synapse_resilience_calls_total` | counter | `label`, `outcome` |
| `synapse_resilience_call_duration_seconds` | histogram | `label`, `outcome` |
| `synapse_resilience_retry_attempts_total` | counter | `label` |
| `synapse_resilience_breaker_state` | gauge | `name` (0 = closed, 1 = open, 2 = half-open) |
| `synapse_resilience_breaker_transitions_total` | counter | `name`, `transition` |
| `synapse_ledger_errors_total` | counter | `backend` |
| `synapse_ledger_dropped_total` | counter | — |
| `synapse_embeddings_total` | counter | `route`, `model`, `provider` |
| `synapse_embedding_duration_seconds` | histogram | `route`, `model`, `provider` |

Embedding token usage and cost are recorded to the **ledger** (not Prometheus) as `UsageEvent`s with `op = "embedding"` — see [embeddings.md](embeddings.md).

The `$job` dashboard variable (a textbox, default `synapse-gateway`) selects the Prometheus `job` label; every panel filters on `{job="$job"}`.

## How to use it

### A. Import into any Grafana (quickest)

1. Grafana → **Dashboards → New → Import**.
2. Upload `synapse-gateway-dashboard.json` (or paste its contents).
3. Select your Prometheus data source when prompted.
4. If your scrape job isn't named `synapse-gateway`, change the **Prometheus job** variable at the top.

You need a Prometheus that scrapes the gateway's `/metrics` (9090). Locally:

```bash
# point Prometheus at http://<gateway-host>:9090/metrics
curl -s localhost:9090/metrics | grep -E '^synapse_'
```

### B. Provision in-cluster (kube-prometheus-stack)

The infrastructure repo carries the same dashboard as an example ConfigMap:

```
acm/config-root/base/monitoring/kube-prometheus-stack/grafana-dashboard-synapse-gateway.yaml
```

It is a `ConfigMap` in the `monitoring` namespace, labelled `grafana_dashboard: "1"` and annotated `grafana_folder: Qonstrue Platform` — the kube-prometheus-stack Grafana **sidecar auto-discovers** any such ConfigMap and loads the dashboard (data source uid `prometheus`, matching the other infrastructure dashboards).

To actually provision it, register the file in that directory's `kustomization.yaml`, alongside the other dashboards:

```yaml
resources:
  ...
  - grafana-dashboard-talos.yaml
  - grafana-dashboard-wine2o2.yaml
  - grafana-dashboard-synapse-gateway.yaml   # add this line
```

On merge, Config Sync applies the ConfigMap and the dashboard appears in Grafana under **Qonstrue Platform → synapse-gateway**.

Metrics reach Prometheus via the gateway's **ServiceMonitor** (`acm/infrastructure/base/synapse-gateway/servicemonitor.yaml`), which scrapes the `metrics` port (9090) at `/metrics` every 30s.

## Regenerating the ConfigMap from the JSON

The example ConfigMap is just this JSON wrapped in a ConfigMap. If you edit `synapse-gateway-dashboard.json`, regenerate the wrapper:

```bash
python3 - <<'PY'
js = open('_docs/synapse-gateway-dashboard.json').read().rstrip('\n')
body = '\n'.join(('    ' + l) if l.strip() else '' for l in js.splitlines())
head = ("apiVersion: v1\nkind: ConfigMap\nmetadata:\n"
        "  name: grafana-dashboard-synapse-gateway\n  namespace: monitoring\n"
        "  labels:\n    grafana_dashboard: \"1\"\n"
        "  annotations:\n    grafana_folder: Qonstrue Platform\n"
        "data:\n  synapse-gateway.json: |-\n")
open('grafana-dashboard-synapse-gateway.yaml','w').write(head + body + '\n')
PY
```

Keep the dashboard `uid` (`synapse-gateway`) stable so links and saved state survive edits.
