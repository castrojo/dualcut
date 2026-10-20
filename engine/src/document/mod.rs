//! Document model v2: serializable schema, validation, traversal, and edit operations.
//!
//! The public API remains re-exported from this module while each responsibility
//! lives in its own implementation module.

mod model;
mod ops;
mod rate_ramp;
mod traversal;
mod validation;

pub use model::*;
pub use ops::*;
pub use rate_ramp::expand_rate_ramp;
pub use traversal::{effective_duration, find_clip, find_clip_mut, remove_clip};
#[cfg(test)]
pub(crate) use model::{default_color, default_font};

#[cfg(test)]
use std::collections::BTreeMap;
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
