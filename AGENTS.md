# Editing dualcut projects (for agents)

The entire video is one declarative JSON document (`*.json` project
file). Everything below describes that document and the surfaces for
editing it. Domain glossary: CONTEXT.md. Types: engine/schema/.

The Rust engine in `engine/` uses document model v2 — **scenes** (sequential
narrative spine) + **overlays** (tracks crossing scene cuts) + **defs**
(reusable parameterised compositions). Full types: `engine/src/document.rs`.

## HTTP ops (POST /op)

Ops with nontrivial math, so you don't reimplement them:
`{"op": "split", "id": "clip", "at": 5.0}` → splits at absolute time
(media offsets advance, animations divide) and returns `new_id`;
`{"op": "ripple_delete", "id": "clip"}` closes the gap;
`{"op": "detach_audio", "id": "clip"}`;
`{"op": "move_to_lane", "id": "clip", "lane": 2, "at": 3.5}`.
Recipes: docs/recipes/ (auto-captions).

## HTTP API Reference

The HTTP API serves JSON on `127.0.0.1:7357` (override with `DUALCUT_API_PORT`;
`0` disables it). Two binaries expose it — they are **not** the same surface:

| Binary | Server | Holds the project in… | Endpoints |
|---|---|---|---|
| `preview` (the app) | `api::serve_file_api` | the **file on disk** — every request reads/writes it; the app hot-reloads via mtime | `/project`, `/op`, `/script`, `/status` |
| `serve` (`cargo run --bin serve -- <project.json> [port]`) | in-memory | a `Mutex` in memory (the file is written only on `/project`/`/script`) | `/project`, `/render`, `/script`, `/status` |

The `preview` app serves `/op` but **not** `/render`; the `serve` bin serves
`/render` but **not** `/op`.

**Conventions (both servers)**
- Every response is `Content-Type: application/json`.
- Errors use the envelope `{"error":"<message>"}`. The message is the anyhow
  chain (`e.to_string()`), so it carries context — `"loading <path>"`,
  `"'at' (seconds) required"`, `"clip is not a video clip"`.
- Status codes: `200` success · `400` bad request (malformed/invalid body,
  unknown op, missing required param, clip not found, validation failed — the
  file is left untouched) · `404` unknown route (`{"error":"not found"}`) ·
  `500` server error (project file unreadable on a read, or a write/render
  failed) · `501` feature not compiled in (`/script` on a `serve` bin built
  without `scripting`).
- Writes (`/project`, `/op`, `/script`) validate then write; a validation error
  returns `400` and leaves the file unchanged. Each write drops a short-lived
  `.dualcut-cache/agent-edit.json` marker so the app's Edit History can tell an
  agent edit from a manual one.

### `/project`
- `GET /project` → `200` the full project document (see Schema summary).
  `500` if the file can't be read/parsed.
- `POST /project` — body: the full project document. `200` `{"ok":true}` (saved
  to disk; file-backed only). `400` if the body isn't valid/validated project
  JSON (file untouched). `500` if saving fails.

### `/op`  _(file-backed server only)_
Body: `{"op": "<name>", ...}`. The server loads the file, applies the op,
re-validates, and writes it back. `200` returns the op-specific body below; any
failure → `400` `{"error":"..."}` (file left untouched).

| op | request body | success (200) | notes / constraints |
|---|---|---|---|
| `split` | `{op, id, at}` | `{ok:true, new_id:"…"}` | `at` required, seconds (f64); splits at absolute time |
| `ripple_delete` | `{op, id}` | `{ok:true}` | closes the resulting gap |
| `detach_audio` | `{op, id}` | `{ok:true, new_id:"…"}` | `400` if the clip is not a `video` clip |
| `move_to_lane` | `{op, id, lane, at}` | `{ok:true}` | `lane` required (uint, 0-based), `at` required (seconds) |
| `remove_silence` | `{op, id, threshold_db?, min_duration?}` | `{ok:true, removed:[...]}` | **feature: `preview`**; defaults `threshold_db=-40.0` dBFS, `min_duration=0.5` s; clip must have media |

Unknown `op` → `400` `{"error":"unknown op \"…\" (split, ripple_delete, detach_audio, move_to_lane, remove_silence)"}`.

### `/render`  _(serve bin only)_
`POST /render` — body: `{"out"?: "path.mp4", "profile"?: "mp4"|"webm"}`. `out`
defaults to `"out/render.mp4"`; `profile` defaults to the value of `out`.
`200` `{"ok":true, "out":"…", "warnings":[…]}`. `500` `{"error":"…"}` if
rendering fails.

### `/script`  _(requires the `scripting` cargo feature)_
`POST /script` — body: TypeScript source,
`export function edit(p: Project): Project { … }`. Runs in-process, result
validated, then saved (file-backed) or held in memory (`serve`).
`200` `{"ok":true}`. `400` `{"error":"…"}` on a script error. On a `serve` bin
built without the feature → `501` `{"error":"engine built without the scripting feature"}`.

### `/status`
`GET /status` → `200` `{"engine":"dualcut", "version":"…", "project":"<title>", "duration":<s>, "scenes":<n>}`.
`500` if the project file can't be read/parsed.

Keep this reference in sync with `engine/src/api.rs` (file-backed) and
`engine/src/bin/serve.rs` (headless) as operations are added or changed.

## Editing surfaces

1. **File**: edit the project JSON (e.g. `engine/examples/demo-project.json`).
2. **HTTP**: drive the running app or `serve` — see [HTTP API Reference](#http-api-reference) for every endpoint, its request/response schema, status codes and error format.
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
