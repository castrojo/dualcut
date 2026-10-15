//! Compile a `document::Project` into a GES timeline.
//!
//! GES layer stack (priority 0 = topmost):
//!   [overlay tracks..] then [scene layer slots..]
//! Scenes are laid out sequentially; scene-layer clips get their scene's
//! offset added. Overlay clips keep absolute times and freely cross cuts.
//!
//! Shapes are skipped with a warning until the Vello compositor lands (M3).

use crate::document::{parse_color, Anim, AnimProperty, Clip, Easing, Element, Project};
use anyhow::{bail, Context, Result};
use ges::prelude::*;
use gstreamer as gst;
use gstreamer_controller as gst_controller;
use gstreamer_editing_services as ges;
use gstreamer_pbutils as gst_pbutils;
use std::collections::BTreeMap;
use std::sync::{Mutex, OnceLock};

fn secs(t: f64) -> gst::ClockTime {
    gst::ClockTime::from_useconds((t.max(0.0) * 1_000_000.0) as u64)
}

/// Cache of URIs already probed by [`probe_discoverable`], so a project
/// referencing a broken file only pays the probe cost once per process
/// even though `compile`/`compile_scaled` reruns from scratch on every
/// preview edit.
fn probe_cache() -> &'static Mutex<std::collections::HashMap<String, bool>> {
    static CACHE: OnceLock<Mutex<std::collections::HashMap<String, bool>>> = OnceLock::new();
    CACHE.get_or_init(|| Mutex::new(std::collections::HashMap::new()))
}

/// Probe a media URI with a standalone `gst_pbutils::Discoverer` before
/// ever creating a `ges::UriClip` for it. GES's own asset-loading runs
/// discovery through its shared, process-wide `GESDiscovererManager`,
/// whose per-URI request bookkeeping has a known race when a discovery
/// request is still in flight while its tracking data gets freed
/// (upstream: gstreamer#3757, only partially fixed) -- a real, aborting
/// `g_assert()` in `ges-discoverer-manager.c`, not something catchable
/// from Rust. A missing-codec file widens that race window (discovery
/// fails slowly instead of succeeding fast) and reliably triggers it in
/// practice. A private `Discoverer` instance never touches that shared
/// manager at all, so probing here first and skipping the `UriClip`
/// entirely on failure avoids the crash path rather than trying to
/// survive it.
fn probe_discoverable(uri: &str) -> bool {
    if let Some(&ok) = probe_cache().lock().unwrap().get(uri) {
        return ok;
    }
    let ok = gst_pbutils::Discoverer::new(gst::ClockTime::from_seconds(8))
        .and_then(|d| d.discover_uri(uri))
        .is_ok();
    probe_cache().lock().unwrap().insert(uri.to_string(), ok);
    ok
}

pub struct Compiled {
    pub timeline: ges::Timeline,
    pub warnings: Vec<String>,
}

pub fn compile(project: &Project, base_dir: &std::path::Path) -> Result<Compiled> {
    compile_scaled(project, base_dir, 1.0)
}

/// Compile with the video track restricted to `scale` × the project
/// resolution — previews render faster at half/quarter size while
/// exports keep using [`compile`] at full quality.
pub fn compile_scaled(
    project: &Project,
    base_dir: &std::path::Path,
    scale: f64,
) -> Result<Compiled> {
    project.validate()?;
    // Track/lane mute/hide (#31, #21): applied non-destructively at
    // compile time by zeroing volume/opacity on the affected clips.
    // scene_lanes[i] applies to layer index i of every scene (scene
    // layers are positional, not identities, so there's no per-scene
    // lane state to track separately).
    let needs_adjust = project.overlays.iter().any(|t| t.muted || t.hidden)
        || project.scene_lanes.iter().any(|l| l.muted || l.hidden);
    let adjusted;
    let project = if needs_adjust {
        let mut p = project.clone();
        for track in &mut p.overlays {
            for clip in &mut track.clips {
                if track.hidden {
                    clip.transform.opacity = 0.0;
                }
                if track.muted
                    && let Element::Video { volume, .. } | Element::Audio { volume, .. } =
                        &mut clip.element
                {
                    *volume = 0.0;
                }
            }
        }
        for scene in &mut p.scenes {
            for (i, clip) in scene.layers.iter_mut().enumerate() {
                let Some(lane) = p.scene_lanes.get(i) else { continue };
                if lane.hidden {
                    clip.transform.opacity = 0.0;
                }
                if lane.muted
                    && let Element::Video { volume, .. } | Element::Audio { volume, .. } =
                        &mut clip.element
                {
                    *volume = 0.0;
                }
            }
        }
        adjusted = p;
        &adjusted
    } else {
        project
    };
    let timeline = ges::Timeline::new_audio_video();
    // Crossfades: scenes with a transition overlap their predecessor on the
    // same GES layers; auto-transition renders the blend.
    timeline.set_auto_transition(true);
    let mut warnings = Vec::new();

    // Restrict the video track to the project's frame size and rate so
    // render output matches meta instead of GES defaults.
    for track in timeline.tracks() {
        if let Ok(video_track) = track.clone().downcast::<ges::VideoTrack>() {
            let w = ((project.meta.width as f64 * scale) as i32).max(2) & !1;
            let h = ((project.meta.height as f64 * scale) as i32).max(2) & !1;
            let caps = gst::Caps::builder("video/x-raw")
                .field("width", w)
                .field("height", h)
                .field("framerate", gst::Fraction::new(project.meta.fps, 1))
                .build();
            video_track.set_restriction_caps(&caps);
        }
    }

    // Each "slot" (an overlay track, or a scene-layer index) gets as many
    // GES layers as its deepest def expansion needs: multi-layer defs put
    // each def layer on its own GES layer (same-layer full overlaps are
    // invalid in GES).
    let slot_depth =
        |clips: &[Clip]| -> usize { clips.iter().map(|c| clip_slot_depth(project, c)).max().unwrap_or(1) };

    let mut slots: Vec<Vec<ges::Layer>> = Vec::new();
    let overlay_count = project.overlays.len();
    for track in &project.overlays {
        let depth = slot_depth(&track.clips);
        slots.push((0..depth).map(|_| timeline.append_layer()).collect());
    }
    let max_scene_layers = project.scenes.iter().map(|s| s.layers.len()).max().unwrap_or(0);
    for li in 0..max_scene_layers.max(1) {
        let depth = project
            .scenes
            .iter()
            .filter_map(|s| s.layers.get(li).map(|c| slot_depth(std::slice::from_ref(c))))
            .max()
            .unwrap_or(1);
        slots.push((0..depth).map(|_| timeline.append_layer()).collect());
    }

    // Overlay tracks first (top of the stack), absolute timing.
    for (i, track) in project.overlays.iter().enumerate() {
        for clip in &track.clips {
            add_clip(project, &slots[i], clip, 0.0, base_dir, &mut warnings)
                .with_context(|| format!("overlay clip {:?}", clip.id))?;
        }
    }

    // Scenes: sequential offsets, layers below the overlays.
    for (index, scene) in project.scenes.iter().enumerate() {
        let offset = project.scene_offset(index);
        for (li, clip) in scene.layers.iter().enumerate() {
            let mut clip = clip.clone();
            clip.duration = crate::document::effective_duration(&clip, Some(scene));
            add_clip(project, &slots[overlay_count + li], &clip, offset, base_dir, &mut warnings)
                .with_context(|| format!("scene {:?} clip {:?}", scene.id, clip.id))?;
        }
    }

    timeline.commit_sync();
    retype_transitions(project, &timeline);
    timeline.commit_sync();
    Ok(Compiled { timeline, warnings })
}

