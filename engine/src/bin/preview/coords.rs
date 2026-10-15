//! Coordinate mapping, clip bounding boxes, snapping, and media resolution
//! utilities.
//!
//! Extracted from `preview.rs` (dualcut#78): `clip_box` / `active_clips_at` /
//! `widget_to_comp` / `snap_time` handle layout and hit testing, while
//! `failed_*` caches and `media_uri` / `fx_hash` support thumbnail/proxy resolution.

use super::*;

pub(crate) fn clip_box(project: &Project, clip: &document::Clip) -> (f64, f64, f64, f64) {
    let t = &clip.transform;
    let w = if t.width > 0.0 { t.width } else { project.meta.width as f64 };
    let h = if t.height > 0.0 { t.height } else { project.meta.height as f64 };
    (t.x, t.y, w, h)
}

/// Clips active at `time` with absolute-time info, topmost first
/// (overlays before scene layers, lower layer index above higher).
pub(crate) fn active_clips_at(project: &Project, time: f64) -> Vec<(String, f64, f64, f64, f64)> {
    let mut out = Vec::new();
    for track in &project.overlays {
        for clip in &track.clips {
            if time >= clip.start && time < clip.start + clip.duration.max(0.01) {
                let (x, y, w, h) = clip_box(project, clip);
                out.push((clip.id.clone(), x, y, w, h));
            }
        }
    }
    for (i, scene) in project.scenes.iter().enumerate() {
        let offset = project.scene_offset(i);
        if time < offset || time >= offset + scene.duration {
            continue;
        }
        for clip in &scene.layers {
            let local = time - offset;
            let duration = if clip.duration > 0.0 { clip.duration } else { scene.duration - clip.start };
            if local >= clip.start && local < clip.start + duration {
                let (x, y, w, h) = clip_box(project, clip);
                out.push((clip.id.clone(), x, y, w, h));
            }
        }
    }
    out
}

/// Map preview-widget coords to composition coords through ContentFit::Contain
/// letterboxing. Returns None outside the video area.
pub(crate) fn widget_to_comp(
    project: &Project,
    widget_w: f64,
    widget_h: f64,
    wx: f64,
    wy: f64,
) -> Option<(f64, f64, f64)> {
    let (cw, ch) = (project.meta.width as f64, project.meta.height as f64);
    let scale = (widget_w / cw).min(widget_h / ch);
    if scale <= 0.0 {
        return None;
    }
    let (vw, vh) = (cw * scale, ch * scale);
    let (ox, oy) = ((widget_w - vw) / 2.0, (widget_h - vh) / 2.0);
    let (cx, cy) = ((wx - ox) / scale, (wy - oy) / scale);
    if cx < 0.0 || cy < 0.0 || cx > cw || cy > ch {
        return None;
    }
    Some((cx, cy, scale))
}

/// Snap a time to scene boundaries or the half-second grid (0.15s window).
pub(crate) fn snap_time(project: &Project, raw: f64) -> f64 {
    const WINDOW: f64 = 0.15;
    let mut candidates: Vec<f64> = (0..=project.scenes.len())
        .map(|i| {
            if i == project.scenes.len() {
                project.duration()
            } else {
                project.scene_offset(i)
            }
        })
        .collect();
    candidates.push((raw * 2.0).round() / 2.0);
    candidates
        .into_iter()
        .filter(|c| (c - raw).abs() <= WINDOW)
        .min_by(|a, b| (a - raw).abs().total_cmp(&(b - raw).abs()))
        .unwrap_or(raw)
        .max(0.0)
}

/// Proxy transcodes that failed this session — skipped on later rebuilds.
pub(crate) fn failed_thumbs() -> &'static std::sync::Mutex<std::collections::HashSet<String>> {
    static SET: std::sync::OnceLock<std::sync::Mutex<std::collections::HashSet<String>>> =
        std::sync::OnceLock::new();
    SET.get_or_init(|| std::sync::Mutex::new(std::collections::HashSet::new()))
}

