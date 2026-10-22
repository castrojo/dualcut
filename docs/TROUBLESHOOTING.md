# Dualcut Troubleshooting

Common problems and how to fix them. Start with **Diagnostic commands** to
pin down which subsystem is failing — GStreamer (media/codec), the engine
(rendering/proxy), the HTTP API, or the project file itself.

## Diagnostic commands

Run these before anything else. They isolate the failing layer.

```bash
# GStreamer debug trace — the single most useful tool for codec/render problems.
# Level 5 (debug) is the usual starting point; * means every element.
GST_DEBUG=*:5 flatpak run org.tunaos.dualcut 2>&1 | grep -iE 'error|warn|fail|negotiat'

# GES (GStreamer Editing Services) rendering only:
GST_DEBUG=2,ges:4,gespipeline:5 flatpak run org.tunaos.dualcut 2>&1 | tail -80

# GTK/GLib messages:
G_MESSAGES_DEBUG=all flatpak run org.tunaos.dualcut

# Are the encoder/decoder plugins actually installed?
gst-inspect-1.0 | grep -E 'h264|vp8|aac|vorbis|avdec'
gst-inspect-1.0 -k h264parse   # H.264 muxer/parser
gst-inspect-1.0 -k vp8enc       # VP8 encoder

# Is the HTTP API answering? (only while it is running — see below)
curl -s http://127.0.0.1:7357/status

# Flatpak session logs (stdout/stderr flow through the systemd user journal):
journalctl --user -u org.tunaos.dualcut --since "30 min ago" -p err
```

`GST_DEBUG` levels: `0`=none, `1`=error, `2`=warning, `3`=fixme, `4`=info,
`5`=debug, `6`=log, `7`=trace. Pipe through `grep`/`tail` — a full trace is
large.

## GStreamer / codec issues

### "Pipeline negotiation failed" / "no element …"

The pipeline could not build because an element or codec is missing. dualcut
renders with **GStreamer Editing Services (GES)** and exports:

- **MP4** → H.264 video + AAC audio
- **WebM** → VP8 video + Vorbis audio

Missing plugins are the usual cause. In the Flatpak the codec plugins ship
with the runtime; a from-source install needs them on the host:

```bash
# Fedora: full GStreamer + good extras (contains h264/aac/vp8/vorbis)
sudo dnf install gstreamer1-plugins-base gstreamer1-plugins-good \
  gstreamer1-plugins-bad gstreamer1-plugins-ugly gstreamer1-plugins-good-extras \
  gstreamer1-plugins-bad-extras gstreamer1-plugin-h264 gstreamer1-plugin-vp8

# Verify each is now visible:
gst-inspect-1.0 -k h264parse && gst-inspect-1.0 -k aacenc \
  && gst-inspect-1.0 -k vp8enc && gst-inspect-1.0 -k vorbenc
```

If an element is missing that the trace names, find its plugin:
`gst-inspect-1.0 <element_name>` reports `Plugin: <plugin>` — install that
plugin package.

### Hardware acceleration

GES falls back to software encoding if a hardware encoder is unavailable or
the source format is unsupported. If an export is unexpectedly slow, confirm
what is actually running:

```bash
GST_DEBUG=ges:5,gespipeline:5 flatpak run org.tunaos.dualcut 2>&1 | grep -iE 'vaapi|nvenc|v4l2|software|swscale'
```

Most dualcut exports are already GPU-assisted through the render path; a
software fallback usually means a codec the hardware does not support, not a
misconfiguration. Do not force a hardware encoder that the source cannot
decode — that produces the negotiation failures above.

## Performance & responsiveness

### Timeline scrubbing slow with large projects

dualcut transcodes imported videos into lightweight **proxy media** and
scrubs through those, not the originals. If scrubbing is slow the proxies are
either disabled or stale:

- Confirm *Preferences → Use proxy media* is on.
- Proxies live in `.dualcut-cache/` inside the project folder. If missing or
  corrupt, delete the folder and reopen — they regenerate automatically on
  next preview. Safe to delete at any time.

### Preview playback stuttering

Stutter on large/4K footage is usually the preview decode path, not the
export. Proxies are 960px-wide with every frame a keyframe specifically so the
timeline stays smooth; exports still render from the originals at full
quality. If stutter persists:

- Lower the preview resolution in Preferences if available.
- Close other GPU-heavy apps; the preview decode shares the GPU.

### Proxy media cache management

- Location: `<project_dir>/.dualcut-cache/`
- Safe to delete anytime; regenerated on next preview.
- Disable per-project with *Preferences → Use proxy media*.
- Full disk / space pressure? Clear the cache; it is pure regenerated
  scratch.

### High memory usage with 4K footage

The preview uses proxies (small), but the **export renders originals**. A 4K
export of a long timeline holds significant frame data. Mitigations:

- Split very long 4K exports into shorter segments and join later.
- Free RAM / swap before exporting; close other apps.
- If the export is killed for memory, reduce concurrent work (close the
  preview of other projects) and retry.

## HTTP API (agents & scripts)

