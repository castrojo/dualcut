//! Timeline and document mutation operations.
use anyhow::{bail, Result};
use super::{
    ease, find_clip, find_clip_mut, Anim, AnimProperty, Clip, CompDef, Element, Keyframe,
    OverlayTrack, Project, Transform,
};

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
