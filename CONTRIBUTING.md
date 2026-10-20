# Contributing to dualcut

> **Deprecation notice:** New development has moved to
> [shrimply](https://github.com/soirihiroka/shrimply). Please port useful
> ideas there instead of starting new feature work in this repository. See
> [DEPRECATED.md](DEPRECATED.md).

Thanks for helping with **dualcut** — the GNOME video editor with dual usage:
edit manually in the timeline/inspector, or programmatically via a live JSON
document, TypeScript scripts, and an HTTP API.

This guide covers how to build, what to read before you start, and what CI
checks your contribution must pass.

<!-- hive-contribute-plea: donated-compute appeal, keep in sync across repos -->
## Contribute compute — no code needed

**[Donate time with your AI CLI](https://hub.tunaos.org/#contribute).**

Your machine runs one task at a time using your signed-in AI coding CLI.
The relay alternates between eligible **School** (OS) and **Reef** (apps)
hives after each task. If one hive has no work, the other can continue.

The guide covers prerequisites, verified `just` setup, credentials, provider
costs, and how to stop with Ctrl+C. Your subscription quota or API charges apply.
Never post account credentials or API keys in issues or pull requests.

## Before you start

Read these — they are short and will save you (and reviewers) time:

- **[CONTEXT.md](CONTEXT.md)** — the domain glossary. The words used across
  the document schema, engine, UI, and docs have exact meanings; when they
  appear in code or issues, they mean precisely what this file says.
- **[AGENTS.md](AGENTS.md)** — how the project document works and the
  surfaces for editing it (file / HTTP / TS scripts). Required reading if
  your change touches the document model, ops, or rendering.
- **[PARITY.md](PARITY.md)** — where dualcut stands against mainstream
  editors, split GUI vs. backend. Check it if your change adds a feature.
- **[ROADMAP.md](ROADMAP.md)** — the native, GPU-accelerated rewrite plan.
  `engine/` is the current implementation.

## Project layout

| Path | What lives here |
|---|---|
| `engine/` | The Rust engine: document model (`engine/src/document/mod.rs`), rendering (GStreamer Editing Services), the HTTP server, the GTK4/libadwaita UI |
| `engine/src/document/` | Document schema and timeline boundaries: `model.rs`, `validation.rs`, `ops.rs`, `rate_ramp.rs`, and `traversal.rs` |
| `engine/schema/` | The canonical document schema: `dualcut.d.ts` (TS types) + `dualcut.schema.json` (JSON Schema) |
| `skills/dualcut/references/` | Bundled copies of the schema used by the agent skill — **must stay byte-identical** to `engine/schema/` (CI enforces this) |
| `docs/` | `USER_GUIDE.md` (user documentation), `guide/` (screenshots), `recipes/`, `adr/` |
| `tests/` | Integration / render smoke tests |
| `scripts/` | Release and build scripts (`release.sh` builds the Flatpak on every `v*` tag) |

## Setting up a dev environment

You need the **canonical dependency list** from
[`engine/build-aux/org.tunaos.dualcut.json`](engine/build-aux/org.tunaos.dualcut.json)
— that Flatpak manifest is the source of truth for GStreamer (+ GES), GTK4,
libadwaita, and friends. A Fedora-style environment works well:

```sh
sudo dnf install -y cargo rust clippy gtk4-devel libadwaita-devel \
  gstreamer1-devel gstreamer1-plugins-base-devel gst-editing-services-devel \
  gstreamer1-plugins-good gstreamer1-plugins-bad-free \
  gstreamer1-plugins-base-tools gstreamer1-plugins-good-extras \
  gstreamer1-plugin-libav mesa-vulkan-drivers python3-pillow \
  xorg-x11-server-Xvfb xset xz
```

CI runs inside a Fedora 45 container. See
[`.github/workflows/ci.yml`](.github/workflows/ci.yml) for the authoritative
package list and validation commands.

## Making a change

1. **Branch from `main`** — use a descriptive name, e.g.
   `git checkout -b fix/crossfade-offset` or `feat/export-webm`.
2. **Keep commits focused** and sign them with DCO:
   `git commit -s` (your commit must carry a `Signed-off-by` trailer).
3. **Run the checks CI will run** before pushing:

   ```sh
   # 1. Schema copies must be byte-identical to the agent skill bundle
   diff engine/schema/dualcut.d.ts skills/dualcut/references/dualcut.d.ts
   diff engine/schema/dualcut.schema.json skills/dualcut/references/dualcut.schema.json

   # 2. Clippy — warnings are errors
   (cd engine && cargo clippy --all-features -- -D warnings)

   # 3. Unit tests
   (cd engine && cargo test --all-features)

   # 4. Release render build (vector feature)
   (cd engine && cargo build --release --features vector)
   ```

4. **Open a PR** describing what changed and why; link any related issue.

### If you change the schema

`engine/schema/dualcut.d.ts` and `engine/schema/dualcut.schema.json` are the
canonical types — update **both**, then copy them into
`skills/dualcut/references/` so the bundled agent skill stays in sync. CI
fails if they drift.

### If you change docs

- User-facing behavior belongs in `docs/USER_GUIDE.md`; screenshots are
  regenerated automatically on every release.
- Recipes (e.g. auto-captions) live in `docs/recipes/`.
- Architectural decisions go in `docs/adr/`.

## Code of conduct

Be respectful and constructive. Follow the same norms as the rest of the
TunaOS organization (see the [org-level Code of Conduct](https://github.com/tuna-os/.github/blob/main/CODE_OF_CONDUCT.md)).

## Questions?

Open an issue — the maintainers and the automated hive agents monitor the
tracker. For anything about the document model specifically, `CONTEXT.md`
and `AGENTS.md` are the authoritative references.