The API listens on **`127.0.0.1:7357`** (localhost only — never exposed to
the network). Endpoints: `GET /project`, `POST /project`, `POST /render`,
`POST /script`, `GET /status`. It runs inside the GUI (`preview`) and as the
standalone `serve` binary.

### Port 7357 already in use

Another dualcut instance (or a stale one) holds the port.

- GUI: set a different port with the `DUALCUT_API_PORT` environment variable
  before launch; `DUALCUT_API_PORT=0` disables the API entirely.
- `serve` binary: pass a port argument — `dualcut serve project.json 8080`.
- Find and stop the occupant: `ss -ltnp | grep 7357`.

### Connection refused

The server is not running. Start it:

```bash
# From the GUI, the API starts automatically. Or run headless:
cd engine
cargo run --bin serve -- myproject.json      # defaults to 7357
# then:
curl -s http://127.0.0.1:7357/status
```

If the GUI is running but `curl` refuses, confirm you hit `127.0.0.1` (not a
hostname that resolves elsewhere) and that `DUALCUT_API_PORT` was not set to
`0`.

### Script execution timeouts

Scripts run synchronously in the engine — a long script blocks the API until
it returns. Causes and fixes:

- Large projects / many clips: the `edit()` function does real work per
  clip. Profile the script; batch or simplify.
- An infinite loop or blocking call in the script hangs the request. Add
  timeouts in your own code and test on a small project first.
- Confirm the server is up (`GET /status`) before assuming the script is the
  problem; a dead server also looks like a hang.

## File & project issues

### Project won't open (validation errors)

The project is a single JSON document. A structural problem fails validation
on load/apply:

```bash
python3 -c "import json,sys; json.load(open('project.json')); print('valid JSON')"
```

- Valid JSON but still rejected → the structure violates the schema. Check
  against `engine/schema/dualcut.schema.json` (the canonical schema) or
  `engine/schema/dualcut.d.ts` (types).
- The *Code view* tab shows the live document; *Apply JSON* validates, makes
  the change undoable, and hot-reloads — a good place to test edits.

### Media file import failures

- File no longer exists at the recorded path (moved/deleted) — the most
  common cause. Re-import to update the library entry.
- Unsupported format — the source must be decodable by GStreamer. Confirm with
  `gst-inspect-1.0 <format>` / `ffprobe` if available.
- Permissions — the app must be able to read the file. In the Flatpak, grant
  local-files access via the Flatpak permissions tool if the file is outside
  the home sandbox.

### Corrupted proxy cache

Symptoms: stale or flickering preview, scrub jumps, errors referencing
`.dualcut-cache/`. Fix:

```bash
# Close dualcut, then clear the cache for that project:
rm -rf <project_dir>/.dualcut-cache
# Reopen — proxies regenerate on next preview.
```

## Rendering / export

### Export fails mid-render

```bash
GST_DEBUG=2,ges:4,gespipeline:5 flatpak run org.tunaos.dualcut 2>&1 | grep -iE 'error|fail|broken|negotiat' | tail -30
```

- A source that GStreamer cannot decode aborts the pipeline — see
  "Media file import failures" and the codec section above.
- A clip with zero duration or an out-of-range transform often surfaces here;
  validate the project (above) and check clip timings.

### Output file codec incompatibility

The container and its streams must match. dualcut exports:

- **`.mp4`** → H.264 video + AAC audio. If you renamed a WebM export to `.mp4`
  or the player rejects the file, the streams don't match the container.
- **`.webm`** → VP8 video + Vorbis audio.

Re-export with the correct extension and the matching plugins installed (see
"GStreamer / codec issues"). A player that refuses the file almost always means
a mismatched container/codec, not a dualcut bug.

### Out-of-memory during export

Exports render originals at full resolution, so 4K/long timelines are heavy:

- Free memory and swap before exporting; close other apps.
- Split long exports into segments.
- If killed mid-export with OOM, retry after freeing resources. See
  "High memory usage with 4K footage".

## When to file a bug vs. a config issue

**Config / environment (not a bug):**

- Missing codec or "no element" errors → install the GStreamer plugin.
- Port 7357 in use or connection refused → API lifecycle / port config.
- Slow export on 4K → memory/segment, or hardware-fallback behaviour.
- Media won't import → file moved, unsupported, or sandbox permission.
- Corrupt/stale preview → clear `.dualcut-cache/`.

**File a bug when:**

- The same project reproduces on a fresh install with all plugins present.
- A normal project (small, common formats) crashes or errors with no
  obvious cause.
- Capture: dualcut version, install method (Flatpak vs. from source),
  OS/distro, the `GST_DEBUG` trace (piped through `grep` for errors), and the
  exact steps. See [issues](https://github.com/tuna-os/dualcut/issues).

## References

- [GStreamer docs](https://gstreamer.freedesktop.org/documentation/) ·
  [GES](https://gstreamer.freedesktop.org/documentation/ges.html)
- [GTK4 docs](https://docs.gtk.org/gtk4/)
- [Dualcut User Guide](USER_GUIDE.md) ·
  [Observability & diagnostics](observability.md) ·
  [AGENTS.md](../AGENTS.md) · [CONTEXT.md](../CONTEXT.md)
