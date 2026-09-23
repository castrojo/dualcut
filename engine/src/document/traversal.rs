//! Generic traversal over clips stored in scenes and overlay tracks.

use super::{Clip, Project, Scene};

/// Compute the effective duration of a clip.
///
/// For scene layers, a duration `<= 0.0` fills the rest of the scene:
/// `(scene.duration - clip.start).max(0.1)`. Overlay clips and clips without a
/// parent scene do not inherit this rule and return their raw duration.
pub fn effective_duration(clip: &Clip, scene: Option<&Scene>) -> f64 {
    if clip.duration > 0.0 {
        clip.duration
    } else if let Some(scene) = scene {
        (scene.duration - clip.start).max(0.1)
    } else {
        clip.duration
    }
}

/// Find a clip anywhere in the project by id.
pub fn find_clip<'a>(project: &'a Project, id: &str) -> Option<&'a Clip> {
    project
        .scenes
        .iter()
        .flat_map(|scene| scene.layers.iter())
        .chain(
            project
                .overlays
                .iter()
                .flat_map(|track| track.clips.iter()),
        )
        .find(|clip| clip.id == id)
}

/// Find a mutable clip anywhere in the project by id.
pub fn find_clip_mut<'a>(project: &'a mut Project, id: &str) -> Option<&'a mut Clip> {
    if let Some(clip) = project
        .scenes
        .iter_mut()
        .flat_map(|scene| scene.layers.iter_mut())
        .find(|clip| clip.id == id)
    {
        return Some(clip);
    }

    project
        .overlays
        .iter_mut()
        .flat_map(|track| track.clips.iter_mut())
        .find(|clip| clip.id == id)
}

/// Remove every clip with the given id from scenes and overlay tracks.
pub fn remove_clip(project: &mut Project, id: &str) {
    for scene in &mut project.scenes {
        scene.layers.retain(|clip| clip.id != id);
    }
    for track in &mut project.overlays {
        track.clips.retain(|clip| clip.id != id);
    }
}
