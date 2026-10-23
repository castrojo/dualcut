# dualcut

> **Deprecated:** Development has moved to [shrimply](https://github.com/soirihiroka/shrimply), the recommended project for new work. We keep this repository for its history and its releases. Port useful ideas to shrimply; do not start new feature work in dualcut.

A GNOME video editor with **dual usage**: edit by hand or by program. Every surface stays in sync while the app runs.

> Concept and spec by **[KiKaraage](https://github.com/KiKaraage)** (main author).
> Implementation built with Claude Code.

![dualcut editor](docs/screenshot.png)

**[📘 User Guide](docs/USER_GUIDE.md)** — screenshots regenerated automatically on every release.

**Install (Flatpak, via the TunaOS remote):**

```sh
flatpak remote-add --if-not-exists tuna-os https://tunaos.org/flatpak/tuna-os.flatpakrepo
flatpak install tuna-os org.tunaos.dualcut

flatpak run org.tunaos.dualcut ~/Videos/myproject.json
```

Updates land automatically (`flatpak update`) whenever a new `v*` tag ships.

<details>
<summary>Install one release bundle without a remote</summary>

```sh
# x86_64
curl -LO https://github.com/tuna-os/dualcut/releases/latest/download/dualcut.flatpak && flatpak install --user --reinstall -y dualcut.flatpak
# aarch64
curl -LO https://github.com/tuna-os/dualcut/releases/latest/download/dualcut-aarch64.flatpak && flatpak install --user --reinstall -y dualcut-aarch64.flatpak
```

</details>

<!-- hive-contribute-plea: donated-compute appeal, keep in sync across repos -->
## Contribute compute — no code needed

**[Donate time with your AI CLI](https://hub.tunaos.org/#contribute).**

Your machine runs one task at a time using your signed-in AI coding CLI.
The relay alternates between eligible **School** (OS) and **Reef** (apps)
hives after each task. If one hive has no work, the other can continue.

The guide covers prerequisites, verified `just` setup, credentials, provider
costs, and how to stop with Ctrl+C. Your subscription quota or API charges apply.
Never post account credentials or API keys in issues or pull requests.

## How it works

One JSON **project document** is the single source of truth. It holds
**scenes** (sequential narrative spine), **overlays** (tracks that cross
scene cuts), and **defs** (reusable parameterised templates). The engine
edits with GStreamer (GES); shapes draw on the GPU via Vello; the UI is
GTK4/libadwaita.

| Surface | What | Sync |
|---|---|---|
| App | scene strip w/ thumbnails, inspector, script tab, preview | writes the document, hot-reloads external edits |
| File | edit the project JSON in any editor/agent | app reloads live (mtime watch) |
| HTTP | `GET/POST /project`, `POST /script` (TypeScript), `/status` on `127.0.0.1:7357` | file-backed, works against the running app |

Agents: point your tool at **[skills/dualcut/SKILL.md](skills/dualcut/SKILL.md)**
(ships in-repo, versioned with every commit). Deep reference:
[AGENTS.md](AGENTS.md) · types [engine/schema/dualcut.d.ts](engine/schema/dualcut.d.ts)
· [JSON Schema](engine/schema/dualcut.schema.json).

## From source

```sh
cd engine
cargo run --features preview,vector,scripting --bin preview -- examples/demo-project.json
cargo run --bin render -- new myproject.json "My video"   # scaffold
cargo run --bin render -- myproject.json out.mp4          # or out.webm
cargo run --bin serve  -- myproject.json                  # headless agent API
```

Needs GStreamer (+GES), GTK4, libadwaita dev packages; see
[engine/build-aux/org.tunaos.dualcut.json](engine/build-aux/org.tunaos.dualcut.json)
for the canonical dependency list.

## Features

- Edit scenes with crossfade transitions and overlay tracks
- Text, video, audio, image clips + GPU vector shapes (rect, circle,
  ellipse, star, polygon, line, arrow)
- Tweened animations (x/y/opacity, easing) compiled to GStreamer control sources
- Reusable parameterised templates (lower third, title card, caption built in)
- Detach audio, undo/redo, multi-select, first-frame thumbnails
- Write TypeScript scripts in-app and over HTTP (`export function edit(p) {…}`)
- MP4 (H.264/AAC) and WebM (VP8/Vorbis) export
- Releases: every `v*` tag auto-builds the Flatpak (`scripts/release.sh`)

Roadmap and status: [ROADMAP.md](ROADMAP.md) ·
open work: [issues](https://github.com/tuna-os/dualcut/issues)

## License

GPL-3.0-only — see [LICENSE](LICENSE).
