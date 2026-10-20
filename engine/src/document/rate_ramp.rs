//! Expansion of animated playback-rate clips into static-rate segments.
use super::{ease, Anim, AnimProperty, Clip, Element};
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
