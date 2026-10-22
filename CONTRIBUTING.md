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
| `engine/` | The Rust engine: document model (`engine/src/document.rs`), rendering (GStreamer Editing Services), the HTTP server, the GTK4/libadwaita UI |
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

## Testing

Two layers, both enforced in CI (`.github/workflows/ci.yml` — the authoritative
list of what blocks a merge). Run them before you push.

### Unit tests — `cargo test`

The unit tests live **inline**, in `#[cfg(test)] mod tests` blocks inside the
`engine/src/*.rs` file for the thing they cover (document model in
`document.rs`, the HTTP API in `api.rs`, mapping in `mapping.rs`, …). There is
no separate `tests/` Rust crate for them — 131 tests across the crate today.

```sh
cd engine && cargo test --all-features
```

Run one in isolation while iterating: `cargo test --all-features document::tests::detach_audio`.

These are the right place for **document-model mutation** and **HTTP API
contract** tests — anything that is pure logic and needs no renderer:

```rust
// engine/src/document.rs — mutate a document, assert the new state
#[test]
fn detach_audio_silences_video_and_adds_overlay_clip() {
    let mut p = demo();
    let new_id = detach_audio(&mut p, "media-ball").expect("detaches");
    match &find_clip(&p, "media-ball").unwrap().element {
        Element::Video { volume, .. } => assert_eq!(*volume, 0.0),
        _ => panic!("still a video clip"),
    }
    let audio = find_clip(&p, &new_id).unwrap();
    assert_eq!(audio.start, 3.0); // scene offset 3.0 + clip start 0
    assert!(matches!(audio.element, Element::Audio { .. }));
    assert!(p.validate().is_ok());
}
```

```rust
// engine/src/api.rs — drive the HTTP surface over a real socket, assert the contract
#[test]
fn op_split_mutates_the_project_and_returns_a_new_id() {
    let path = spawn_server(port);
    let (status, body) = http(port, "POST", "/op", r#"{"op":"split","id":"c1","at":1.0}"#);
    assert_eq!(status, 200);
    assert!(body.contains(r#""ok":true"#) && body.contains("new_id"));
    let on_disk = Project::from_json(&std::fs::read_to_string(&path).unwrap()).unwrap();
    assert_eq!(on_disk.scenes[0].layers.len(), 2, "split should produce two clips");
}
```

### Render smoke tests — `tests/smoke.sh`

`cargo test` can't catch a render regression (a missing encoder, an effect that
does nothing, a keying artifact), so `tests/smoke.sh` renders real projects with
the release binary and inspects the **output**: it decodes frames to PNG and
checks pixel values with Pillow, decodes audio to raw and checks amplitude, and
asserts every export profile produces a nonempty file.

```sh
cd engine && cargo build --release --features vector   # produces target/release/render
../tests/smoke.sh engine/target/release/render          # or: tests/smoke.sh <path-to-render>
```

The render-validation pattern each case follows:

1. write a tiny project JSON (see the heredocs in `tests/smoke.sh`),
2. render it: `"$BIN" in.json out.webm webm`,
3. extract the part you care about (`gst-launch-1.0 … ! pngenc` for a frame,
   `! audioconvert ! 'audio/x-raw,format=F32LE' ! filesink` for samples),
4. assert on the extracted bytes with Python (`PIL.Image` for pixels, `struct`
   for audio).

```python
# chromakey: the keyed pixel must no longer be green — the blue background shows through
r, g, b = px[140, 90]
assert g < 150 or b > 100, f"keyed area still green: {(r,g,b)}"
```

### Coverage

There is no hard numeric coverage target. The bar is *regression memory*: add a
test wherever a bug has already happened or could recur silently — an effect
that renders without doing anything, an op that corrupts the document, an API
route that 500s instead of 400s. Unit tests for pure logic, render smoke tests
for anything that produces pixels or audio. If you changed rendering, extend
`tests/smoke.sh`; if you changed logic, add an inline `#[test]`.

### CI — what blocks a merge

`.github/workflows/ci.yml` runs on every push to `main` and on every PR, on a
bare Fedora 45 container: schema-in-sync check, clippy (`-D warnings`), unit
tests, release render build, the render smoke suite, and a headless app-start
walkthrough. All of them must pass. The `ste.yml` workflow runs the same suite
under the Flatpak/GES build.

### Debugging CI failures locally

Reproduce the same job locally:

```sh
cd engine
cargo clippy --all-features -- -D warnings   # clippy warnings are errors
cargo test --all-features                     # unit tests
cargo build --release --features vector
../tests/smoke.sh engine/target/release/render
```

The render smoke suite is the one that most often differs between "green" and
"red" — if it fails in CI but not locally (or vice versa), it is almost always
a missing GStreamer plugin/encoder in your environment, not a code bug: match the
package list in `ci.yml` (see *Setting up a dev environment*) and re-run. A
smoke test that flakes on timing or GPU-dependent pixel thresholds should be
made deterministic (fixed seed, tolerance, or a non-GPU path) rather than
ignored.

## Code of conduct

Be respectful and constructive. Follow the same norms as the rest of the
TunaOS organization (see the [org-level Code of Conduct](https://github.com/tuna-os/.github/blob/main/CODE_OF_CONDUCT.md)).

## Questions?

Open an issue — the maintainers and the automated hive agents monitor the
tracker. For anything about the document model specifically, `CONTEXT.md`
and `AGENTS.md` are the authoritative references.