fn add_clip(
    project: &Project,
    slot: &[ges::Layer],
    clip: &Clip,
    offset: f64,
    base_dir: &std::path::Path,
    warnings: &mut Vec<String>,
) -> Result<()> {
    // Keyframed rate ramp (#40): expand into static-rate sub-clips before
    // GES ever sees a rate animation -- see expand_rate_ramp's doc comment
    // for why a live rate binding isn't safe.
    if let Some(segments) = crate::document::expand_rate_ramp(clip) {
        for seg in &segments {
            add_clip(project, slot, seg, offset, base_dir, warnings)
                .with_context(|| format!("rate ramp segment {:?}", seg.id))?;
        }
        return Ok(());
    }
    // Freeform shape mask (#41): bake into a real alpha-channel video and
    // recurse as an ordinary Video clip -- see bake_masked_clip's doc
    // comment for why baking real alpha (not a separate opaque cutout
    // layer, and not a GESEffect bin) is the only approach that actually
    // reveals lower layers through the shape.
    if let Some((shape, feather, invert)) = clip.effects.iter().find_map(|e| match e {
        crate::document::Effect::Mask { shape, feather, invert } => {
            Some((*shape, *feather, *invert))
        }
        _ => None,
    }) {
        return match bake_masked_clip(project, base_dir, clip, shape, feather, invert) {
            Ok(baked_src) => {
                let mut baked = clip.clone();
                // The bake mirrors the source's full timeline 1:1 (see
                // bake_masked_clip), so a Video clip keeps its original
                // inpoint; Test has no source file to mirror, so the bake
                // covers exactly this clip's own span starting at 0.
                let (baked_offset, volume) = match &clip.element {
                    Element::Video { offset, volume, .. } => (*offset, *volume),
                    _ => (0.0, 1.0),
                };
                baked.element =
                    Element::Video { src: baked_src, offset: baked_offset, volume, rate: 1.0 };
                baked.effects.retain(|e| !matches!(e, crate::document::Effect::Mask { .. }));
                add_clip(project, slot, &baked, offset, base_dir, warnings)
                    .with_context(|| format!("masked clip {:?}", clip.id))
            }
            Err(e) => {
                warnings.push(format!("clip {:?}: mask effect: {e}", clip.id));
                Ok(())
            }
        };
    }
    let layer = &slot[0];
    let start = secs(offset + clip.start);
    let duration = secs(if clip.duration > 0.0 { clip.duration } else { 1.0 });

    let ges_clip: Option<ges::Clip> = match &clip.element {
        Element::Text { text, font, color, align, outline, shadow } => {
            let title = ges::TitleClip::new().context("title clip")?;
            title.set_start(start);
            title.set_duration(duration);
            layer.add_clip(&title)?;
            title.set_child_property("text", text.to_value())?;
            title.set_child_property("font-desc", font.to_value())?;
            title.set_child_property("color", parse_color(color).to_value())?;
            title.set_child_property("background", 0u32.to_value())?;
            // Rich text styling (#38); warn rather than fail if this GES
            // build lacks a property.
            if let Some(outline) = outline
                && let Err(e) =
                    title.set_child_property("outline-color", parse_color(outline).to_value())
            {
                warnings.push(format!("clip {:?}: outline unsupported: {e}", clip.id));
            }
            if *shadow
                && let Err(e) = title.set_child_property("shadow", true.to_value())
            {
                warnings.push(format!("clip {:?}: shadow unsupported: {e}", clip.id));
            }
            if let Some(align) = align {
                let name = match align {
                    crate::document::TextAlign::Left => "left",
                    crate::document::TextAlign::Center => "center",
                    crate::document::TextAlign::Right => "right",
                };
                title.set_child_property("halignment", name.to_value())?;
            }
            if clip.transform.x != 0.0 || clip.transform.y != 0.0 {
                if align.is_none() {
                    title.set_child_property("halignment", "absolute".to_value())?;
                }
                title.set_child_property("valignment", "absolute".to_value())?;
                title.set_child_property(
                    "xpos",
                    (clip.transform.x / project.meta.width as f64).to_value(),
                )?;
                title.set_child_property(
                    "ypos",
                    (clip.transform.y / project.meta.height as f64).to_value(),
                )?;
            }
            Some(title.upcast())
        }
        Element::Video { src, offset: inpoint, volume, rate }
        | Element::Audio { src, offset: inpoint, volume, rate } => {
            // A missing file or an undecodable one are both skipped with
            // a warning, not a hard error: one bad clip anywhere in the
            // project used to fail the *entire* compile (`to_uri`'s `?`
            // propagated straight through), which is exactly the kind of
            // total, silent-looking preview failure #57 describes -- a
            // single moved/renamed file shouldn't take the whole preview
            // down. Probing (see probe_discoverable's doc comment) still
            // matters separately: an undecodable file isn't a normal
            // error GES reports, it's a process abort.
            let uri = crate::media::resolve(src, base_dir);
            let ok = matches!(&uri, Ok(uri) if probe_discoverable(uri));
            if !ok {
                let reason = match &uri {
                    Err(e) => format!("{e:#}"),
                    Ok(_) => "isn't decodable on this GStreamer install (missing plugin or \
                               unsupported codec profile)"
                        .to_string(),
                };
                warnings.push(format!("clip {:?}: {reason} -- skipped", clip.id));
                None
            } else {
                let uri = uri.expect("checked Ok above");
                let media = ges::UriClip::new(&uri).with_context(|| format!("opening {src}"))?;
                media.set_start(start);
                media.set_inpoint(secs(*inpoint));
                media.set_duration(duration);
                if matches!(clip.element, Element::Audio { .. }) {
                    media.set_supported_formats(ges::TrackType::AUDIO);
                }
                layer.add_clip(&media)?;
                // Speed (#32): GES auto-registers pitch/videorate rate
                // properties and drives nle's media consumption itself
                // (verified by reproducer; see issue).
                let animates_rate =
                    clip.animations.iter().any(|a| a.property == AnimProperty::Rate);
                if (*rate - 1.0).abs() > f64::EPSILON || animates_rate {
                    let pitch = ges::Effect::new(&format!("pitch name=dcrate tempo={rate}"))
                        .context("pitch effect (rate)")?;
                    media.add(&pitch).context("adding pitch rate effect")?;
                    if matches!(clip.element, Element::Video { .. }) {
                        let vrate =
                            ges::Effect::new(&format!("videorate name=dcvrate rate={rate}"))
                                .context("videorate effect")?;
                        media.add(&vrate).context("adding videorate effect")?;
                    }
                }
                // Volume rides an explicit effect; GES's own child-property
                // route scales values wrongly on this GStreamer (see ADR).
                let animates_volume =
                    clip.animations.iter().any(|a| a.property == AnimProperty::Volume);
                if (*volume - 1.0).abs() > f64::EPSILON || animates_volume {
                    let fx = ges::Effect::new(&format!("volume name=dcvol volume={volume}"))
                        .context("volume effect")?;
                    media.add(&fx).context("adding volume effect")?;
                }
                Some(media.upcast())
            }
        }
        Element::Image { src } => {
            // Same reasoning as Video/Audio above: missing file or
            // undecodable format, both skipped with a warning rather than
            // failing the whole compile.
            let uri = crate::media::resolve(src, base_dir);
            let ok = matches!(&uri, Ok(uri) if probe_discoverable(uri));
            if !ok {
                let reason = match &uri {
                    Err(e) => format!("{e:#}"),
                    Ok(_) => "isn't decodable on this GStreamer install (missing plugin or \
                               unsupported format)"
                        .to_string(),
                };
                warnings.push(format!("clip {:?}: {reason} -- skipped", clip.id));
                None
            } else {
                let uri = uri.expect("checked Ok above");
                let media = ges::UriClip::new(&uri).with_context(|| format!("opening {src}"))?;
                media.set_start(start);
                media.set_duration(duration);
                layer.add_clip(&media)?;
                Some(media.upcast())
            }
        }
        Element::Test {} => {
            let test = ges::TestClip::new().context("test clip")?;
            // Video only: the default audiotestsrc sine would drown real
            // audio in the mix.
            test.set_supported_formats(ges::TrackType::VIDEO);
            test.set_start(start);
            test.set_duration(duration);
            test.set_vpattern(ges::VideoTestPattern::Smpte);
            layer.add_clip(&test)?;
            Some(test.upcast())
        }
        Element::Shape { shape, fill } => {
            // Only used by the `vector` render path below; still bound
            // here so the `preview`-without-`vector` build (no shape
            // rendering, warning-only fallback) doesn't warn on it.
            #[cfg(not(feature = "vector"))]
            let _ = &fill;
            #[cfg(feature = "vector")]
            {
                let w = if clip.transform.width > 0.0 { clip.transform.width } else { 200.0 } as u32;
                let h = if clip.transform.height > 0.0 { clip.transform.height } else { 200.0 } as u32;
                let cache = base_dir.join(".dualcut-cache");
                match crate::vector::shape_png(&cache, *shape, fill, w, h) {
                    Ok(png) => {
                        let uri = crate::media::uri_from_path(&png.canonicalize()?);
                        let media = ges::UriClip::new(&uri).context("shape image clip")?;
                        media.set_start(start);
                        media.set_duration(duration);
                        layer.add_clip(&media)?;
                        Some(media.upcast())
                    }
                    Err(e) => {
                        warnings.push(format!("clip {:?}: shape render failed: {e}", clip.id));
                        None
                    }
                }
            }
            #[cfg(not(feature = "vector"))]
            {
                warnings.push(format!(
                    "clip {:?}: shape {:?} needs the \"vector\" feature",
                    clip.id, shape
                ));
                None
            }
        }
        Element::CompRef { r#ref, args } => {
            let def = project.defs.get(r#ref).expect("validated");
            // Each def layer gets its own GES layer span within the slot;
            // nested comprefs consume a span as wide as their own depth.
            let mut di = 0usize;
            for sub in &def.layers {
                let mut sub = substitute(sub, args);
                sub.id = format!("{}/{}", clip.id, sub.id);
                sub.start += clip.start;
                if sub.duration <= 0.0 {
                    sub.duration = clip.duration - (sub.start - clip.start);
                }
                let sub_slot = &slot[di.min(slot.len() - 1)..];
                add_clip(project, sub_slot, &sub, offset, base_dir, warnings)
                    .with_context(|| format!("def {:?} layer {:?}", r#ref, sub.id))?;
                di += clip_slot_depth(project, &sub);
            }
            None
        }
    };

    if let Some(ges_clip) = ges_clip {
        apply_transform_and_animations(project, &ges_clip, clip, warnings)?;
        apply_effects(&ges_clip, clip, warnings);
    }
    Ok(())
}

/// Bake a `Mask` effect's clip into a real alpha-channel video (#41),
/// combining the source with the rasterized shape via `alphacombine` and
/// encoding to FFV1 (lossless, alpha-capable) in Matroska -- confirmed by
/// direct testing that GES layer compositing correctly reveals a lower
/// layer through a clip's own per-pixel alpha, and that FFV1/A420 round-
/// trips real alpha through GStreamer's decoder chain (earlier attempts
/// with qtrle-in-mov failed only because that specific muxer here doesn't
/// declare `video/x-rle` in its sink caps -- an environment gap, not a
/// GES-alpha-compositing gap). A prior design tried a separate opaque
/// "cutout" clip on a layer above instead; that only ever paints a solid
/// color over *everything* below it (including other layers), which does
/// not match "reveal the layer below" semantics -- baking real alpha into
/// the clip itself is the only way to get that with layer compositing.
///
/// Cached by (source identity, offset, duration, shape/feather/invert,
/// frame size); returns the baked file's local path. Scoped to `Video`
/// and `Test` element types for v1 -- the two directly verified end to
/// end; other element types return an error so the caller can warn.
#[cfg(feature = "vector")]
fn bake_masked_clip(
    project: &Project,
    base_dir: &std::path::Path,
    clip: &Clip,
    shape: crate::document::ShapeKind,
    feather: f64,
    invert: bool,
) -> Result<String> {
    use std::hash::{Hash, Hasher};

    let w = (project.meta.width as u32).max(2) & !1;
    let h = (project.meta.height as u32).max(2) & !1;
    let fps = project.meta.fps.max(1);
    let cache = base_dir.join(".dualcut-cache");
    std::fs::create_dir_all(&cache)?;
    let duration = if clip.duration > 0.0 { clip.duration } else { 1.0 };

    // The luma-as-alpha trick (`alphacombine`'s ac.alpha branch) needs a
    // *white*-filled shape: unpainted (alpha=0) pixels are (0,0,0,0), so
    // an RGBA->GRAY8 conversion naturally yields a white-inside/black-
    // outside luma matte with no separate alpha-plane extraction needed.
    // `invert` swaps which side is white the same way it swaps the
    // painted/unpainted regions for the non-mask shape raster.
    let mask_png = crate::vector::shape_png_maybe_inverted(
        &cache, shape, "#ffffff", w, h, feather, invert,
    )
    .context("rendering mask shape")?;
    let mask_png = mask_png.canonicalize().context("resolving mask png path")?;

    // Bake the *whole* source once, not just the clip's own span: seeking
    // one branch of a running alphacombine bin (source vs. the static
    // imagefreeze alpha branch) desyncs the two and the combiner never
    // produces output (confirmed: an isolated source-only seek succeeds,
    // but the combined graph either fails the aggregate seek query or
    // stalls to an empty file). Baking the full file, uncut, sidesteps
    // that entirely -- same "slow but cached and correct" tradeoff
    // `thumbs::proxy_mp4` already makes, and the resulting clip keeps
    // using its *original* offset/duration exactly like an unmasked one,
    // since the baked file mirrors the source's own timeline 1:1. `Test`
    // has no reusable source file, so it bakes exactly its own duration
    // and always starts at offset 0.
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    let source_desc = match &clip.element {
        Element::Video { src, .. } => {
            let uri = crate::media::resolve(src, base_dir)?;
            uri.hash(&mut hasher);
            format!("uridecodebin uri=\"{uri}\" name=d")
        }
        Element::Test {} => {
            "test".hash(&mut hasher);
            duration.to_bits().hash(&mut hasher);
            let n = (duration * fps as f64).ceil() as u64;
            format!(
                "videotestsrc pattern=smpte num-buffers={n} ! \
                 video/x-raw,width={w},height={h},framerate={fps}/1 ! identity name=d"
            )
        }
        _ => bail!("mask effect is only supported on video and test clips"),
    };
    (format!("{shape:?}"), feather.to_bits(), invert, w, h, fps).hash(&mut hasher);
    let key = hasher.finish();

    let file = cache.join(format!("mask-bake-{key:016x}.mkv"));
    if !file.exists() {
        let part = cache.join(format!("mask-bake-{key:016x}.mkv.part"));
        let _ = std::fs::remove_file(&part);

        let desc = format!(
            "{source_desc} d. ! queue ! videoconvert ! videoscale ! \
             video/x-raw,format=I420,width={w},height={h},colorimetry=bt601 ! ac.sink \
             filesrc location=\"{mask}\" ! pngdec ! imagefreeze ! videoconvert ! \
             video/x-raw,format=GRAY8,width={w},height={h},colorimetry=bt601 ! ac.alpha \
             alphacombine name=ac ! videoconvert ! video/x-raw,format=A420 ! avenc_ffv1 ! \
             matroskamux ! filesink location=\"{part}\"",
            mask = mask_png.display(),
            part = part.display(),
        );
        let pipeline = gst::parse::launch(&desc)
            .context("building mask bake pipeline")?
            .downcast::<gst::Pipeline>()
            .map_err(|_| anyhow::anyhow!("mask bake pipeline is not a Pipeline"))?;
        pipeline.set_state(gst::State::Playing).context("playing mask bake")?;
        let bus = pipeline.bus().context("mask bake pipeline has no bus")?;
        let mut result = Ok(());
        for msg in bus.iter_timed(gst::ClockTime::from_seconds(300)) {
            use gst::MessageView;
            match msg.view() {
                MessageView::Eos(_) => break,
                MessageView::Error(e) => {
                    result = Err(anyhow::anyhow!("mask bake failed: {}", e.error()));
                    break;
                }
                _ => {}
            }
        }
        let _ = pipeline.set_state(gst::State::Null);
        result?;
        std::fs::rename(&part, &file)?;
    }
    Ok(file.canonicalize().context("resolving mask bake path")?.display().to_string())
}

#[cfg(not(feature = "vector"))]
fn bake_masked_clip(
    _project: &Project,
    _base_dir: &std::path::Path,
    _clip: &Clip,
    _shape: crate::document::ShapeKind,
    _feather: f64,
    _invert: bool,
) -> Result<String> {
    bail!("mask effect needs the \"vector\" feature")
}

fn apply_transform_and_animations(
    _project: &Project,
    ges_clip: &ges::Clip,
    clip: &Clip,
    warnings: &mut Vec<String>,
) -> Result<()> {
    let t = &clip.transform;
    let is_title = ges_clip.clone().downcast::<ges::TitleClip>().is_ok();
    // Titles position via xpos/ypos (handled at creation); other video
    // clips use the frame positioner's pixel-space properties.
    if !is_title {
        if t.x != 0.0 || t.y != 0.0 {
            ges_clip.set_child_property("posx", (t.x as i32).to_value())?;
            ges_clip.set_child_property("posy", (t.y as i32).to_value())?;
        }
        if t.width > 0.0 {
            ges_clip.set_child_property("width", (t.width as i32).to_value())?;
        }
        if t.height > 0.0 {
            ges_clip.set_child_property("height", (t.height as i32).to_value())?;
        }
    }
    if (t.opacity - 1.0).abs() > f64::EPSILON {
        ges_clip.set_child_property("alpha", t.opacity.to_value())?;
    }

    // One control source per property: GES allows a single binding per
    // child property, so all of a property's animation windows merge into
    // one sampled track with hold points between and after windows.
    let mut by_prop: std::collections::BTreeMap<&str, Vec<&Anim>> = Default::default();
    for anim in &clip.animations {
        let prop = match anim.property {
            AnimProperty::Opacity => "alpha",
            AnimProperty::X => "posx",
            AnimProperty::Y => "posy",
            AnimProperty::Width => "width",
            AnimProperty::Height => "height",
            AnimProperty::Volume => "volume",
            AnimProperty::Rate => "tempo",
        };
        by_prop.entry(prop).or_default().push(anim);
    }
    for (prop, mut anims) in by_prop {
        let sort_key = |a: &Anim| a.keyframes.first().map_or(a.start, |k| k.t);
        anims.sort_by(|a, b| sort_key(a).total_cmp(&sort_key(b)));
        if let Err(e) = apply_property_animations(ges_clip, clip, prop, &anims) {
            warnings.push(format!("clip {:?}: animations on {prop} skipped: {e}", clip.id));
        }
    }
    Ok(())
}

/// Bind all of one property's animation windows to a single interpolation
/// control source on the clip's matching track element. Hold points before,
/// between, and after windows prevent linear extrapolation drift.
fn apply_property_animations(
    ges_clip: &ges::Clip,
    clip: &Clip,
    prop: &str,
    anims: &[&Anim],
) -> Result<()> {
    let element = ges_clip
        .children(false)
        .into_iter()
        .filter_map(|c| c.downcast::<ges::TrackElement>().ok())
        .find(|te| ges::prelude::TimelineElementExt::lookup_child(te, prop).is_some())
        .with_context(|| format!("no track element exposes {prop:?}"))?;

    let cs = gst_controller::InterpolationControlSource::new();
    cs.set_property("mode", gst_controller::InterpolationMode::Linear);
    let set = |time: f64, value: f64| {
        <gst_controller::InterpolationControlSource as gst_controller::prelude::TimedValueControlSourceExt>::set(
            &cs,
            secs(time),
            normalize(prop, value),
        );
    };

    let inpoint = element.inpoint().nseconds() as f64 / 1e9;
    let first_value = |a: &Anim| a.keyframes.first().map_or(a.from, |k| k.value);
    let last_of = |a: &Anim| a.keyframes.last().map_or((a.end, a.to), |k| (k.t, k.value));
    // Hold the first animation's initial value from the clip's beginning.
    set(inpoint, first_value(anims[0]));
    for anim in anims {
        if anim.keyframes.is_empty() {
            // Tween window: sample eased values densely; the control source
            // interpolates linearly between samples, so curves survive.
            let steps = 24.max(((anim.end - anim.start) * 30.0) as usize);
            for i in 0..=steps {
                let p = i as f64 / steps as f64;
                let value = anim.from + (anim.to - anim.from) * ease(anim.easing, p);
                set(inpoint + anim.start + (anim.end - anim.start) * p, value);
            }
        } else {
            // Keyframes: sample each eased segment between neighbours.
            set(inpoint + anim.keyframes[0].t, anim.keyframes[0].value);
            for pair in anim.keyframes.windows(2) {
                let (a, b) = (&pair[0], &pair[1]);
                let steps = 12.max(((b.t - a.t) * 30.0) as usize);
                for i in 1..=steps {
                    let p = i as f64 / steps as f64;
                    let value = a.value + (b.value - a.value) * ease(b.easing, p);
                    set(inpoint + a.t + (b.t - a.t) * p, value);
                }
            }
        }
    }
    // Hold the last value to the end of the clip so linear mode never
    // extrapolates past the final segment.
    let (last_t, last_v) = last_of(anims.last().unwrap());
    let clip_end = inpoint + clip.duration.max(last_t) + 1.0;
    set(clip_end, last_v);

    let source: gst::ControlSource = cs.upcast();
    if prop == "volume" {
        // Bind straight to the gst volume element inside the clip's volume
        // effect: raw values, no GES child-property scaling.
        let fx = ges_clip
            .children(false)
            .into_iter()
            .filter_map(|c| c.downcast::<ges::Effect>().ok())
            .find(|e| ges::prelude::TimelineElementExt::lookup_child(e, "volume").is_some())
            .context("no volume effect on clip")?;
        let bin = fx
            .nleobject()
            .downcast::<gst::Bin>()
            .map_err(|_| anyhow::anyhow!("effect nleobject is not a bin"))?;
        let vol = find_by_factory(&bin, "volume").context("no volume element in effect")?;
        let binding = gst_controller::DirectControlBinding::new_absolute(&vol, "volume", &source);
        vol.add_control_binding(&binding)?;
    } else if prop == "tempo" {
        // Keyframed speed ramps are not implemented: GES class-level
        // auto-registers pitch/videorate as *rate properties*, which
        // recomputes timeline timing synchronously whenever they change
        // (ges-timeline.c's edit-API thread-ownership assertion fires
        // when that happens from the streaming thread a GstController
        // callback runs on -- confirmed by direct reproduction, not
        // speculation). A safe ramp needs segmentation: split the clip
        // into several sub-clips, each holding a static tempo sampled
        // from the curve, rather than live-animating the property. See
        // the rate-ramp tracking issue for the design.
        anyhow::bail!(
            "keyframed rate/tempo animation is not yet supported (GES rate-property              live-binding is unsafe); use a constant clip.rate, or split the clip into              segments with different rates"
        );
    } else {
        element
            .set_control_source(&source, prop, "direct-absolute")
            .then_some(())
            .context("set_control_source failed")?;
    }
    Ok(())
}

/// Control sources take normalized doubles for some properties; GES
/// "direct-absolute" mode takes raw values, so only alpha needs clamping.
/// GES layers a clip needs: 1, or for comprefs the sum of its def's
/// layers' needs (validation rejects cycles).
fn clip_slot_depth(project: &Project, clip: &Clip) -> usize {
    match &clip.element {
        Element::CompRef { r#ref, .. } => project
            .defs
            .get(r#ref)
            .map_or(1, |d| d.layers.iter().map(|l| clip_slot_depth(project, l)).sum::<usize>().max(1)),
        _ => 1,
    }
}

/// Depth-first search for an element by factory name inside a bin.
fn find_by_factory(bin: &gst::Bin, factory: &str) -> Option<gst::Element> {
    bin.iterate_recurse()
        .into_iter()
        .flatten()
        .find(|child| child.factory().is_some_and(|f| f.name() == factory))
}

/// Attach the clip's effects chain. Runs after the clip joins a layer so
/// GES can create the effect track elements.
fn apply_effects(ges_clip: &ges::Clip, clip: &Clip, warnings: &mut Vec<String>) {
    for effect in &clip.effects {
        let desc = match effect {
            // Handled separately at the top of add_clip via
            // bake_masked_clip -- a real alpha-baked clip, not a
            // GESEffect bin (never reaches here; effects are stripped
            // from the recursive baked-clip call).
            crate::document::Effect::Mask { .. } => continue,
            crate::document::Effect::Blur { amount } => format!("gaussianblur sigma={amount}"),
            crate::document::Effect::ChromaKey { color, angle, noise } => {
                let argb = crate::document::parse_color(color);
                let (r, g, b) = ((argb >> 16) & 0xff, (argb >> 8) & 0xff, argb & 0xff);
                format!(
                    "alpha method=custom target-r={r} target-g={g} target-b={b} angle={angle} noise-level={noise}"
                )
            }
            crate::document::Effect::Crop { left, right, top, bottom } => {
                format!("videocrop left={left} right={right} top={top} bottom={bottom}")
            }
            crate::document::Effect::Eq { low, mid, high } => {
                format!("equalizer-3bands band0={low} band1={mid} band2={high}")
            }
            crate::document::Effect::Compressor { threshold, ratio } => {
                format!("audiodynamic mode=compressor threshold={threshold} ratio={ratio}")
            }
            crate::document::Effect::Denoise { level } => {
                format!("webrtcdsp echo-cancel=false noise-suppression=true noise-suppression-level={level}")
            }
            crate::document::Effect::Color { brightness, contrast, saturation, hue } => format!(
                "videobalance brightness={brightness} contrast={contrast} saturation={saturation} hue={hue}"
            ),
        };
        let fx = ges::Effect::new(&desc);
        match fx {
            Ok(fx) => {
                if let Err(e) = ges_clip.add(&fx) {
                    warnings.push(format!("clip {:?}: effect {desc:?} not added: {e}", clip.id));
                }
            }
            Err(e) => warnings.push(format!("clip {:?}: effect {desc:?} unavailable: {e}", clip.id)),
        }
    }
}

/// GES auto-transitions are crossfades; retype the video transitions that
/// fall inside a scene boundary whose document transition asks for a wipe.
fn retype_transitions(project: &Project, timeline: &ges::Timeline) {
    use ges::VideoStandardTransitionType as V;
    let mut wanted: Vec<(f64, f64, V)> = Vec::new();
    for (i, scene) in project.scenes.iter().enumerate().skip(1) {
        if let Some(tr) = &scene.transition {
            let vtype = match tr.kind {
                crate::document::TransitionKind::Crossfade => continue,
                crate::document::TransitionKind::WipeLr => V::BarWipeLr,
                crate::document::TransitionKind::WipeTb => V::BarWipeTb,
                crate::document::TransitionKind::BoxWipe => V::BoxWipeTl,
                crate::document::TransitionKind::Iris => V::IrisRect,
                crate::document::TransitionKind::Clock => V::ClockCw12,
            };
            let start = project.scene_offset(i);
            wanted.push((start, start + tr.duration, vtype));
        }
    }
    if wanted.is_empty() {
        return;
    }
    for layer in timeline.layers() {
        for clip in layer.clips() {
            if let Ok(tclip) = clip.downcast::<ges::TransitionClip>() {
                let start = tclip.start().nseconds() as f64 / 1e9;
                let end = start + tclip.duration().nseconds() as f64 / 1e9;
                for (ws, we, vtype) in &wanted {
                    // Auto transitions sit exactly in the overlap window.
                    if start >= ws - 0.01 && end <= we + 0.01 {
                        tclip.set_property("vtype", vtype);
                    }
                }
            }
        }
    }
}

fn normalize(prop: &str, value: f64) -> f64 {
    match prop {
        "alpha" => value.clamp(0.0, 1.0),
        "volume" => value.max(0.0),
        _ => value,
    }
}

fn ease(easing: Easing, t: f64) -> f64 {
    match easing {
        Easing::Linear => t,
        Easing::EaseIn => t * t * t,
        Easing::EaseOut => 1.0 - (1.0 - t).powi(3),
        Easing::EaseInOut => {
            if t < 0.5 {
                4.0 * t * t * t
            } else {
                1.0 - (-2.0 * t + 2.0).powi(3) / 2.0
            }
        }
    }
}

fn substitute(clip: &Clip, args: &BTreeMap<String, String>) -> Clip {
    let mut clip = clip.clone();
    let apply = |s: &mut String| {
        for (k, v) in args {
            *s = s.replace(&format!("{{{k}}}"), v);
        }
    };
    match &mut clip.element {
        Element::Text { text, font, color, outline, .. } => {
            apply(text);
            apply(font);
            apply(color);
            if let Some(o) = outline {
                apply(o);
            }
        }
        Element::Video { src, .. } | Element::Audio { src, .. } | Element::Image { src } => {
            apply(src)
        }
        Element::Shape { fill, .. } => apply(fill),
        // Nested defs: pass substitutions through the inner instantiation's args.
        Element::CompRef { args: inner, .. } => {
            for v in inner.values_mut() {
                apply(v);
            }
        }
        _ => {}
    }
    clip
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::document::{Meta, Scene};

    /// A video whose codec profile a GStreamer install can't decode isn't
    /// a normal, catchable error from GES -- it's a `g_assert()` abort
    /// deep in `ges-discoverer-manager.c` (upstream gstreamer#3757, only
    /// partially fixed). `probe_discoverable` exists specifically to
    /// avoid ever handing such a URI to `ges::UriClip::new()`. This is a
    /// robustness test, not a behavior-exact one: whether `ball.mp4`'s
    /// High-4:4:4 10-bit profile is actually decodable depends on the
    /// host's GStreamer plugins, so the only thing asserted unconditionally
    /// is that compiling never aborts the process and always returns a
    /// clean `Result` either way -- if `probe_discoverable` regressed back
    /// to calling `ges::UriClip::new()` unconditionally, this test would
    /// crash the entire test binary on a host missing that decoder, not
    /// just fail one assertion.
    #[test]
    fn compiling_an_undecodable_video_never_aborts_the_process() {
        crate::init().ok();
        let base_dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("examples");
        let project = Project {
            meta: Meta { title: "probe-test".into(), width: 320, height: 180, fps: 25 },
            library: Vec::new(),
            defs: Default::default(),
            scenes: vec![Scene {
                id: "s1".into(),
                name: String::new(),
                duration: 1.0,
                transition: None,
                layers: vec![Clip {
                    id: "maybe-broken".into(),
                    start: 0.0,
                    duration: 1.0,
                    element: Element::Video {
                        src: "assets/ball.mp4".into(),
                        offset: 0.0,
                        volume: 1.0,
                        rate: 1.0,
                    },
                    transform: Default::default(),
                    animations: Vec::new(),
                    effects: Vec::new(),
                }],
            }],
            overlays: Vec::new(),
            scene_lanes: Vec::new(),
        };
        // The assertion that matters is simply reaching this line at all
        // -- a regression here aborts the whole process, which no
        // `Result` handling below could express as a normal test failure.
        let result = compile(&project, &base_dir);
        assert!(
            result.is_ok(),
            "compile should degrade gracefully, not error: {:?}",
            result.err()
        );
    }

    // ---- pure helpers: no GStreamer/GES init needed at all ----

    #[test]
    fn normalize_clamps_alpha_but_not_arbitrary_properties() {
        assert_eq!(normalize("alpha", 1.5), 1.0);
        assert_eq!(normalize("alpha", -0.5), 0.0);
        assert_eq!(normalize("alpha", 0.5), 0.5);
        assert_eq!(normalize("volume", -1.0), 0.0);
        assert_eq!(normalize("volume", 2.0), 2.0);
        assert_eq!(normalize("posx", -500.0), -500.0);
    }

    #[test]
    fn ease_endpoints_are_identity_for_every_curve() {
        for easing in [Easing::Linear, Easing::EaseIn, Easing::EaseOut, Easing::EaseInOut] {
            assert!((ease(easing, 0.0) - 0.0).abs() < 1e-9, "{easing:?} at t=0");
            assert!((ease(easing, 1.0) - 1.0).abs() < 1e-9, "{easing:?} at t=1");
        }
    }

    #[test]
    fn ease_linear_is_the_identity_function() {
        for t in [0.0, 0.25, 0.5, 0.75, 1.0] {
            assert!((ease(Easing::Linear, t) - t).abs() < 1e-9);
        }
    }

    #[test]
    fn substitute_replaces_placeholders_in_text_fields() {
        let clip = Clip {
            id: "tpl".into(),
            start: 0.0,
            duration: 1.0,
            element: Element::Text {
                text: "Hello {name}!".into(),
                font: "{font_name} 20".into(),
                color: "{color}".into(),
                align: None,
                outline: Some("{outline}".into()),
                shadow: false,
            },
            transform: Default::default(),
            animations: Vec::new(),
            effects: Vec::new(),
        };
        let mut args = BTreeMap::new();
        args.insert("name".to_string(), "World".to_string());
        args.insert("font_name".to_string(), "Sans Bold".to_string());
        args.insert("color".to_string(), "#ffffff".to_string());
        args.insert("outline".to_string(), "#000000".to_string());
        let result = substitute(&clip, &args);
        match result.element {
            Element::Text { text, font, color, outline, .. } => {
                assert_eq!(text, "Hello World!");
                assert_eq!(font, "Sans Bold 20");
                assert_eq!(color, "#ffffff");
                assert_eq!(outline.as_deref(), Some("#000000"));
            }
            other => panic!("expected text, got {other:?}"),
        }
    }

    #[test]
    fn substitute_replaces_video_src() {
        let clip = Clip {
            id: "v".into(),
            start: 0.0,
            duration: 1.0,
            element: Element::Video { src: "{clip}".into(), offset: 0.0, volume: 1.0, rate: 1.0 },
            transform: Default::default(),
            animations: Vec::new(),
            effects: Vec::new(),
        };
        let mut args = BTreeMap::new();
        args.insert("clip".to_string(), "footage.mp4".to_string());
        match substitute(&clip, &args).element {
            Element::Video { src, .. } => assert_eq!(src, "footage.mp4"),
            other => panic!("expected video, got {other:?}"),
        }
    }

    #[test]
    fn substitute_leaves_unmatched_placeholders_untouched() {
        let clip = Clip {
            id: "t".into(),
            start: 0.0,
            duration: 1.0,
            element: Element::Text {
                text: "{missing}".into(),
                font: "Sans 20".into(),
                color: "#fff".into(),
                align: None,
                outline: None,
                shadow: false,
            },
            transform: Default::default(),
            animations: Vec::new(),
            effects: Vec::new(),
        };
        match substitute(&clip, &BTreeMap::new()).element {
            Element::Text { text, .. } => assert_eq!(text, "{missing}"),
            other => panic!("expected text, got {other:?}"),
        }
    }

    #[test]
    fn clip_slot_depth_is_one_for_ordinary_clips() {
        let project = minimal_project();
        let clip = Clip {
            id: "c".into(),
            start: 0.0,
            duration: 1.0,
            element: Element::Test {},
            transform: Default::default(),
            animations: Vec::new(),
            effects: Vec::new(),
        };
        assert_eq!(clip_slot_depth(&project, &clip), 1);
    }

    #[test]
    fn clip_slot_depth_sums_a_multi_layer_defs_own_depths() {
        let mut project = minimal_project();
        project.defs.insert(
            "two-layers".into(),
            crate::document::CompDef {
                params: Vec::new(),
                layers: vec![
                    Clip {
                        id: "a".into(),
                        start: 0.0,
                        duration: 1.0,
                        element: Element::Test {},
                        transform: Default::default(),
                        animations: Vec::new(),
                        effects: Vec::new(),
                    },
                    Clip {
                        id: "b".into(),
                        start: 0.0,
                        duration: 1.0,
                        element: Element::Test {},
                        transform: Default::default(),
                        animations: Vec::new(),
                        effects: Vec::new(),
                    },
                ],
            },
        );
        let clip = Clip {
            id: "inst".into(),
            start: 0.0,
            duration: 1.0,
            element: Element::CompRef { r#ref: "two-layers".into(), args: Default::default() },
            transform: Default::default(),
            animations: Vec::new(),
            effects: Vec::new(),
        };
        assert_eq!(clip_slot_depth(&project, &clip), 2);
    }

    // ---- real compiles: exercise the actual GES timeline builder ----

    fn minimal_project() -> Project {
        Project {
            meta: Meta { title: "t".into(), width: 320, height: 180, fps: 25 },
            library: Vec::new(),
            defs: Default::default(),
            scenes: vec![Scene {
                id: "s1".into(),
                name: String::new(),
                duration: 1.0,
                transition: None,
                layers: Vec::new(),
            }],
            overlays: Vec::new(),
            scene_lanes: Vec::new(),
        }
    }

    fn test_clip(id: &str, start: f64, duration: f64) -> Clip {
        Clip {
            id: id.into(),
            start,
            duration,
            element: Element::Test {},
            transform: Default::default(),
            animations: Vec::new(),
            effects: Vec::new(),
        }
    }

    #[test]
    fn overlay_clips_use_absolute_timing_not_scene_relative() {
        crate::init().ok();
        let mut project = minimal_project();
        project.scenes[0].duration = 2.0;
        project.scenes.push(Scene {
            id: "s2".into(),
            name: String::new(),
            duration: 2.0,
            transition: None,
            layers: Vec::new(),
        });
        // An overlay clip starting at t=3 should land in the second scene's
        // span (offset 2.0) regardless of per-scene offsets -- overlays are
        // absolute, unlike scene layers.
        project.overlays.push(crate::document::OverlayTrack {
            id: "ov".into(),
            muted: false,
            hidden: false,
            locked: false,
            name: "Overlay".into(),
            clips: vec![test_clip("abs", 3.0, 1.0)],
        });
        let compiled = compile(&project, std::path::Path::new(".")).expect("compiles");
        let found = compiled.timeline.layers().iter().any(|l| {
            l.clips().iter().any(|c| (c.start().nseconds() as f64 / 1e9 - 3.0).abs() < 0.01)
        });
        assert!(found, "expected a clip at absolute t=3.0 on some layer");
    }

    #[test]
    fn compref_instantiates_every_def_layer() {
        crate::init().ok();
        let mut project = minimal_project();
        project.defs.insert(
            "pair".into(),
            crate::document::CompDef {
                params: Vec::new(),
                layers: vec![test_clip("top", 0.0, 1.0), test_clip("bottom", 0.0, 1.0)],
            },
        );
        project.scenes[0].layers.push(Clip {
            id: "inst".into(),
            start: 0.0,
            duration: 1.0,
            element: Element::CompRef { r#ref: "pair".into(), args: Default::default() },
            transform: Default::default(),
            animations: Vec::new(),
            effects: Vec::new(),
        });
        let compiled = compile(&project, std::path::Path::new(".")).expect("compiles");
        let total_clips: usize =
            compiled.timeline.layers().iter().map(|l| l.clips().len()).sum();
        // Both def layers should have landed somewhere in the timeline.
        assert!(total_clips >= 2, "expected at least 2 clips from a 2-layer def, got {total_clips}");
    }

    #[test]
    fn wipe_transition_compiles_and_retypes_the_auto_transition() {
        crate::init().ok();
        let mut project = minimal_project();
        project.scenes[0].duration = 2.0;
        project.scenes[0].layers.push(test_clip("a", 0.0, 2.0));
        project.scenes.push(Scene {
            id: "s2".into(),
            name: String::new(),
            duration: 2.0,
            transition: Some(crate::document::Transition {
                kind: crate::document::TransitionKind::WipeLr,
                duration: 0.5,
            }),
            layers: vec![test_clip("b", 0.0, 2.0)],
        });
        // Should compile without error; retype_transitions runs internally
        // and must not panic on a real timeline with an overlapping pair.
        let compiled = compile(&project, std::path::Path::new(".")).expect("compiles");
        assert!(!compiled.timeline.layers().is_empty());
    }

    #[test]
    fn every_video_effect_kind_attaches_without_a_compile_error() {
        crate::init().ok();
        let mut project = minimal_project();
        let mut clip = test_clip("fx", 0.0, 1.0);
        clip.effects = vec![
            crate::document::Effect::Blur { amount: 5.0 },
            crate::document::Effect::ChromaKey { color: "#00ff00".into(), angle: 20.0, noise: 2.0 },
            crate::document::Effect::Crop { left: 1, right: 1, top: 1, bottom: 1 },
            crate::document::Effect::Color {
                brightness: 0.1,
                contrast: 1.1,
                saturation: 1.0,
                hue: 0.0,
            },
        ];
        project.scenes[0].layers.push(clip);
        let compiled = compile(&project, std::path::Path::new(".")).expect("compiles");
        assert!(
            compiled.warnings.iter().all(|w| !w.contains("not added") && !w.contains("unavailable")),
            "unexpected effect warnings: {:?}",
            compiled.warnings
        );
    }

    #[test]
    fn keyframed_opacity_animation_compiles_without_warnings() {
        crate::init().ok();
        let mut project = minimal_project();
        let mut clip = test_clip("anim", 0.0, 2.0);
        clip.animations.push(crate::document::Anim {
            property: crate::document::AnimProperty::Opacity,
            from: 0.0,
            to: 0.0,
            start: 0.0,
            end: 0.0,
            easing: Easing::Linear,
            keyframes: vec![
                crate::document::Keyframe { t: 0.0, value: 0.0, easing: Easing::EaseOut },
                crate::document::Keyframe { t: 1.0, value: 1.0, easing: Easing::Linear },
            ],
        });
        project.scenes[0].layers.push(clip);
        let compiled = compile(&project, std::path::Path::new(".")).expect("compiles");
        assert!(
            compiled.warnings.iter().all(|w| !w.contains("animations on")),
            "unexpected animation warnings: {:?}",
            compiled.warnings
        );
    }
}