pub(crate) fn failed_proxies() -> &'static std::sync::Mutex<std::collections::HashSet<String>> {
    static FAILED: std::sync::OnceLock<std::sync::Mutex<std::collections::HashSet<String>>> =
        std::sync::OnceLock::new();
    FAILED.get_or_init(Default::default)
}

/// Def names whose Templates-tab preview thumbnail failed this session --
/// skipped on later rebuilds. Defs with a video/audio/image layer can
/// never render a preview this way (the param-preview substitution fakes
/// a value like "CLIP" for a {clip} param, which isn't a real media
/// path), so without this a rebuild_strip() on every zoom/edit would
/// retry -- and fail -- the same doomed thumbnail forever.
pub(crate) fn failed_templates() -> &'static std::sync::Mutex<std::collections::HashSet<String>> {
    static FAILED: std::sync::OnceLock<std::sync::Mutex<std::collections::HashSet<String>>> =
        std::sync::OnceLock::new();
    FAILED.get_or_init(Default::default)
}

/// Thin preview-side wrapper over [`dualcut_engine::media::resolve`]:
/// the preview skips what it cannot resolve instead of failing, so an
/// unresolvable `src` is `None` rather than an error.
pub(crate) fn media_uri(src: &str, base_dir: &std::path::Path) -> Option<String> {
    dualcut_engine::media::resolve(src, base_dir).ok()
}

