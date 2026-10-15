//! Document model v2 (see ROADMAP.md): scenes are the sequential narrative
//! spine, overlays span scene cuts, defs are reusable parameterised
//! compositions. This document — not GES — is what the UI, in-app scripts,
//! and external agents edit; `crate::mapping` compiles it into a GES
//! timeline.

use anyhow::{bail, Result};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

mod traversal;

pub use traversal::{effective_duration, find_clip, find_clip_mut, remove_clip};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Project {
    pub meta: Meta,
    /// Media files the user imported into the project's library. Paths
    /// are relative to the project file (or absolute).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub library: Vec<String>,
    /// Reusable compositions, instantiated by `Layer::CompRef`.
    #[serde(default)]
    pub defs: BTreeMap<String, CompDef>,
    /// Sequential scenes; order defines time. No gaps.
    pub scenes: Vec<Scene>,
    /// Composition-spanning tracks (subtitles, music, watermarks) with
    /// absolute timing; they freely cross scene cuts.
    #[serde(default)]
    pub overlays: Vec<OverlayTrack>,
    /// Per-layer-index lock/hide/mute for scene layers (#21). Scene
    /// layers are positional (a vec index, not an identity), so this is
    /// indexed the same way: `scene_lanes[i]` applies to layer `i` of
    /// every scene. Missing/short entries default to unlocked/visible.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub scene_lanes: Vec<LaneMeta>,
}

