/**
 * dualcut project document — TypeScript declarations.
 * Mirrors engine/src/document/mod.rs (the source of truth).
 *
 * Scripts POSTed to the engine's /script endpoint must export:
 *   export function edit(project: Project): Project
 */

export interface Project {
  /** Media files imported into the project library (paths relative to the project file). */
  library?: string[];
  meta: Meta;
  /** Reusable parameterised compositions, instantiated via CompRefClip. */
  defs?: Record<string, CompDef>;
  /** Sequential narrative spine; order = time, no gaps. */
  scenes: Scene[];
  /** Absolutely-timed tracks that cross scene cuts (subtitles, music…). */
  overlays?: OverlayTrack[];
}

export interface Meta {
  title: string;
  width: number;
  height: number;
  fps: number;
}

export interface CompDef {
  /** Occurrences of `{param}` in string fields substitute at instantiation. */
  params?: string[];
  layers: Clip[];
}

export interface Scene {
  id: string;
  name?: string;
  /** Seconds. */
  duration: number;
  /** Transition from the previous scene into this one; omit = hard cut. */
  transition?: Transition;
  /** Composited top-first (index 0 renders on top). */
  layers?: Clip[];
}

export interface Transition {
  kind?: "crossfade" | "wipe-lr" | "wipe-tb" | "box-wipe" | "iris" | "clock";
  /** Seconds of overlap with the previous scene; must be < scene duration. */
  duration: number;
}

export interface OverlayTrack {
  id: string;
  /** Mute this track's audio (non-destructive). */
  muted?: boolean;
  /** Hide this track's video (non-destructive). */
  hidden?: boolean;
  name?: string;
  clips?: Clip[];
}

export type Clip = ClipBase &
  (
    | TextClip
    | VideoClip
    | AudioClip
    | ImageClip
    | ShapeClip
    | CompRefClip
    | TestClip
  );

export interface ClipBase {
  /** Unique across the whole document. */
  id: string;
  /** Seconds. Scene layers: relative to the scene. Overlays: absolute. */
  start?: number;
  /** Seconds. 0 on a scene layer = fill the rest of the scene. */
  duration?: number;
  transform?: Transform;
  animations?: Anim[];
  /** Video effects applied in order. */
  effects?: Effect[];
}

export interface TextClip {
  type: "text";
  text: string;
  /** Pango font description, e.g. "Sans Bold 32". */
  font?: string;
  /** "#rrggbb" or "#aarrggbb". */
  color?: string;
}

export interface VideoClip {
  type: "video";
  src: string;
  /** Seek into the source file, seconds. */
  offset?: number;
  /** 0..1; 0 mutes (see detach-audio pattern in AGENTS.md). */
  volume?: number;
}

export interface AudioClip {
  type: "audio";
  src: string;
  offset?: number;
  volume?: number;
}

export interface ImageClip {
  type: "image";
  src: string;
}

export interface ShapeClip {
  type: "shape";
  shape: "rect" | "circle" | "ellipse" | "star" | "polygon" | "line" | "arrow";
  fill?: string;
}

export interface CompRefClip {
  type: "compref";
  /** Key into Project.defs. */
  ref: string;
  args?: Record<string, string>;
}

export interface TestClip {
  type: "test";
}

/** Position/size in composition pixels; 0 width/height = natural size. */
export interface Transform {
  x?: number;
  y?: number;
  width?: number;
  height?: number;
  /** 0..1 */
  opacity?: number;
}

export interface Anim {
  property: "x" | "y" | "width" | "height" | "opacity" | "volume" | "rate";
  /** Tween window form (ignored when keyframes is set). */
  from?: number;
  to?: number;
  /** Seconds relative to the clip's own start. */
  start?: number;
  end?: number;
  easing?: "linear" | "easeIn" | "easeOut" | "easeInOut";
  /** Explicit keyframes (>= 2, strictly increasing t). Wins over the tween. */
  keyframes?: Keyframe[];
}

export type Effect =
  | { type: "blur"; /** Blur sigma, 0-50 (~3 is subtle). */ amount: number }
  | {
      type: "chromakey";
      /** Key color (default #00ff00). Pixels near it turn transparent. */
      color?: string;
      /** Hue tolerance in degrees, 1-90 (default 20). */
      angle?: number;
      /** Noise suppression, 0-64. */
      noise?: number;
    }
  | { type: "crop"; left?: number; right?: number; top?: number; bottom?: number }
  | { type: "eq"; /** dB, -24..12 per band */ low?: number; mid?: number; high?: number }
  | {
      type: "denoise";
      /** 0=low, 1=moderate, 2=high, 3=very-high (default 1). */
      level?: number;
    }
  | {
      type: "compressor";
      /** Level where compression starts, 0-1 (default 0.25). */
      threshold?: number;
      /** Ratio 1-4 (default 2). */
      ratio?: number;
    }
  | {
      type: "color";
      /** -1..1, neutral 0 */ brightness?: number;
      /** 0..2, neutral 1 */ contrast?: number;
      /** 0..2, neutral 1 */ saturation?: number;
      /** -1..1, neutral 0 */ hue?: number;
    }
  | {
      type: "mask";
      /** Only `video`/`test` clips; other types warn and skip (#41). */
      shape: "rect" | "circle" | "ellipse" | "star" | "polygon" | "line" | "arrow";
      /** Soft edge, Gaussian sigma in pixels, 0-50. */
      feather?: number;
      /** Show outside the shape instead of inside it. */
      invert?: boolean;
    };

export interface Keyframe {
  /** Seconds relative to the clip. */
  t: number;
  value: number;
  /** Easing from the previous keyframe into this one. */
  easing?: "linear" | "easeIn" | "easeOut" | "easeInOut";
}
