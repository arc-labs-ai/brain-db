# 14.10 References

References for observability and operations.

## 1. Foundational works

- **Beyer et al., "Site Reliability Engineering" (Google SRE Book, 2016).** [sre.google/sre-book/table-of-contents](https://sre.google/sre-book/table-of-contents/). Free online; the canonical SRE reference.

- **Beyer et al., "The Site Reliability Workbook" (Google, 2018).** [sre.google/workbook/table-of-contents](https://sre.google/workbook/table-of-contents/). Practical companion.

- **Limoncelli, Hogan, Chalup, "The Practice of Cloud System Administration" (2014).**

## 2. Observability standards

- **OpenTelemetry** — [opentelemetry.io](https://opentelemetry.io/). The standard for tracing, metrics, and logs.

- **OpenMetrics specification** — [openmetrics.io](https://openmetrics.io/). The metric format Brain uses.

- **W3C Trace Context** — [w3.org/TR/trace-context](https://www.w3.org/TR/trace-context/). The trace propagation standard.

## 3. Metrics tools

- **Prometheus** — [prometheus.io](https://prometheus.io/). The de facto metrics platform.

- **Prometheus naming guide** — [prometheus.io/docs/practices/naming](https://prometheus.io/docs/practices/naming/).

- **Grafana** — [grafana.com](https://grafana.com/). Dashboarding.

- **VictoriaMetrics** — [victoriametrics.com](https://victoriametrics.com/). Prometheus-compatible alternative.

## 4. Logging tools

- **Loki** — [grafana.com/oss/loki](https://grafana.com/oss/loki/). Logs as time series.

- **Elasticsearch + Kibana** — [elastic.co](https://www.elastic.co/). Comprehensive log analysis.

- **Vector** — [vector.dev](https://vector.dev/). Log routing / processing.

## 5. Tracing tools

- **Jaeger** — [jaegertracing.io](https://www.jaegertracing.io/). Open-source tracing.

- **Tempo** — [grafana.com/oss/tempo](https://grafana.com/oss/tempo/). High-volume tracing.

- **Honeycomb** — [honeycomb.io](https://www.honeycomb.io/). Commercial tracing platform.

## 6. Alerting

- **Alertmanager** — [prometheus.io/docs/alerting/latest/alertmanager](https://prometheus.io/docs/alerting/latest/alertmanager/). The standard alerting platform.

- **PagerDuty** — [pagerduty.com](https://www.pagerduty.com/). Incident management.

## 7. Capacity planning resources

- **Bondi, "Foundations of Software and System Performance Engineering" (2014).** Performance engineering textbook.

- **Gunther, "Guerrilla Capacity Planning" (2007).** Pragmatic capacity planning.

- **USE method (Brendan Gregg)** — [brendangregg.com/usemethod.html](https://www.brendangregg.com/usemethod.html). Resource analysis methodology.

## 8. Linux performance tools

- **Brendan Gregg's site** — [brendangregg.com](https://www.brendangregg.com/). Authoritative Linux performance.

- **`pprof`** — Google's profiling format. [github.com/google/pprof](https://github.com/google/pprof).

## 9. Audit logging

- **Logging guidelines from NIST and ISO** — for compliance audit logging.

- **Event logging best practices** — [owasp.org/www-community/Logging_Cheat_Sheet](https://owasp.org/www-community/Logging_Cheat_Sheet).

## 10. SLOs and error budgets

- **SLO blog (Google)** — [sre.google/sre-book/service-level-objectives](https://sre.google/sre-book/service-level-objectives/).

- **The Site Reliability Workbook, chapter on SLOs** — [sre.google/workbook/implementing-slos](https://sre.google/workbook/implementing-slos/).

## 11. Brain-internal references

- See [01. System Architecture](../01_system_architecture/) for what's being observed.
- See [11. Background Workers](../11_background_workers/) for worker observability.
- See [13. SDK Design](../13_sdk_design/07_observability.md) for SDK observability.
