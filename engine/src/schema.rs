//! Document schema (dualcut#205 Phase 1): the versioned, immutable data
//! model that describes the structure of a video composition — scenes,
//! overlays, clips, effects, timeline — independent of persistence
//! (serialization, file I/O, migration) and UI state (selection, viewport,
//! undo/redo).
//!
//! This is the schema slice of `crate::document`; that module re-exports
//! these types and adds the editable operations layer on top. See
//! dualcut#187 for the full decomposition roadmap.

use anyhow::{bail, Result};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

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
