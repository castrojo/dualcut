//! Project and clip invariant validation.
use anyhow::{bail, Result};
use std::collections::BTreeMap;
use super::{AnimProperty, Clip, CompDef, Effect, Element, Project};
impl Project {
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
    pub(super) fn validate(&self, defs: &BTreeMap<String, CompDef>) -> Result<()> {
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