pub(crate) fn fx_hash(s: &str) -> u64 {
    let mut h: u64 = 0xcbf29ce484222325;
    for b in s.bytes() {
        h ^= b as u64;
        h = h.wrapping_mul(0x100000001b3);
    }
    h
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_clip(id: &str, start: f64, duration: f64) -> document::Clip {
        document::Clip {
            id: id.into(),
            start,
            duration,
            element: document::Element::Video {
                src: "clip.mp4".into(),
                offset: 0.0,
                volume: 1.0,
                rate: 1.0,
            },
            transform: Default::default(),
            animations: Vec::new(),
            effects: Vec::new(),
        }
    }

    #[test]
    fn clip_box_falls_back_to_project_canvas_size_when_transform_is_zero() {
        let project = dualcut_engine::templates::new_project("t");
        let clip = test_clip("c1", 0.0, 1.0);
        let (x, y, w, h) = clip_box(&project, &clip);
        assert_eq!((x, y, w, h), (0.0, 0.0, 1920.0, 1080.0));
    }

    #[test]
    fn clip_box_uses_explicit_transform_when_set() {
        let project = dualcut_engine::templates::new_project("t");
        let mut clip = test_clip("c1", 0.0, 1.0);
        clip.transform = document::Transform { x: 10.0, y: 20.0, width: 300.0, height: 200.0, opacity: 1.0 };
        assert_eq!(clip_box(&project, &clip), (10.0, 20.0, 300.0, 200.0));
    }

    #[test]
    fn active_clips_at_finds_scene_layers_within_their_time_window() {
        let mut project = dualcut_engine::templates::new_project("t"); // 0..5
        project.scenes[0].layers.push(test_clip("c1", 1.0, 1.0)); // active in [1.0, 2.0)
        assert!(active_clips_at(&project, 0.5).is_empty());
        let hits = active_clips_at(&project, 1.5);
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].0, "c1");
        assert!(active_clips_at(&project, 3.0).is_empty());
    }

    #[test]
    fn active_clips_at_finds_overlay_clips_by_absolute_time_across_scenes() {
        let mut project = dualcut_engine::templates::new_project("t"); // scene 0: 0..5
        project.scenes.push(document::Scene {
            id: "s2".into(),
            name: String::new(),
            duration: 5.0,
            transition: None,
            layers: Vec::new(),
        }); // scene 1: 5..10
        project.overlays.push(document::OverlayTrack {
            id: "ov".into(),
            muted: false,
            hidden: false,
            locked: false,
            name: String::new(),
            clips: vec![test_clip("overlay-clip", 6.0, 2.0)], // active in [6.0, 8.0)
        });
        assert!(active_clips_at(&project, 0.5).iter().all(|c| c.0 != "overlay-clip"));
        let hits = active_clips_at(&project, 6.5);
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].0, "overlay-clip");
    }

    #[test]
    fn active_clips_at_overlays_come_before_scene_layers() {
        let mut project = dualcut_engine::templates::new_project("t");
        project.scenes[0].layers.push(test_clip("scene-clip", 0.0, 5.0));
        project.overlays.push(document::OverlayTrack {
            id: "ov".into(),
            muted: false,
            hidden: false,
            locked: false,
            name: String::new(),
            clips: vec![test_clip("overlay-clip", 0.0, 5.0)],
        });
        let hits = active_clips_at(&project, 1.0);
        assert_eq!(hits.len(), 2);
        assert_eq!(hits[0].0, "overlay-clip", "overlays should be topmost");
    }

    #[test]
    fn widget_to_comp_maps_widget_coords_through_letterboxing() {
        let project = dualcut_engine::templates::new_project("t"); // 1920x1080
        // A 1920x1080 comp in a 960x1080 widget (2:1 taller-than-video
        // widget) is scaled by 0.5 to 960x540, letterboxed top/bottom.
        let (cx, cy, scale) = widget_to_comp(&project, 960.0, 1080.0, 480.0, 270.0 + 270.0)
            .expect("center of the video area maps to a composition point");
        assert_eq!(scale, 0.5);
        assert!((cx - 960.0).abs() < 0.01);
        assert!((cy - 540.0).abs() < 0.01);
    }

    #[test]
    fn widget_to_comp_returns_none_outside_the_letterboxed_video_area() {
        let project = dualcut_engine::templates::new_project("t");
        // Same 960x1080 widget as above: the video occupies y in
        // [270, 810], so y=10 lands in the top letterbox bar.
        assert!(widget_to_comp(&project, 960.0, 1080.0, 480.0, 10.0).is_none());
    }

    #[test]
    fn snap_time_snaps_to_a_nearby_scene_boundary() {
        let mut project = dualcut_engine::templates::new_project("t"); // scene 1: 0..5
        project.scenes.push(document::Scene {
            id: "scene-2".into(),
            name: String::new(),
            duration: 5.0,
            transition: None,
            layers: Vec::new(),
        });
        assert_eq!(snap_time(&project, 5.05), 5.0);
    }

    #[test]
    fn snap_time_falls_back_to_the_half_second_grid_far_from_any_boundary() {
        let project = dualcut_engine::templates::new_project("t");
        assert_eq!(snap_time(&project, 2.4), 2.5);
    }

    #[test]
    fn snap_time_never_returns_negative() {
        let project = dualcut_engine::templates::new_project("t");
        assert_eq!(snap_time(&project, -1.0), 0.0);
    }

    #[test]
    fn fx_hash_is_deterministic_and_distinguishes_inputs() {
        assert_eq!(fx_hash("hello"), fx_hash("hello"));
        assert_ne!(fx_hash("hello"), fx_hash("world"));
        assert_ne!(fx_hash(""), fx_hash("a"));
    }

    #[test]
    fn media_uri_passes_through_an_existing_uri_unchanged() {
        let base = std::path::Path::new("/does/not/matter");
        assert_eq!(media_uri("file:///already/a/uri.mp4", base), Some("file:///already/a/uri.mp4".into()));
        assert_eq!(media_uri("https://example.com/a.mp4", base), Some("https://example.com/a.mp4".into()));
    }

    #[test]
    fn media_uri_resolves_an_existing_relative_path_to_a_file_uri() {
        let dir = std::env::temp_dir().join("dualcut-media-uri-test");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("clip.mp4"), b"fake").unwrap();
        let uri = media_uri("clip.mp4", &dir).expect("existing relative path resolves");
        assert!(uri.starts_with("file://"));
        assert!(uri.ends_with("clip.mp4"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn media_uri_returns_none_for_a_relative_path_that_does_not_exist() {
        let base = std::env::temp_dir();
        assert_eq!(media_uri("this-file-does-not-exist-anywhere.mp4", &base), None);
    }
}
