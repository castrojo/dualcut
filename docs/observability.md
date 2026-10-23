# Dualcut Observability & Operational Readiness

This document details the operational readiness, logging infrastructure, diagnostic signals, and observability guidelines for Dualcut (`org.tunaos.dualcut`).

## Executive Summary

- **Component Name**: Dualcut (`org.tunaos.dualcut`)
- **Architecture**: GNOME video editor in Rust, built with GTK4, Libadwaita, GStreamer (GES), and Vello. It also embeds an HTTP API server for agentic automation and script execution.
- **Maintenance Status**: Active (development moved to `shrimply` for new features).
- **Distribution**: Flatpak (TunaOS OCI registry and direct releases).

## Telemetry & Data Policy

**This project configures and authorizes no external telemetry backends.** All application diagnostics and session logs remain strictly local. This design keeps privacy-first observability:

- HTTP API is bound exclusively to `127.0.0.1` (localhost)
- No external metrics export or remote telemetry transmission
- GStreamer debug output stays on the local machine
- Project documents are never sent outside the local system

### Future Telemetry Considerations

If the team designates an operational backend in the future, we recommend the following architecture:

1. **OpenTelemetry via Rust `tracing`** (`tracing-opentelemetry`):
   - Instrument tasks that run for a long time: project render, silence detection, script execution
   - Use bounded span attributes (project ID hashes, clip format, pipeline state transitions)
   - **Never** export a user's raw content or file paths

2. **Metrics Collection**:
   - Expose operational metrics (export duration, frames rendered, pipeline init latency) only via local, pull-based endpoints (e.g., `/metrics` on localhost)
   - Use standard event listeners where applicable

## Diagnostic Signal Sources

### Systemd Journal & Flatpak Logs

When the Flatpak runs, session logs flow through standard output/stderr to the systemd journal:

```bash
# Stream live logs for Dualcut
journalctl --user -f -u org.tunaos.dualcut

# Retrieve recent error events (last hour)
journalctl --user-unit=org.tunaos.dualcut --since "1 hour ago" -p err
```

### GStreamer & GLib Debug Logging

GStreamer pipelines send many diagnostic channels through environment variables:

```bash
# Debug GStreamer Editing Services (GES) and video rendering
GST_DEBUG=2,ges:4,gespipeline:5 flatpak run org.tunaos.dualcut

# Trace GTK4 and GLib debug events
G_MESSAGES_DEBUG=all flatpak run org.tunaos.dualcut
```

Common `GST_DEBUG` levels: 0=none, 1=error, 2=warning, 3=fixme, 4=info, 5=debug, 6=log, 7=trace.

### HTTP Agent API Diagnostics

While the API server runs (default port 7357):
- Access logs and errors print to standard error
- Health check endpoint: `GET /status` verifies server responsiveness
- Project API: `GET /project` validates document structure

## Operational Health Verification

| Component | Health Signal | Verification |
|-----------|---------------|---------------|
| **Flatpak Launch** | GTK window initialization | `flatpak run org.tunaos.dualcut --version` |
| **GStreamer Pipeline** | GES element availability | `gst-inspect-1.0 ges` |
| **Rendering** | Encoder plugins (H.264, VP8) | `gst-inspect-1.0 -k h264` `gst-inspect-1.0 -k vp8` |
| **HTTP API** | Local server response | `curl http://localhost:7357/status` (while serving) |
| **Project Validation** | JSON schema compliance | `python3 -c "import json; json.load(open('project.json'))"` |

## Operations Troubleshooting

### GStreamer Pipeline Issues

Use environment variable debugging to isolate media encode/decode problems:

```bash
# Enable all GStreamer debugging
GST_DEBUG=*:5 flatpak run org.tunaos.dualcut 2>&1 | grep -i error

# Check specific element availability
gst-inspect-1.0 | grep -E 'h264|vp8|aac|vorbis'
```

### Proxy Media Cache

Proxy media lives in `.dualcut-cache/` inside the project directory:
- Safe to delete; the build regenerates it on the next preview
- Reduces the time it takes to scrub large videos
- Disable with *Preferences → Use proxy media*

### Deprecation Guidance

The team maintains its important security and crash fixes in Dualcut. For new features, evaluate against [shrimply](https://github.com/soirihiroka/shrimply) — the active development project.
