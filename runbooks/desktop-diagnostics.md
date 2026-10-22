# Desktop Diagnostics & Operational Runbook — Dualcut

This runbook provides step-by-step diagnostic workflows, failure mode triage, and remediation procedures for Dualcut (`org.tunaos.dualcut`).

---

## 1. Triage Workflow

```
[Issue Reported]
       │
       ▼
1. Verify Flatpak Sandbox & Runtime Environment
       │
       ▼
2. Inspect Journalctl & GStreamer Debug Streams
       │
       ▼
3. Validate Media Codecs & GES Pipeline
       │
       ▼
4. Triage HTTP Agent API & Scripting Server
       │
       ▼
5. Apply Remediation / Reset Application State
```

---

## 2. Standard Triage Procedures

### Step 1: Verify Sandbox & App Launch
Check the installed Flatpak package state and system dependencies:

```bash
# Check installed application info
flatpak info org.tunaos.dualcut

# Launch from terminal to inspect stdout/stderr
flatpak run org.tunaos.dualcut
```

### Step 2: Extract Debug & GStreamer Log Streams
Enable full diagnostic tracing for GTK4 and GStreamer:

```bash
# Capture user journal logs
journalctl --user -e -u org.tunaos.dualcut

# Verbose GStreamer debug output for GES and playback
GST_DEBUG=3,ges:5 flatpak run org.tunaos.dualcut
```

---

## 3. Common Operational Failure Modes & Resolutions

### Failure Mode 1: GStreamer Pipeline / Render Crash

* **Symptom**: Timeline preview freezes, video frames fail to render, or project export crashes.
* **Root Cause**: Missing GStreamer media plugins (VA-API, H.264 decoders, or GES dependencies).
* **Diagnostic Steps**:
  1. Inspect GStreamer plugin availability in sandbox:
     ```bash
     flatpak run --command=gst-inspect-1.0 org.tunaos.dualcut ges
     ```
  2. Run render pipeline check with debug logging:
     ```bash
     GST_DEBUG=ges:5,videoscale:4 flatpak run org.tunaos.dualcut
     ```
* **Remediation**: Install required GStreamer plugin extensions in Flatpak runtime or fall back to software decoding.

---

## Failure Mode 2: HTTP Agent API Connection Refused / Timeout

* **Symptom**: External scripts or HTTP clients fail to communicate with Dualcut (`Connection refused`).
* **Root Cause**: Embedded API server disabled, port collision on `7357`, or local firewall rules.
* **Diagnostic Steps**:
  1. Check if the port is bound:
     ```bash
     ss -tuln | grep 7357
     ```
  2. Test health endpoint:
     ```bash
     curl -i http://127.0.0.1:7357/status
     ```
* **Remediation**: Reconfigure API server port in preferences or launch with `--api-port=<PORT>`.

---

## Failure Mode 3: Project Document Mtime Hot-Reload Failure

* **Symptom**: Modifying the JSON project file externally does not update the UI timeline live.
* **Root Cause**: File watcher limits reached (`fs.inotify.max_user_watches`) or invalid JSON syntax.
* **Diagnostic Steps**:
  1. Validate JSON syntax:
     ```bash
     python3 -m json.tool project.json > /dev/null
     ```
  2. Check system inotify watch limits:
     ```bash
     cat /proc/sys/fs/inotify/max_user_watches
     ```
* **Remediation**: Increase inotify user watches or repair malformed JSON project document.

---

## 4. Resetting Application State

To reset cached application state, user preferences, and temporary render files:

```bash
# Clear user data and settings cache
rm -rf ~/.var/app/org.tunaos.dualcut/cache/
rm -rf ~/.var/app/org.tunaos.dualcut/config/
```

---

## 5. Maintenance & Escalation Policy

Dualcut is in **maintenance mode**:
- Triage critical crashes, security vulnerabilities, or regression defects.
- Port major features or strategic enhancements to `shrimply`.
