# dualcut domain glossary

The vocabulary used across the document schema, engine code, UI, and docs.
When these words appear in code or issues, they mean exactly this.

**Project (document)** — the single JSON file that fully describes a video.
The only source of truth; every surface (app, agents, scripts) edits it.
Types: `engine/src/document/mod.rs` (Rust), `engine/schema/dualcut.d.ts` (TS),
`engine/schema/dualcut.schema.json` (JSON Schema).

**Scene** — one beat of the narrative spine. Scenes are strictly
sequential; order defines time, there are no gaps. A scene has a
`duration` and `layers`. Scene-layer clip `start` times are relative to
the scene, so reordering scenes never breaks their contents.

**Transition** — an overlap between a scene and its predecessor
(`crossfade` today). Overlapping shortens the total timeline; offsets are
computed by `Project::scene_offset`.

**Overlay track** — a composition-level track whose clips use absolute
timing and freely cross scene cuts: subtitles, music, watermarks,
detached audio. The answer to "what doesn't fit the scene model".

**Def / template** — a reusable, parameterised set of clips stored in
`defs`, instantiated by a `compref` clip with `args`. `{param}`
placeholders substitute in string fields. Defs may nest, but reference
cycles are rejected. The editor's *Save as template* action cannot create a
def from a selection that already contains a template instance.
Built-ins ship in `engine/templates/starter.json`.

**Clip** — one timed element on a lane: `text`, `video`, `audio`,
`image`, `shape`, `compref`, or `test`. `duration: 0` on a scene layer
means "fill the rest of the scene". Every clip id is unique
document-wide.

**Transform** — pixel-space placement (`x`, `y`, `width`, `height`) plus
`opacity`. Zero width/height = natural/full-frame size.

**Animation** — per-property motion, two forms: a *tween window*
(`from`→`to` over `start`..`end` with easing) or a *keyframe list*
(`keyframes: [{t, value, easing}]`). Multiple animations per property
merge into one GStreamer control source (see `mapping.rs`).

**Lane** — a horizontal row in the timeline UI: scene layer slots first
(top-down by index), then overlay tracks. `document::lane_count` and
`move_clip_to_lane` define the mapping.

**Detach audio** — the editor op that mutes a video clip (`volume: 0`)
and adds an independent `audio` clip with the same source to the
`detached-audio` overlay track.

**Compile / mapping** — turning a document into a GES timeline
(`engine/src/mapping.rs`). Never edited directly; always regenerated.

**Agent surface** — the ways non-humans edit: the project file on disk
(mtime-watched, hot-reloaded), the HTTP API (port 7357: `/project`,
`/script`, `/render`, `/status`), and TypeScript scripts
(`export function edit(p: Project): Project`). Skill:
`skills/dualcut/SKILL.md`.

**vello:// source** — a live GPU vector source; any `video` clip may use
`vello://<shape>?fill=…&w=…&h=…` as its `src`. Implemented by the
`vellosrc` GStreamer element (`engine/src/vellosrc.rs`).