/// Lock/hide/mute state for one timeline lane (#21). Hide/mute are
/// applied non-destructively at compile time (opacity/volume to 0);
/// lock only affects the GUI (drag is disabled).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct LaneMeta {
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub locked: bool,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub muted: bool,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub hidden: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Meta {
    pub title: String,
    pub width: i32,
    pub height: i32,
    pub fps: i32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CompDef {
    /// Parameter names; occurrences of `{name}` in the def's string fields
    /// are substituted at instantiation.
    #[serde(default)]
    pub params: Vec<String>,
    pub layers: Vec<Clip>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Scene {
    pub id: String,
    #[serde(default)]
    pub name: String,
    /// Seconds.
    pub duration: f64,
    /// Transition from the previous scene into this one (ignored on the
    /// first scene). None = hard cut.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub transition: Option<Transition>,
    /// Layers composited top-first (index 0 renders on top).
    #[serde(default)]
    pub layers: Vec<Clip>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Transition {
    #[serde(default)]
    pub kind: TransitionKind,
    /// Seconds of overlap with the previous scene.
    pub duration: f64,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum TransitionKind {
    #[default]
    Crossfade,
    #[serde(rename = "wipe-lr")]
    WipeLr,
    #[serde(rename = "wipe-tb")]
    WipeTb,
    #[serde(rename = "box-wipe")]
    BoxWipe,
    Iris,
    Clock,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OverlayTrack {
    pub id: String,
    /// Mute the track's audio without touching its clips (#31).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub muted: bool,
    /// Hide the track's video without touching its clips (#31).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub hidden: bool,
    /// Disable dragging clips on this track in the GUI (#21).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub locked: bool,
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub clips: Vec<Clip>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Clip {
    pub id: String,
    /// Seconds. Relative to the scene for scene layers; absolute for
    /// overlay clips and def layers (relative to instantiation start).
    #[serde(default)]
    pub start: f64,
    /// Seconds. Defaults to the remainder of the scene/def when 0.
    #[serde(default)]
    pub duration: f64,
    #[serde(flatten)]
    pub element: Element,
    #[serde(default)]
    pub transform: Transform,
    #[serde(default)]
    pub animations: Vec<Anim>,
    /// Video effects applied in order.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub effects: Vec<Effect>,
}

/// A video effect on one clip.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum Effect {
    /// Gaussian blur; `amount` is the blur sigma (0-50, ~3 is subtle).
    Blur { amount: f64 },
    /// Green-screen keying (#33): pixels near `color` turn transparent.
    ChromaKey {
        #[serde(default = "default_key_color")]
        color: String,
        /// Hue tolerance in degrees (1-90).
        #[serde(default = "default_key_angle")]
        angle: f64,
        /// Noise suppression level (0-64).
        #[serde(default)]
        noise: f64,
    },
    /// Crop pixels from the clip's edges (#33).
    Crop {
        #[serde(default)]
        left: i32,
        #[serde(default)]
        right: i32,
        #[serde(default)]
        top: i32,
        #[serde(default)]
        bottom: i32,
    },
    /// Three-band EQ in dB (-24..12) (#36).
    Eq {
        #[serde(default)]
        low: f64,
        #[serde(default)]
        mid: f64,
        #[serde(default)]
        high: f64,
    },
    /// Dynamic range compressor (#36).
    Compressor {
        /// Level above which compression starts (0-1).
        #[serde(default = "default_comp_threshold")]
        threshold: f64,
        /// Compression ratio (1-4 for audiodynamic).
        #[serde(default = "default_comp_ratio")]
        ratio: f64,
    },
    /// Noise suppression (WebRTC audio processing) (#36).
    Denoise {
        /// 0=low, 1=moderate, 2=high, 3=very-high.
        #[serde(default = "default_denoise_level")]
        level: u32,
    },
    /// Color balance. Neutral is brightness 0, contrast 1, saturation 1, hue 0.
    Color {
        #[serde(default)]
        brightness: f64,
        #[serde(default = "default_one")]
        contrast: f64,
        #[serde(default = "default_one")]
        saturation: f64,
        #[serde(default)]
        hue: f64,
    },
    /// Freeform shape mask: the clip is only visible inside `shape`
    /// (rendered full-frame, same shape catalog as `shape` clips). Needs
    /// the `vector` feature; skipped with a warning otherwise (#41).
    Mask {
        shape: ShapeKind,
        /// Gaussian blur sigma applied to the mask edge (0-50) for a
        /// soft/feathered cutoff instead of a hard edge.
        #[serde(default)]
        feather: f64,
        /// Show outside the shape instead of inside it.
        #[serde(default)]
        invert: bool,
    },
}

fn default_one() -> f64 {
    1.0
}
fn default_key_color() -> String {
    "#00ff00".into()
}
fn default_key_angle() -> f64 {
    20.0
}
fn default_comp_threshold() -> f64 {
    0.25
}
fn default_comp_ratio() -> f64 {
    2.0
}
fn default_denoise_level() -> u32 {
    1
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum Element {
    Text {
        text: String,
        #[serde(default = "default_font")]
        font: String,
        #[serde(default = "default_color")]
        color: String,
        /// Horizontal alignment; overrides transform.x positioning.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        align: Option<TextAlign>,
        /// Outline color (#rrggbb / #aarrggbb).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        outline: Option<String>,
        /// Draw a drop shadow behind the text.
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        shadow: bool,
    },
    Video {
        src: String,
        /// Seek into the source, seconds.
        #[serde(default)]
        offset: f64,
        #[serde(default = "default_volume")]
        volume: f64,
        /// Playback speed multiplier (0.1–10; 1.0 = normal). Duration
        /// stays in timeline seconds; media consumed = duration x rate.
        #[serde(default = "default_rate", skip_serializing_if = "is_default_rate")]
        rate: f64,
    },
    Audio {
        src: String,
        #[serde(default)]
        offset: f64,
        #[serde(default = "default_volume")]
        volume: f64,
        /// Playback speed multiplier (0.1–10; 1.0 = normal).
        #[serde(default = "default_rate", skip_serializing_if = "is_default_rate")]
        rate: f64,
    },
    Image {
        src: String,
    },
    /// GPU vector shapes; rendered by the Vello compositor from M3.
    /// The M1 GES mapping skips them with a warning.
    Shape {
        shape: ShapeKind,
        #[serde(default = "default_color")]
        fill: String,
    },
    /// Instantiate a reusable composition from `defs`.
    CompRef {
        r#ref: String,
        #[serde(default)]
        args: BTreeMap<String, String>,
    },
    /// Built-in test pattern (useful for scripts/tests).
    Test {},
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ShapeKind {
    Rect,
    Circle,
    Ellipse,
    Star,
    Polygon,
    Line,
    Arrow,
}

/// Position/size in composition pixels; opacity 0..1.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Transform {
    #[serde(default)]
    pub x: f64,
    #[serde(default)]
    pub y: f64,
    /// 0 = natural / full size.
    #[serde(default)]
    pub width: f64,
    #[serde(default)]
    pub height: f64,
    #[serde(default = "default_opacity")]
    pub opacity: f64,
}

impl Default for Transform {
    fn default() -> Self {
        Self { x: 0.0, y: 0.0, width: 0.0, height: 0.0, opacity: 1.0 }
    }
}

/// Animation for one property: either a tween window (`from`/`to` over
/// `start`..`end`) or an explicit `keyframes` list. Keyframes win when
/// both are present.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Anim {
    pub property: AnimProperty,
    #[serde(default)]
    pub from: f64,
    #[serde(default)]
    pub to: f64,
    /// Seconds relative to the clip's own start.
    #[serde(default)]
    pub start: f64,
    #[serde(default)]
    pub end: f64,
    #[serde(default)]
    pub easing: Easing,
    /// Explicit keyframes (seconds relative to the clip). When set, the
    /// tween fields above are ignored.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub keyframes: Vec<Keyframe>,
}

/// A single keyframe: property value at time `t`, eased from the previous
/// keyframe with `easing`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Keyframe {
    pub t: f64,
    pub value: f64,
    #[serde(default)]
    pub easing: Easing,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum TextAlign {
    Left,
    Center,
    Right,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum AnimProperty {
    X,
    Y,
    Width,
    Height,
    Opacity,
    /// Audio volume (0.0 silent, 1.0 unity). Audio/video clips only.
    Volume,
    /// Playback speed multiplier. NOT YET SUPPORTED as an animation --
    /// GES's rate-property auto-registration is unsafe to drive from a
    /// live control binding (see mapping.rs); use a constant
    /// clip.rate, or split the clip into segments with different
    /// rates. Kept as a variant for forward-compat / clear errors.
    Rate,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum Easing {
    #[default]
    Linear,
    EaseIn,
    EaseOut,
    EaseInOut,
}

/// Apply an easing curve to a 0..1 progress fraction.
pub fn ease(easing: Easing, p: f64) -> f64 {
    match easing {
        Easing::Linear => p,
        Easing::EaseIn => p * p * p,
        Easing::EaseOut => 1.0 - (1.0 - p).powi(3),
        Easing::EaseInOut => {
            if p < 0.5 { 4.0 * p * p * p } else { 1.0 - (-2.0 * p + 2.0).powi(3) / 2.0 }
        }
    }
}

impl Project {
    pub fn from_json(json: &str) -> Result<Self> {
        let project: Project = serde_json::from_str(json)?;
        project.validate()?;
        Ok(project)
    }

    pub fn to_json(&self) -> String {
        serde_json::to_string_pretty(self).expect("project serializes")
    }

    /// Total duration in seconds. Scenes are sequential; a transition
    /// overlaps a scene with its predecessor, shortening the total.
    pub fn duration(&self) -> f64 {
        match self.scenes.len() {
            0 => 0.0,
            n => self.scene_offset(n - 1) + self.scenes[n - 1].duration,
        }
    }

    /// Absolute start time of a scene by index (transition overlaps pull
    /// scenes earlier).
    pub fn scene_offset(&self, index: usize) -> f64 {
        let mut offset = 0.0;
        for i in 0..index {
            offset += self.scenes[i].duration;
            offset -= self.scenes[i + 1].transition.as_ref().map_or(0.0, |t| t.duration);
        }
        offset.max(0.0)
    }

    pub fn validate(&self) -> Result<()> {
        if self.meta.width <= 0 || self.meta.height <= 0 || self.meta.fps <= 0 {
            bail!("meta.width/height/fps must be positive");
        }
        let mut ids = std::collections::HashSet::new();
        let mut check = |id: &str, what: &str| -> Result<()> {
            if id.is_empty() {
                bail!("every {what} needs a non-empty id");
            }
            if !ids.insert(id.to_string()) {
                bail!("duplicate id {id:?}");
            }
            Ok(())
        };
        for scene in &self.scenes {
            check(&scene.id, "scene")?;
            if scene.duration <= 0.0 {
                bail!("scene {:?}: duration must be > 0", scene.id);
            }
            if let Some(t) = &scene.transition
                && (t.duration <= 0.0 || t.duration >= scene.duration) {
                    bail!("scene {:?}: transition duration must be > 0 and < scene duration", scene.id);
                }
            for clip in &scene.layers {
                check(&clip.id, "clip")?;
                clip.validate(&self.defs)?;
            }
        }
        for track in &self.overlays {
            check(&track.id, "overlay track")?;
            for clip in &track.clips {
                check(&clip.id, "clip")?;
                clip.validate(&self.defs)?;
            }
        }
        for def in self.defs.values() {
            for clip in &def.layers {
                clip.validate(&self.defs)?;
            }
        }
        // Defs may nest, but reference cycles would expand forever.
        for start in self.defs.keys() {
            let mut stack = vec![(start.clone(), vec![start.clone()])];
            while let Some((name, path)) = stack.pop() {
                let Some(def) = self.defs.get(&name) else { continue };
                for clip in &def.layers {
                    if let Element::CompRef { r#ref, .. } = &clip.element {
                        if path.contains(r#ref) {
                            bail!("def cycle: {} -> {:?}", path.join(" -> "), r#ref);
                        }
                        let mut next = path.clone();
                        next.push(r#ref.clone());
                        stack.push((r#ref.clone(), next));
                    }
                }
            }
        }
        Ok(())
    }
}

impl Clip {
    fn validate(&self, defs: &BTreeMap<String, CompDef>) -> Result<()> {
        if self.start < 0.0 || self.duration < 0.0 {
            bail!("clip {:?}: start/duration must be >= 0", self.id);
        }
        for anim in &self.animations {
            if anim.keyframes.is_empty() {
                if anim.end <= anim.start {
                    bail!("clip {:?}: animation end must be > start", self.id);
                }
            } else {
                if anim.keyframes.len() < 2 {
                    bail!("clip {:?}: keyframe animations need at least 2 keyframes", self.id);
                }
                if anim.keyframes.windows(2).any(|w| w[1].t <= w[0].t) {
                    bail!("clip {:?}: keyframes must be in strictly increasing time order", self.id);
                }
            }
        }
        if let Element::Video { rate, .. } | Element::Audio { rate, .. } = &self.element
            && !(0.1..=10.0).contains(rate) {
                bail!("clip {:?}: rate must be 0.1-10", self.id);
            }
        for anim in self.animations.iter().filter(|a| a.property == AnimProperty::Rate) {
            // A plain tween (from/to) has no natural segment boundaries
            // for expand_rate_ramp (#40) to sample from -- only keyframed
            // rate curves are supported; tween-style throws here as
            // before, with a clearer pointer at the actual gap.
            if anim.keyframes.is_empty() {
                bail!(
                    "clip {:?}: rate animation needs keyframes (a plain tween has no segment \
                     boundaries to expand into static-rate clips); use a constant rate or \
                     keyframes",
                    self.id
                );
            }
            if anim.keyframes.iter().any(|k| !(0.1..=10.0).contains(&k.value)) {
                bail!("clip {:?}: rate keyframe values must be 0.1-10", self.id);
            }
        }
        for effect in &self.effects {
            match effect {
                Effect::Blur { amount } => {
                    if !(0.0..=50.0).contains(amount) {
                        bail!("clip {:?}: blur amount must be 0-50", self.id);
                    }
                }
                Effect::ChromaKey { angle, noise, .. } => {
                    if !(1.0..=90.0).contains(angle) || !(0.0..=64.0).contains(noise) {
                        bail!("clip {:?}: chromakey angle 1-90, noise 0-64", self.id);
                    }
                }
                Effect::Crop { left, right, top, bottom } => {
                    if [left, right, top, bottom].iter().any(|v| **v < 0) {
                        bail!("clip {:?}: crop values must be >= 0", self.id);
                    }
                }
                Effect::Eq { low, mid, high } => {
                    if [low, mid, high].iter().any(|v| !(-24.0..=12.0).contains(*v)) {
                        bail!("clip {:?}: eq bands are -24..12 dB", self.id);
                    }
                }
                Effect::Compressor { threshold, ratio } => {
                    if !(0.0..=1.0).contains(threshold) || !(1.0..=4.0).contains(ratio) {
                        bail!("clip {:?}: compressor threshold 0-1, ratio 1-4", self.id);
                    }
                }
                Effect::Denoise { level } => {
                    if *level > 3 {
                        bail!("clip {:?}: denoise level must be 0-3", self.id);
                    }
                }
                Effect::Color { brightness, contrast, saturation, hue } => {
                    if !(-1.0..=1.0).contains(brightness)
                        || !(0.0..=2.0).contains(contrast)
                        || !(0.0..=2.0).contains(saturation)
                        || !(-1.0..=1.0).contains(hue)
                    {
                        bail!(
                            "clip {:?}: color ranges are brightness/hue -1..1, contrast/saturation 0..2",
                            self.id
                        );
                    }
                }
                Effect::Mask { feather, .. } => {
                    if !(0.0..=50.0).contains(feather) {
                        bail!("clip {:?}: mask feather must be 0-50", self.id);
                    }
                }
            }
        }
        if let Element::CompRef { r#ref, .. } = &self.element
            && !defs.contains_key(r#ref) {
                bail!("clip {:?} references unknown def {ref:?}", self.id);
            }
        Ok(())
    }
}

fn default_font() -> String {
    "Sans Bold 32".into()
}
fn default_color() -> String {
    "#ffffff".into()
}
fn default_rate() -> f64 {
    1.0
}
fn is_default_rate(r: &f64) -> bool {
    (*r - 1.0).abs() < f64::EPSILON
}
fn default_volume() -> f64 {
    1.0
}
fn default_opacity() -> f64 {
    1.0
}

/// Parse "#rrggbb" / "#aarrggbb" into ARGB u32 (as GES title colors expect).
pub fn parse_color(color: &str) -> u32 {
    let hex = color.trim_start_matches('#');
    let value = u32::from_str_radix(hex, 16).unwrap_or(0x00ff_ffff);
    if hex.len() <= 6 {
        0xff00_0000 | value
    } else {
        value
    }
}

/// Grow a scene's duration so a scene-layer clip (by id) fits, if it
/// doesn't already (#21: "make scene duration flexible" — dragging or
/// trimming a clip past the current scene end expands the scene rather
/// than clamping the clip). No-op for overlay clips (already
/// unbounded) or if the clip already fits.
pub fn grow_scene_for_clip(project: &mut Project, id: &str) {
    for scene in &mut project.scenes {
        let Some(clip) = scene.layers.iter().find(|c| c.id == id) else { continue };
        let needed = clip.start + if clip.duration > 0.0 { clip.duration } else { 0.1 };
        if needed > scene.duration {
            scene.duration = needed;
        }
        return;
    }
}

/// Ripple delete (#34): remove the clip and close the gap it leaves.
/// Overlay clips shift everything after them in the track left; scene
/// clips shrink the scene toward the remaining content.
pub fn ripple_delete(project: &mut Project, id: &str) -> Result<()> {
    for track in &mut project.overlays {
        if let Some(pos) = track.clips.iter().position(|c| c.id == id) {
            let removed = track.clips.remove(pos);
            let span = removed.duration.max(0.0);
            for clip in &mut track.clips {
                if clip.start >= removed.start {
                    clip.start = (clip.start - span).max(0.0);
                }
            }
            return Ok(());
        }
    }
    for si in 0..project.scenes.len() {
        let scene = &mut project.scenes[si];
        if let Some(pos) = scene.layers.iter().position(|c| c.id == id) {
            let removed = scene.layers.remove(pos);
            let span = if removed.duration > 0.0 {
                removed.duration
            } else {
                (scene.duration - removed.start).max(0.0)
            };
            // Shrink the scene, but never below the remaining content.
            let content_end = scene
                .layers
                .iter()
                .map(|c| c.start + if c.duration > 0.0 { c.duration } else { 0.1 })
                .fold(0.1f64, f64::max);
            scene.duration = (scene.duration - span).max(content_end);
            return Ok(());
        }
    }
    bail!("no clip {id:?}")
}

/// Splice out the stretches of `id`'s own media that fall within
/// `silent_ranges` (media-relative seconds, e.g. from
/// `crate::silence::detect_silence`), closing the gap left behind (#46).
/// Only clips at normal playback rate (1.0) are supported, matching
/// `split_clip`'s timeline-second contract. Returns how many ranges were
/// actually removed (a range entirely outside the clip's used media
/// window is silently skipped).
///
/// Overlay clips ripple the whole track (via `ripple_delete`, already
/// correct there); scene layers are independent lanes stacked in time
/// (`ripple_delete` deliberately leaves siblings alone for those), so this
/// shifts only the edited clip's own later pieces and shrinks the scene to
/// fit -- other layers in the same scene keep their timing.
pub fn remove_silence(project: &mut Project, id: &str, silent_ranges: &[(f64, f64)]) -> Result<usize> {
    let mut container_offset = 0.0;
    let mut found = false;
    for (i, scene) in project.scenes.iter().enumerate() {
        if scene.layers.iter().any(|c| c.id == id) {
            container_offset = project.scene_offset(i);
            found = true;
            break;
        }
    }
    if !found && !project.overlays.iter().any(|t| t.clips.iter().any(|c| c.id == id)) {
        bail!("no clip {id:?}");
    }

    let clip = find_clip(project, id).expect("checked above");
    let (offset, rate) = match &clip.element {
        Element::Video { offset, rate, .. } | Element::Audio { offset, rate, .. } => (*offset, *rate),
        _ => bail!("clip {id:?} has no media offset (not video/audio)"),
    };
    if (rate - 1.0).abs() > 1e-6 {
        bail!("remove_silence only supports rate 1.0 clips (clip {id:?} has rate {rate})");
    }
    let media_span = if clip.duration > 0.0 { clip.duration } else { f64::MAX };
    let clip_start_abs = container_offset + clip.start;

    // Media-relative ranges -> timeline-absolute, clipped to the clip's
    // actual used media window, latest-first so earlier split points stay
    // valid as later cuts ripple everything after them backward.
    let mut abs_ranges: Vec<(f64, f64)> = silent_ranges
        .iter()
        .filter_map(|&(ms, me)| {
            let s = ms.max(offset);
            let e = me.min(offset + media_span);
            (e > s).then_some((clip_start_abs + (s - offset), clip_start_abs + (e - offset)))
        })
        .collect();
    abs_ranges.sort_by(|a, b| a.0.total_cmp(&b.0));

    // Individual ranges can legitimately fail to split (e.g. silence
    // sitting exactly at the clip's edge, which split_clip rejects as "not
    // inside the clip") -- skip those rather than aborting a batch that's
    // otherwise fine.
    let mut removed = 0;
    for (start, end) in abs_ranges.into_iter().rev() {
        let Ok(tail_id) = split_clip(project, id, start) else { continue };
        let Ok(after_id) = split_clip(project, &tail_id, end) else { continue };
        let span = end - start;
        let is_overlay = project.overlays.iter().any(|t| t.clips.iter().any(|c| c.id == tail_id));
        if is_overlay {
            // Overlay tracks are one shared sequential timeline, so
            // ripple_delete already shifts every later clip on the track
            // correctly.
            if ripple_delete(project, &tail_id).is_ok() {
                removed += 1;
            }
            continue;
        }
        // Scene layers are independent lanes stacked in time (captions
        // shouldn't jump just because the video layer lost a silent
        // stretch) -- ripple_delete deliberately leaves siblings alone
        // there, so close this clip's own gap by hand: shift only its own
        // continuation (after_id) back, then shrink the scene to fit.
        for scene in &mut project.scenes {
            let Some(pos) = scene.layers.iter().position(|c| c.id == tail_id) else { continue };
            scene.layers.remove(pos);
            if let Some(after) = scene.layers.iter_mut().find(|c| c.id == after_id) {
                after.start = (after.start - span).max(0.0);
            }
            scene.duration = scene
                .layers
                .iter()
                .map(|c| c.start + if c.duration > 0.0 { c.duration } else { 0.1 })
                .fold(0.1f64, f64::max);
            break;
        }
        removed += 1;
    }
    Ok(removed)
}

/// Expand a clip with a keyframed rate animation into N sub-clips, each
/// with its own *static* rate sampled at that segment's midpoint (#40).
/// GES auto-registers pitch/videorate rate properties at the class level
/// and synchronously recomputes the clip's timeline-to-media mapping
/// whenever one changes via edit APIs that assert the calling thread owns
/// the timeline -- but a live GstController binding (the same mechanism
/// used for opacity/position animation) fires from the streaming thread,
/// not the app thread, so a keyframed *live* rate binding reliably hits
/// that threading assertion. This sidesteps it entirely: the document
/// keeps one clip with a rate curve, and mapping::compile() calls this to
/// turn it into ordinary static-rate clips before GES ever sees a rate
/// animation. Segment boundaries are the keyframes themselves; each
/// segment's media offset advances by the *previous* segments' actual
/// media consumption (duration x sampled rate), not just clip-time
/// duration. Returns `None` if `clip` has no keyframed Rate animation.
pub fn expand_rate_ramp(clip: &Clip) -> Option<Vec<Clip>> {
    let anim = clip.animations.iter().find(|a| a.property == AnimProperty::Rate)?;
    if anim.keyframes.len() < 2 {
        return None;
    }
    let base_offset = match &clip.element {
        Element::Video { offset, .. } | Element::Audio { offset, .. } => *offset,
        _ => return None,
    };
    let other_anims: Vec<Anim> =
        clip.animations.iter().filter(|a| a.property != AnimProperty::Rate).cloned().collect();
    let mut segments = Vec::new();
    let mut media_offset = base_offset;
    for w in anim.keyframes.windows(2) {
        let (k0, k1) = (&w[0], &w[1]);
        let seg_dur = k1.t - k0.t;
        if seg_dur <= 0.0 {
            continue;
        }
        let mid = (k0.t + k1.t) / 2.0;
        let p = ((mid - k0.t) / seg_dur).clamp(0.0, 1.0);
        let rate = k0.value + (k1.value - k0.value) * ease(k1.easing, p);
        let mut seg = clip.clone();
        seg.id = format!("{}-ramp{}", clip.id, segments.len());
        seg.start = clip.start + k0.t;
        seg.duration = seg_dur;
        seg.animations = other_anims.clone();
        match &mut seg.element {
            Element::Video { rate: r, offset: o, .. } | Element::Audio { rate: r, offset: o, .. } => {
                *r = rate;
                *o = media_offset;
            }
            _ => unreachable!("checked above"),
        }
        media_offset += seg_dur * rate;
        segments.push(seg);
    }
    Some(segments)
}

/// Split a clip at an absolute timeline time (#29). The original keeps
/// the left side; a new clip (returned id) takes the right, with media
/// offsets advanced and animations divided between the halves.
pub fn split_clip(project: &mut Project, id: &str, abs_time: f64) -> Result<String> {
    // Locate the clip and the timeline offset of its container.
    let mut container_offset = 0.0;
    let mut container_span = f64::MAX;
    let mut found = false;
    for (i, scene) in project.scenes.iter().enumerate() {
        if scene.layers.iter().any(|c| c.id == id) {
            container_offset = project.scene_offset(i);
            container_span = scene.duration;
            found = true;
            break;
        }
    }
    if !found && !project.overlays.iter().any(|t| t.clips.iter().any(|c| c.id == id)) {
        bail!("no clip {id:?}");
    }

    let new_id = {
        let mut n = 2;
        let mut candidate = format!("{id}-{n}");
        while find_clip(project, &candidate).is_some() {
            n += 1;
            candidate = format!("{id}-{n}");
        }
        candidate
    };

    let clip = find_clip_mut(project, id).expect("checked above");
    let local = abs_time - container_offset;
    let effective = if clip.duration > 0.0 {
        clip.duration
    } else {
        (container_span - clip.start).max(0.0)
    };
    let split_rel = local - clip.start;
    if split_rel <= 0.05 || split_rel >= effective - 0.05 {
        bail!("split point must fall inside the clip");
    }

    let mut right = clip.clone();
    right.id = new_id.clone();
    right.start = clip.start + split_rel;
    right.duration = effective - split_rel;
    clip.duration = split_rel;

    // Media clips: the right half starts later in the source file.
    match &mut right.element {
        Element::Video { offset, .. } | Element::Audio { offset, .. } => *offset += split_rel,
        _ => {}
    }

    // Animations: keep each half's share, clipped/shifted; a straddling
    // tween is split at the boundary with the interpolated value.
    let value_at = |a: &Anim, t: f64| -> f64 {
        if a.end <= a.start {
            return a.to;
        }
        let p = ((t - a.start) / (a.end - a.start)).clamp(0.0, 1.0);
        a.from + (a.to - a.from) * ease(a.easing, p)
    };
    let left_anims: Vec<Anim> = clip
        .animations
        .iter()
        .filter_map(|a| {
            if !a.keyframes.is_empty() {
                let kfs: Vec<Keyframe> =
                    a.keyframes.iter().filter(|k| k.t <= split_rel).cloned().collect();
                if kfs.len() < 2 {
                    return None;
                }
                let mut a = a.clone();
                a.keyframes = kfs;
                return Some(a);
            }
            if a.start >= split_rel {
                return None;
            }
            let mut a = a.clone();
            if a.end > split_rel {
                a.to = value_at(&a, split_rel);
                a.end = split_rel;
            }
            Some(a)
        })
        .collect();
    let right_anims: Vec<Anim> = right
        .animations
        .iter()
        .filter_map(|a| {
            if !a.keyframes.is_empty() {
                let kfs: Vec<Keyframe> = a
                    .keyframes
                    .iter()
                    .filter(|k| k.t >= split_rel)
                    .map(|k| Keyframe { t: k.t - split_rel, ..k.clone() })
                    .collect();
                if kfs.len() < 2 {
                    return None;
                }
                let mut a = a.clone();
                a.keyframes = kfs;
                return Some(a);
            }
            if a.end <= split_rel {
                return None;
            }
            let mut a = a.clone();
            if a.start < split_rel {
                a.from = value_at(&a, split_rel);
                a.start = 0.0;
            } else {
                a.start -= split_rel;
            }
            a.end -= split_rel;
            Some(a)
        })
        .collect();
    clip.animations = left_anims;
    right.animations = right_anims;

    // Insert the right half next to the original.
    for scene in &mut project.scenes {
        if let Some(pos) = scene.layers.iter().position(|c| c.id == id) {
            scene.layers.insert(pos + 1, right);
            return Ok(new_id);
        }
    }
    for track in &mut project.overlays {
        if let Some(pos) = track.clips.iter().position(|c| c.id == id) {
            track.clips.insert(pos + 1, right);
            return Ok(new_id);
        }
    }
    unreachable!("clip location verified above");
}

/// The classic editor op: silence a video clip's embedded audio and give it
/// an independent audio clip on the "detached-audio" overlay track.
/// Returns the new clip's id.
pub fn detach_audio(project: &mut Project, id: &str) -> Option<String> {
    let (src, offset, volume, start, duration) = {
        let clip = find_clip(project, id)?;
        match &clip.element {
            Element::Video { src, offset, volume, .. } => {
                (src.clone(), *offset, *volume, clip.start, clip.duration)
            }
            _ => return None,
        }
    };
    // Scene clips are scene-relative; the detached audio lives on an
    // overlay track, which is absolutely timed.
    let abs_start = project
        .scenes
        .iter()
        .enumerate()
        .find(|(_, s)| s.layers.iter().any(|c| c.id == id))
        .map(|(i, _)| project.scene_offset(i) + start)
        .unwrap_or(start);
    if let Some(clip) = find_clip_mut(project, id)
        && let Element::Video { volume, .. } = &mut clip.element {
            *volume = 0.0;
        }
    let new_id = format!("{id}-audio");
    let audio = Clip {
        id: new_id.clone(),
        start: abs_start,
        duration,
        element: Element::Audio { src, offset, volume, rate: 1.0 },
        transform: Default::default(),
        animations: Vec::new(),
        effects: Vec::new(),
    };
    if let Some(track) = project.overlays.iter_mut().find(|t| t.id == "detached-audio") {
        track.clips.push(audio);
    } else {
        project.overlays.push(OverlayTrack {
            muted: false,
            hidden: false,
            locked: false,
            id: "detached-audio".into(),
            name: "Detached audio".into(),
            clips: vec![audio],
        });
    }
    Some(new_id)
}

/// Save the given clips as a reusable def named `name` (clips stay in
/// place; the def gets copies with starts normalized so the earliest is 0).
pub fn save_as_def(project: &mut Project, ids: &[String], name: &str) -> Result<()> {
    if name.is_empty() {
        bail!("template name must not be empty");
    }
    if project.defs.contains_key(name) {
        bail!("a def named {name:?} already exists");
    }
    let mut clips: Vec<Clip> = ids
        .iter()
        .filter_map(|id| find_clip(project, id).cloned())
        .collect();
    if clips.is_empty() {
        bail!("no clips selected");
    }
    if clips.iter().any(|c| matches!(c.element, Element::CompRef { .. })) {
        bail!("templates cannot contain template instances (defs cannot nest)");
    }
    let min_start = clips.iter().map(|c| c.start).fold(f64::INFINITY, f64::min);
    for clip in &mut clips {
        clip.start -= min_start;
    }
    project.defs.insert(name.to_string(), CompDef { params: Vec::new(), layers: clips });
    project.validate()
}

/// A timeline lane: scene layer slots first (0..max layers), then overlay
/// tracks. Mirrors the editor strip's row order.
pub fn lane_count(project: &Project) -> usize {
    let max_layers = project.scenes.iter().map(|s| s.layers.len()).max().unwrap_or(0);
    max_layers + project.overlays.len()
}

/// Move a clip to another lane, keeping its on-screen absolute time.
/// Scene-layer targets re-anchor scene-relative (the scene containing
/// `abs_start`); overlay targets store absolute time.
pub fn move_clip_to_lane(
    project: &mut Project,
    id: &str,
    lane: usize,
    abs_start: f64,
) -> Result<()> {
    let max_layers = project.scenes.iter().map(|s| s.layers.len()).max().unwrap_or(0);
    if lane >= lane_count(project) {
        bail!("lane {lane} out of range");
    }
    // Detach the clip from wherever it lives.
    let mut found = None;
    for scene in &mut project.scenes {
        if let Some(pos) = scene.layers.iter().position(|c| c.id == id) {
            found = Some(scene.layers.remove(pos));
            break;
        }
    }
    if found.is_none() {
        for track in &mut project.overlays {
            if let Some(pos) = track.clips.iter().position(|c| c.id == id) {
                found = Some(track.clips.remove(pos));
                break;
            }
        }
    }
    let Some(mut clip) = found else { bail!("clip {id:?} not found") };

    if lane < max_layers {
        // Scene layer slot: find the scene containing abs_start.
        let index = (0..project.scenes.len())
            .rev()
            .find(|&i| abs_start >= project.scene_offset(i))
            .unwrap_or(0);
        let offset = project.scene_offset(index);
        let scene = &mut project.scenes[index];
        clip.start = (abs_start - offset).clamp(0.0, (scene.duration - 0.1).max(0.0));
        while scene.layers.len() < lane {
            // Pad missing slots so the clip lands on the requested lane.
            scene.layers.push(Clip {
                id: format!("{id}-slotpad-{}", scene.layers.len()),
                start: 0.0,
                duration: 0.1,
                element: Element::Test {},
                transform: Transform { opacity: 0.0, ..Default::default() },
                animations: Vec::new(),
                effects: Vec::new(),
            });
        }
        scene.layers.insert(lane.min(scene.layers.len()), clip);
    } else {
        let track = &mut project.overlays[lane - max_layers];
        clip.start = abs_start.max(0.0);
        track.clips.push(clip);
    }
    project.validate()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn demo() -> Project {
        Project::from_json(include_str!("../examples/demo-project.json")).unwrap()
    }

    #[test]
    fn parses_and_validates_demo() {
        let p = demo();
        assert_eq!(p.scenes.len(), 2);
        assert_eq!(p.duration(), 7.0);
        assert_eq!(p.scene_offset(1), 3.0);
    }

    #[test]
    fn roundtrips_json() {
        let p = demo();
        let p2 = Project::from_json(&p.to_json()).unwrap();
        assert_eq!(p.to_json(), p2.to_json());
    }

    #[test]
    fn rejects_duplicate_ids() {
        let mut p = demo();
        let clip = p.scenes[0].layers[0].clone();
        p.scenes[0].layers.push(clip);
        assert!(p.validate().is_err());
    }

    #[test]
    fn rejects_unknown_def_ref() {
        let mut p = demo();
        if let Some(c) = find_clip_mut(&mut p, "media-lower-third") {
            c.element = Element::CompRef { r#ref: "nope".into(), args: Default::default() };
        }
        assert!(p.validate().is_err());
    }

    #[test]
    fn detach_audio_silences_video_and_adds_overlay_clip() {
        let mut p = demo();
        let new_id = detach_audio(&mut p, "media-ball").expect("detaches");
        // Video muted.
        match &find_clip(&p, "media-ball").unwrap().element {
            Element::Video { volume, .. } => assert_eq!(*volume, 0.0),
            _ => panic!("still a video clip"),
        }
        // Audio clip exists on the detached-audio overlay with absolute time
        // (scene-media starts at 3.0, clip start 0 -> abs 3.0).
        let audio = find_clip(&p, &new_id).unwrap();
        assert_eq!(audio.start, 3.0);
        assert!(matches!(audio.element, Element::Audio { .. }));
        assert!(p.validate().is_ok());
    }

    #[test]
    fn remove_silence_splices_out_a_media_relative_gap() {
        let mut p = demo();
        let before = p.duration();
        // media-ball: scene-relative start 0, duration 4, offset 0, in the
        // scene starting at abs 3.0 -- silence at media [1.0, 2.0) is
        // abs [4.0, 5.0).
        let n = remove_silence(&mut p, "media-ball", &[(1.0, 2.0)]).expect("removes");
        assert_eq!(n, 1);
        assert!(p.validate().is_ok());
        // The scene also holds media-lower-third (fixed at [0.5, 3.5)),
        // which bounds how far the scene can shrink even though a full
        // 1s was spliced out of media-ball's own footage.
        assert_eq!(p.duration(), before - 0.5);
        let remaining = find_clip(&p, "media-ball").unwrap();
        assert_eq!(remaining.duration, 1.0); // [0,1) of the original media
    }

    #[test]
    fn remove_silence_skips_ranges_outside_the_used_media_window() {
        let mut p = demo();
        let before = p.duration();
        // Entirely past the clip's 4s duration -- nothing to remove.
        let n = remove_silence(&mut p, "media-ball", &[(10.0, 11.0)]).expect("no-op");
        assert_eq!(n, 0);
        assert_eq!(p.duration(), before);
    }

    fn rate_ramp_clip(keyframes: Vec<Keyframe>) -> Clip {
        Clip {
            id: "ramp".into(),
            start: 10.0,
            duration: 0.0,
            element: Element::Video {
                src: "assets/ball.mp4".into(),
                offset: 5.0,
                volume: 1.0,
                rate: 1.0,
            },
            transform: Default::default(),
            animations: vec![Anim {
                property: AnimProperty::Rate,
                from: 0.0,
                to: 0.0,
                start: 0.0,
                end: 0.0,
                easing: Easing::Linear,
                keyframes,
            }],
            effects: vec![],
        }
    }

    #[test]
    fn expand_rate_ramp_none_without_a_rate_animation() {
        let mut c = rate_ramp_clip(vec![]);
        c.animations.clear();
        assert!(expand_rate_ramp(&c).is_none());
    }

    #[test]
    fn expand_rate_ramp_none_with_fewer_than_two_keyframes() {
        let c = rate_ramp_clip(vec![Keyframe { t: 0.0, value: 2.0, easing: Easing::Linear }]);
        assert!(expand_rate_ramp(&c).is_none());
    }

    #[test]
    fn expand_rate_ramp_two_keyframes_one_segment() {
        // Constant 2x across [0, 4) -- media consumed = 4 * 2 = 8s.
        let c = rate_ramp_clip(vec![
            Keyframe { t: 0.0, value: 2.0, easing: Easing::Linear },
            Keyframe { t: 4.0, value: 2.0, easing: Easing::Linear },
        ]);
        let segs = expand_rate_ramp(&c).expect("expands");
        assert_eq!(segs.len(), 1);
        assert_eq!(segs[0].id, "ramp-ramp0");
        assert_eq!(segs[0].start, 10.0); // clip.start + k0.t
        assert_eq!(segs[0].duration, 4.0);
        assert!(segs[0].animations.is_empty()); // Rate anim consumed, not carried over
        match &segs[0].element {
            Element::Video { rate, offset, .. } => {
                assert_eq!(*rate, 2.0);
                assert_eq!(*offset, 5.0); // base offset, first segment
            }
            _ => panic!("expected video"),
        }
    }

    #[test]
    fn expand_rate_ramp_accumulates_media_offset_across_segments() {
        // [0,2) at 1x (2s media), [2,4) at 3x (6s media) -- second
        // segment's offset must advance by the first's actual media
        // consumption, not just clip-time duration.
        let c = rate_ramp_clip(vec![
            Keyframe { t: 0.0, value: 1.0, easing: Easing::Linear },
            Keyframe { t: 2.0, value: 1.0, easing: Easing::Linear },
            Keyframe { t: 4.0, value: 3.0, easing: Easing::Linear },
        ]);
        let segs = expand_rate_ramp(&c).expect("expands");
        assert_eq!(segs.len(), 2);
        let offsets: Vec<f64> = segs
            .iter()
            .map(|s| match &s.element {
                Element::Video { offset, .. } => *offset,
                _ => panic!("expected video"),
            })
            .collect();
        assert_eq!(offsets[0], 5.0); // base offset
        assert_eq!(offsets[1], 5.0 + 2.0 * 1.0); // + first segment's media span
    }

    #[test]
    fn rate_tween_without_keyframes_still_rejected() {
        let mut c = rate_ramp_clip(vec![]);
        c.animations = vec![Anim {
            property: AnimProperty::Rate,
            from: 1.0,
            to: 2.0,
            start: 0.0,
            end: 2.0,
            easing: Easing::Linear,
            keyframes: vec![],
        }];
        assert!(c.validate(&BTreeMap::new()).is_err());
    }

    #[test]
    fn rate_keyframes_out_of_range_rejected() {
        let c = rate_ramp_clip(vec![
            Keyframe { t: 0.0, value: 0.05, easing: Easing::Linear },
            Keyframe { t: 2.0, value: 2.0, easing: Easing::Linear },
        ]);
        assert!(c.validate(&BTreeMap::new()).is_err());
    }

    #[test]
    fn valid_rate_keyframes_pass_clip_validation() {
        let c = rate_ramp_clip(vec![
            Keyframe { t: 0.0, value: 1.0, easing: Easing::Linear },
            Keyframe { t: 2.0, value: 2.0, easing: Easing::Linear },
        ]);
        assert!(c.validate(&BTreeMap::new()).is_ok());
    }

    #[test]
    fn remove_clip_removes_everywhere() {
        let mut p = demo();
        remove_clip(&mut p, "wm-text");
        assert!(find_clip(&p, "wm-text").is_none());
        assert!(p.validate().is_ok());
    }

    #[test]
    fn transitions_overlap_scene_offsets() {
        let mut p = demo();
        p.scenes[1].transition = Some(Transition { kind: TransitionKind::Crossfade, duration: 1.0 });
        assert_eq!(p.scene_offset(1), 2.0); // pulled 1s into scene 1
        assert_eq!(p.duration(), 6.0); // 3 + 4 - 1
        assert!(p.validate().is_ok());
        p.scenes[1].transition = Some(Transition { kind: TransitionKind::Crossfade, duration: 5.0 });
        assert!(p.validate().is_err()); // longer than the scene itself
    }

    #[test]
    fn save_as_def_normalizes_and_validates() {
        let mut p = demo();
        save_as_def(
            &mut p,
            &["intro-title".into(), "wm-text".into()],
            "my-template",
        )
        .unwrap();
        let def = &p.defs["my-template"];
        assert_eq!(def.layers.len(), 2);
        // earliest start normalized to 0 (intro-title 0.4, wm-text 0.5)
        assert_eq!(def.layers.iter().map(|c| c.start).fold(f64::INFINITY, f64::min), 0.0);
        assert!(p.validate().is_ok());
        // duplicate name refused
        assert!(save_as_def(&mut p, &["intro-bg".into()], "my-template").is_err());
        // compref content refused
        assert!(save_as_def(&mut p, &["media-lower-third".into()], "t2").is_err());
    }

    #[test]
    fn move_clip_between_lanes() {
        let mut p = demo();
        // wm-text lives on the overlay (lane 2: 2 scene slots + overlay 0).
        // Move it into scene layer slot 0 at abs 4.0 (scene-media, rel 1.0).
        move_clip_to_lane(&mut p, "wm-text", 0, 4.0).unwrap();
        let scene = p.scenes.iter().find(|s| s.id == "scene-media").unwrap();
        let clip = scene.layers.iter().find(|c| c.id == "wm-text").unwrap();
        assert!((clip.start - 1.0).abs() < 1e-9);
        assert!(p.overlays.iter().all(|t| t.clips.iter().all(|c| c.id != "wm-text")));

        // And back out to the overlay track, absolute time preserved.
        let max_layers = p.scenes.iter().map(|s| s.layers.len()).max().unwrap();
        move_clip_to_lane(&mut p, "wm-text", max_layers, 2.5).unwrap();
        let clip = p.overlays[0].clips.iter().find(|c| c.id == "wm-text").unwrap();
        assert_eq!(clip.start, 2.5);
        assert!(p.validate().is_ok());
    }

    #[test]
    fn document_round_trips_exactly() {
        let mut p = demo();
        // Exercise every optional surface so serialization gaps surface.
        p.library.push("assets/ball.mp4".into());
        p.overlays[0].muted = true;
        p.overlays[0].hidden = true;
        if let Some(c) = find_clip_mut(&mut p, "intro-title") {
            if let Element::Text { align, outline, shadow, .. } = &mut c.element {
                *align = Some(TextAlign::Right);
                *outline = Some("#102030".into());
                *shadow = true;
            }
            c.effects.push(Effect::ChromaKey {
                color: "#00ff00".into(), angle: 30.0, noise: 4.0,
            });
            c.effects.push(Effect::Eq { low: -6.0, mid: 0.0, high: 3.0 });
            c.animations.push(Anim {
                property: AnimProperty::Volume,
                from: 0.0, to: 0.0, start: 0.0, end: 0.0,
                easing: Easing::Linear,
                keyframes: vec![
                    Keyframe { t: 0.0, value: 1.0, easing: Easing::Linear },
                    Keyframe { t: 1.0, value: 0.2, easing: Easing::EaseOut },
                ],
            });
        }
        let json = p.to_json();
        let back = Project::from_json(&json).expect("round trip parses");
        assert_eq!(json, back.to_json(), "serialize -> parse -> serialize must be stable");
    }

    #[test]
    fn validation_rejects_bad_documents() {
        // Duplicate ids.
        let mut p = demo();
        let dup = p.scenes[0].layers[0].clone();
        p.scenes[0].layers.push(dup);
        assert!(p.validate().is_err(), "duplicate ids must fail");
        // Bad transition duration.
        let mut p = demo();
        if let Some(t) = &mut p.scenes[1].transition {
            t.duration = -1.0;
        }
        assert!(p.validate().is_err() || p.scenes[1].transition.is_none());
        // Unknown compref.
        let mut p = demo();
        p.scenes[0].layers[0].element =
            Element::CompRef { r#ref: "missing".into(), args: Default::default() };
        assert!(p.validate().is_err(), "unknown def must fail");
        // Out-of-range effect.
        let mut p = demo();
        p.scenes[0].layers[0].effects.push(Effect::Eq { low: -99.0, mid: 0.0, high: 0.0 });
        assert!(p.validate().is_err(), "eq out of range must fail");
        // Out-of-range chromakey.
        let mut p = demo();
        p.scenes[0].layers[0].effects.push(Effect::ChromaKey {
            color: "#00ff00".into(), angle: 0.5, noise: 0.0,
        });
        assert!(p.validate().is_err(), "chromakey angle out of range must fail");
    }

    #[test]
    fn split_rejects_edges_and_unknown() {
        let mut p = demo();
        assert!(split_clip(&mut p, "nope", 1.0).is_err());
        // At the very start of the clip: rejected.
        assert!(split_clip(&mut p, "media-ball", 3.0).is_err());
    }

    #[test]
    fn grow_scene_for_clip_expands_but_never_shrinks() {
        let mut p = demo();
        // media-ball: scene-media (duration 4.0), start 0, duration 4.
        if let Some(c) = find_clip_mut(&mut p, "media-ball") {
            c.start = 6.0;
            c.duration = 5.0;
        }
        grow_scene_for_clip(&mut p, "media-ball");
        assert!((p.scenes[1].duration - 11.0).abs() < 1e-6);
        // Shrinking the clip back doesn't shrink the scene.
        if let Some(c) = find_clip_mut(&mut p, "media-ball") {
            c.duration = 1.0;
        }
        grow_scene_for_clip(&mut p, "media-ball");
        assert!((p.scenes[1].duration - 11.0).abs() < 1e-6);
    }

    #[test]
    fn ripple_delete_closes_gaps() {
        let mut p = demo();
        p.overlays[0].clips.push(Clip {
            id: "late".into(), start: 6.0, duration: 1.0,
            element: Element::Text { text: "x".into(), font: default_font(), color: default_color(), align: None, outline: None, shadow: false },
            transform: Default::default(), animations: vec![], effects: vec![],
        });
        let first = p.overlays[0].clips[0].clone();
        ripple_delete(&mut p, &first.id).unwrap();
        let late = find_clip(&p, "late").unwrap();
        assert!((late.start - (6.0 - first.duration)).abs() < 1e-6);
        let before = p.scenes[1].duration;
        ripple_delete(&mut p, "media-ball").unwrap();
        assert!(p.scenes[1].duration <= before);
        assert!(p.validate().is_ok());
    }

    #[test]
    fn split_clip_divides_media_and_animations() {
        let mut p = demo();
        if let Some(c) = find_clip_mut(&mut p, "media-ball") {
            c.animations.push(Anim {
                property: AnimProperty::Opacity,
                from: 0.0, to: 1.0, start: 0.0, end: 4.0,
                easing: Easing::Linear, keyframes: vec![],
            });
        }
        let new_id = split_clip(&mut p, "media-ball", 5.0).unwrap();
        let left = find_clip(&p, "media-ball").unwrap();
        let right = find_clip(&p, &new_id).unwrap();
        assert!((left.duration - 2.0).abs() < 1e-6);
        assert!((right.start - 2.0).abs() < 1e-6);
        assert!((right.duration - 2.0).abs() < 1e-6);
        if let Element::Video { offset, .. } = &right.element {
            assert!((offset - 2.0).abs() < 1e-6);
        } else {
            panic!("right half should stay a video clip");
        }
        assert!((left.animations[0].to - 0.5).abs() < 1e-6);
        assert!((right.animations[0].from - 0.5).abs() < 1e-6);
        assert!((right.animations[0].end - 2.0).abs() < 1e-6);
        assert!(p.validate().is_ok());
        assert!(split_clip(&mut p, &new_id, 30.0).is_err());
    }

    #[test]
    fn nested_defs_expand_and_cycles_fail() {
        let mut p = demo();
        p.defs.insert("inner".into(), CompDef {
            params: vec!["msg".into()],
            layers: vec![Clip {
                id: "in-t".into(), start: 0.0, duration: 0.0,
                element: Element::Text {
                    text: "{msg}".into(), font: default_font(), color: default_color(),
                    align: None, outline: None, shadow: false,
                },
                transform: Default::default(), animations: vec![], effects: vec![],
            }],
        });
        p.defs.insert("outer".into(), CompDef {
            params: vec!["msg".into()],
            layers: vec![Clip {
                id: "out-ref".into(), start: 0.0, duration: 0.0,
                element: Element::CompRef {
                    r#ref: "inner".into(),
                    args: [("msg".to_string(), "{msg}".to_string())].into(),
                },
                transform: Default::default(), animations: vec![], effects: vec![],
            }],
        });
        assert!(p.validate().is_ok());
        // introduce a cycle inner -> outer
        p.defs.get_mut("inner").unwrap().layers[0].element = Element::CompRef {
            r#ref: "outer".into(), args: Default::default(),
        };
        assert!(p.validate().is_err());
    }

    #[test]
    fn effects_and_volume_anim_validate() {
        let mut p = demo();
        if let Some(c) = find_clip_mut(&mut p, "media-ball") {
            c.effects.push(Effect::Blur { amount: 4.0 });
            c.effects.push(Effect::Color {
                brightness: 0.1, contrast: 1.1, saturation: 0.8, hue: 0.0,
            });
            c.animations.push(Anim {
                property: AnimProperty::Volume,
                from: 1.0, to: 0.0, start: 2.0, end: 3.0,
                easing: Easing::EaseOut, keyframes: vec![],
            });
        }
        assert!(p.validate().is_ok());
        if let Some(c) = find_clip_mut(&mut p, "media-ball") {
            c.effects.push(Effect::Blur { amount: 99.0 });
        }
        assert!(p.validate().is_err());
    }

    #[test]
    fn keyframe_animations_validate() {
        let mut p = demo();
        if let Some(c) = find_clip_mut(&mut p, "intro-title") {
            c.animations.push(Anim {
                property: AnimProperty::Y,
                from: 0.0, to: 0.0, start: 0.0, end: 0.0,
                easing: Easing::Linear,
                keyframes: vec![
                    Keyframe { t: 0.0, value: 700.0, easing: Easing::Linear },
                    Keyframe { t: 1.0, value: 300.0, easing: Easing::EaseOut },
                    Keyframe { t: 2.0, value: 320.0, easing: Easing::EaseInOut },
                ],
            });
        }
        assert!(p.validate().is_ok());
        // out-of-order keyframes rejected
        if let Some(c) = find_clip_mut(&mut p, "intro-title") {
            c.animations.last_mut().unwrap().keyframes[2].t = 0.5;
        }
        assert!(p.validate().is_err());
    }

    #[test]
    fn parse_color_handles_rgb_and_argb() {
        assert_eq!(parse_color("#ffffff"), 0xffffffff);
        assert_eq!(parse_color("#80ffffff"), 0x80ffffff);
    }

    #[test]
    fn effective_duration_table() {
        let mut clip = Clip {
            id: "test".into(),
            start: 0.0,
            duration: 0.0,
            element: Element::Text {
                text: "hi".into(),
                font: default_font(),
                color: default_color(),
                align: None,
                outline: None,
                shadow: false,
            },
            transform: Default::default(),
            animations: Vec::new(),
            effects: Vec::new(),
        };
        let scene = Scene {
            id: "scene-1".into(),
            name: String::new(),
            duration: 5.0,
            layers: Vec::new(),
            transition: None,
        };

        // (0.0, scene 5.0, start 0) → 5.0
        assert_eq!(effective_duration(&clip, Some(&scene)), 5.0);

        // (0.0, start 2.5) → 2.5
        clip.start = 2.5;
        assert_eq!(effective_duration(&clip, Some(&scene)), 2.5);

        // (2.5 explicit) → 2.5
        clip.duration = 2.5;
        assert_eq!(effective_duration(&clip, Some(&scene)), 2.5);

        // (0.0, start 4.95) → 0.1 (the .max(0.1) clamp)
        clip.duration = 0.0;
        clip.start = 4.95;
        assert!((effective_duration(&clip, Some(&scene)) - 0.1).abs() < 1e-6);

        // overlay clip → raw value
        clip.duration = 0.0;
        clip.start = 0.0;
        assert_eq!(effective_duration(&clip, None), 0.0);
    }
}
