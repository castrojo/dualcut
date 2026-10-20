# Editing dualcut projects (for agents)

The entire video is one declarative JSON document (`*.json` project
file). Everything below describes that document and the surfaces for
editing it. Domain glossary: CONTEXT.md. Types: engine/schema/.

The Rust engine in `engine/` uses document model v2 — **scenes** (sequential
narrative spine) + **overlays** (tracks crossing scene cuts) + **defs**
(reusable parameterised compositions). Full types: `engine/src/document/mod.rs`.

## HTTP ops (POST /op)

Ops with nontrivial math, so you don't reimplement them:
`{"op": "split", "id": "clip", "at": 5.0}` → splits at absolute time
(media offsets advance, animations divide) and returns `new_id`;
`{"op": "ripple_delete", "id": "clip"}` closes the gap;
`{"op": "detach_audio", "id": "clip"}`;
`{"op": "move_to_lane", "id": "clip", "lane": 2, "at": 3.5}`.
Recipes: docs/recipes/ (auto-captions).

## Editing surfaces

1. **File**: edit the project JSON (e.g. `engine/examples/demo-project.json`).
2. **HTTP** (while `cargo run --bin serve -- <project.json> [port]` runs,
   default port 7357):
   - `GET  /project` — current document
   - `POST /project` — replace document (validated, saved to disk)
   - `POST /render` — `{"out": "path.mp4"}` renders and reports warnings
   - `GET  /status` — engine info
3. **CLI render**: `cargo run --bin render -- <project.json> <out.mp4>`

## Schema summary

```jsonc
{
  "meta": { "title": "…", "width": 1280, "height": 720, "fps": 30 },
  "defs": {
    "lower-third": {
      "params": ["name", "role"],
      "layers": [ /* clips; "{name}" etc. substituted at instantiation */ ]
    }
  },
  "scenes": [               // sequential, no gaps; order = time
    { "id": "s1", "duration": 3, "layers": [ /* clips, start relative to scene */ ] }
  ],
  "overlays": [             // absolute timing, cross scene cuts freely
    { "id": "o1", "clips": [ /* subtitles, music, watermarks */ ] }
  ]
}
```

Clip: `{ id, start, duration, type, …element fields, transform?, animations?, effects? }`
- text extras: `align` (left|center|right — overrides x positioning),
  `outline` (color), `shadow` (bool)
- overlay tracks: `muted` / `hidden` booleans (non-destructive)
- `library`: media paths the user imported (relative to the project)
- effects: `{type: "blur", amount}` (sigma 0-50); `{type: "color",
  brightness?, contrast?, saturation?, hue?}`; `{type: "chromakey",
  color?, angle?, noise?}` (green screen); `{type: "crop", left?,
  right?, top?, bottom?}`; `{type: "mask", shape, feather?, invert?}`
  (freeform shape mask, `video`/`test` clips only — see below); audio:
  `{type: "eq", low?, mid?, high?}` (dB),
  `{type: "compressor", threshold?, ratio?}`, and
  `{type: "denoise", level?}` (0-3)
- scene transition kinds: crossfade | wipe-lr | wipe-tb | box-wipe | iris | clock
- defs may nest (compref inside a def); cycles are rejected at validation
- `type`: `text` (text/font/color) · `video`/`audio` (src/offset/volume) ·
  `image` (src) · `shape` (M3, skipped with warning for now) ·
  `compref` (ref + args) · `test`
- `transform`: `{ x, y, width, height, opacity }` pixels; 0 = natural
- animations, two forms: tween `{ property, from, to, start, end, easing }`
  or keyframes `{ property, keyframes: [{t, value, easing}, …] }` (>= 2,
  strictly increasing t; property is x|y|width|height|opacity|volume|rate;
  volume 1.0 = unity, rate 1.0 = normal speed — animate volume for
  audio fades/ducking, animate rate for speed ramps),
  times relative to the clip; easing: linear|easeIn|easeOut|easeInOut
- `duration: 0` on a scene layer = fill the rest of the scene

Rules: unique ids everywhere; scenes need `duration > 0`; `compref` targets
must exist in `defs`; nested defs must not form a reference cycle. The
*Save as template* action cannot create a def from a selection that already
contains a `compref`, even though hand-authored nested defs are valid.
Audio policy: a video clip's own audio is scene-local; music/VO belongs on
overlays.
"Detach audio" = set the video clip's `volume: 0` and add an `audio` clip
with the same `src`/`offset` wherever you want it.

## Shapes (M3)

`shape` clips render on the GPU (Vello) when the engine is built with the
`vector` feature: rect, circle, ellipse, star, polygon, line, arrow, with
`fill` (#rrggbb/#aarrggbb) and size from `transform.width/height`.
Rasters cache under `<project dir>/.dualcut-cache/`.

## Freeform shape masks (#41)

`{type: "mask", shape, feather?, invert?}` on a `video`/`test` clip
compile-time bakes a real alpha-channel copy of that clip (shape
rasterized via Vello, combined with the source through `alphacombine`,
encoded FFV1/A420 in Matroska, cached under `.dualcut-cache/`) and swaps
it in for the original source. GES's own layer compositing then reveals
whatever's on a lower layer through the transparent region -- true
track-matte, not a solid-color cutout. GES itself can't build the
alphacombine bin as a single-clip effect (multi-source bins are
rejected), so this bakes ahead of time instead, the same "slow but
cached" tradeoff already used for preview proxies. Needs the `vector`
feature; other clip types warn and skip.

## Karaoke-style captions (#53)

`dualcut_engine::karaoke::karaoke_captions_to_clips` turns flat word-level
`(start, end, word)` segments (whisper.cpp `--max-len 1`) into caption
clips where the *whole line* stays visible and the currently-spoken word
is highlighted, instead of one word popping on at a time. GES's
`TitleClip` can't do this directly: `GstBaseTextOverlay`'s `auto-resize`
isn't exposed as a settable child property (an isolated word clip and the
full-sentence clip rescale inconsistently), and `TitleClip`'s `text`
property is always markup-escaped (Pango markup only works through a raw
`textoverlay`'s `pango-markup` sink caps, which GES doesn't expose
either). Instead, each word-highlight state is rasterized once with
Pango/Cairo (full line, one color-attribute span on the active word,
same font/layout box every state so nothing rescales) and landed as an
ordinary `Image` clip -- no GES text primitives involved. Needs the
`karaoke` feature (pulled in automatically by `preview`).

## Scripting (M4)

`POST /script` with a TypeScript body: `export function edit(p: Project): Project`.
Runs in-process, result validated and saved. Types: `engine/schema/dualcut.d.ts`;
JSON Schema: `engine/schema/dualcut.schema.json`.

## Live vector sources (vello://)

With the `vector` feature, the engine registers a `vellosrc` GStreamer
element with a `vello://` URI handler. Any `video` clip can use one as its
`src` for live per-frame GPU vector rendering:

```jsonc
{ "id": "spinner", "type": "video", "start": 0, "duration": 3,
  "src": "vello://star?fill=%23ff5470&w=200&h=200&spin=1" }
```

Shapes: rect|circle|ellipse|star|polygon|line|arrow. Query params: `fill`
(url-encoded hex), `w`/`h` (px), `spin=1` (rotation demo of per-frame
rendering). Static `shape` clips keep using cached PNG rasters (zero
per-frame cost); use vello:// when you need live animation.
