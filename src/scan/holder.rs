// SPDX-License-Identifier: MIT
//! Pure planning for one acquisition of selected frames on a continuous strip.
use super::{HolderSelection, PassKind, PlannedPass, ScanPlan};
use crate::{
    Capabilities, Error, Result, ScanSettings,
    capabilities::{FrameFormat, Holder},
};
use serde::Serialize;

/// One independently validated frame, before continuous-strip grouping.
#[derive(Clone, Debug, Serialize)]
pub struct HolderFramePlan {
    pub selection: HolderSelection,
    pub plan: ScanPlan,
}

/// An acquisition and the original frame indices whose pixels it contains.
/// Groups and their members retain first-selected order. For singleton groups,
/// the original plan is preserved, including infrared or thumbnail passes.
#[derive(Clone, Debug, Serialize)]
pub struct HolderBatchPlan {
    pub strip: Option<u32>,
    pub frame_indices: Vec<usize>,
    pub settings: ScanSettings,
    pub plan: ScanPlan,
}

struct Group {
    holder: Holder,
    frame_format: Option<FrameFormat>,
    strip: Option<u32>,
    parameter_key: Option<[u8; 64]>,
    frame_indices: Vec<usize>,
}

/// Group compatible single RGB or grayscale captures by registered continuous strip.
///
/// The union contains the exact effective pixel rectangles from the individual
/// plans, including their rounding and width alignment. Cropping those pixels
/// requires no resizing. Unselected intervening areas are acquired as part of
/// the bounding rectangle. Multipass and infrared-only jobs remain separate.
/// This function performs no I/O and validates every input before returning.
pub fn plan_holder_batches(
    frames: &[HolderFramePlan],
    caps: &Capabilities,
) -> Result<Vec<HolderBatchPlan>> {
    let model = caps.scanner_model()?;
    let mut groups: Vec<Group> = Vec::new();
    for (index, frame) in frames.iter().enumerate() {
        let layout = model
            .holder(frame.selection.holder)?
            .for_format(frame.selection.frame_format)?;
        let frame_format = frame.selection.frame_format.or(layout.default_format);
        let strip = layout.strip_for_frame(frame.selection.frame)?;
        if frame.plan.passes.is_empty() {
            return Err(Error::Invalid("Holder frame plan has no passes".into()));
        }
        for pass in &frame.plan.passes {
            frame.selection.validate(&pass.settings, model)?;
            pass.settings.validate(caps, pass.kind == PassKind::Ir)?;
            let channels = model
                .mode_for(pass.settings.mode, pass.kind == PassKind::Ir)?
                .channels;
            if !pass.kind.matches_mode(pass.settings.mode)
                || pass.pixels != pass.settings.pixels_for(model)?
                || pass.channels != channels
                || pass.expected_bytes
                    != expected_bytes(pass.pixels, channels, pass.settings.depth)?
            {
                return Err(Error::Invalid(
                    "Holder frame plan disagrees with its scan settings".into(),
                ));
            }
        }
        let parameter_key = if frame.plan.passes.len() == 1
            && matches!(frame.plan.passes[0].kind, PassKind::Rgb | PassKind::Gray)
            && strip.is_some()
        {
            let mut key = frame.plan.passes[0]
                .settings
                .parameters_with_capabilities(caps, false)?;
            key[8..24].fill(0);
            // Transfer line count derives from width, so it is geometry policy
            // too; the union will calculate its own block size.
            key[28] = 0;
            Some(key)
        } else {
            None
        };
        if let Some(group) = groups.iter_mut().find(|group| {
            parameter_key.is_some()
                && group.holder == frame.selection.holder
                && group.frame_format == frame_format
                && group.strip == strip
                && group.parameter_key == parameter_key
        }) {
            group.frame_indices.push(index);
        } else {
            groups.push(Group {
                holder: frame.selection.holder,
                frame_format,
                strip,
                parameter_key,
                frame_indices: vec![index],
            });
        }
    }
    groups
        .into_iter()
        .map(|group| {
            let first = &frames[group.frame_indices[0]].plan;
            let plan = if group.frame_indices.len() > 1 {
                let passes: Vec<_> = group
                    .frame_indices
                    .iter()
                    .map(|&index| &frames[index].plan.passes[0])
                    .collect();
                ScanPlan {
                    passes: vec![union_pass(&passes, caps)?],
                }
            } else {
                first.clone()
            };
            Ok(HolderBatchPlan {
                strip: group.strip,
                frame_indices: group.frame_indices,
                settings: plan.passes[0].settings.clone(),
                plan,
            })
        })
        .collect()
}

fn expected_bytes(pixels: [u32; 4], channels: u8, depth: u8) -> Result<u64> {
    u64::from(pixels[2])
        .checked_mul(u64::from(pixels[3]))
        .and_then(|size| size.checked_mul(u64::from(channels)))
        .and_then(|size| size.checked_mul(u64::from(depth / 8)))
        .ok_or_else(|| Error::Invalid("Holder strip capture size overflow".into()))
}

pub(crate) fn union_pass(passes: &[&PlannedPass], caps: &Capabilities) -> Result<PlannedPass> {
    let first = passes[0];
    let model = caps.scanner_model()?;
    let alignment = model.transfer.width_alignment;
    let mut left = u32::MAX;
    let mut top = u32::MAX;
    let mut right = 0;
    let mut bottom = 0;
    for pass in passes {
        let [x, y, width, height] = pass.pixels;
        left = left.min(x);
        top = top.min(y);
        right =
            right.max(x.checked_add(width).ok_or_else(|| {
                Error::Invalid("Holder strip horizontal endpoint overflow".into())
            })?);
        bottom = bottom.max(
            y.checked_add(height)
                .ok_or_else(|| Error::Invalid("Holder strip vertical endpoint overflow".into()))?,
        );
    }
    let width = (right - left)
        .div_ceil(alignment)
        .checked_mul(alignment)
        .ok_or_else(|| Error::Invalid("Holder strip width overflow".into()))?;
    let source_mm = caps.area_mm(first.settings.source);
    let available_width = (source_mm[0] * f64::from(first.settings.dpi) / 25.4 + 0.5).floor();
    if f64::from(width) > available_width || width > caps.max_width_pixels {
        return Err(Error::Invalid(
            "Aligned holder strip exceeds the scanner's source width".into(),
        ));
    }
    // Rounding a union width upward may require a few extra columns. Near the
    // source edge, acquire those columns on the left rather than outside it.
    left = left.min((available_width.min(f64::from(u32::MAX)) as u32) - width);
    let pixels = [left, top, width, bottom - top];
    if left.checked_add(width).is_none_or(|end| end < right)
        || top.checked_add(pixels[3]).is_none_or(|end| end < bottom)
    {
        return Err(Error::Invalid(
            "An in-bounds holder strip cannot contain every requested frame pixel".into(),
        ));
    }
    let mut settings = first.settings.clone();
    settings.rect_mm = exact_rectangle_mm(pixels, settings.dpi, source_mm);
    settings.validate(caps, false)?;
    if settings.pixels_for(model)? != pixels {
        return Err(Error::Invalid(
            "Holder strip millimetres do not preserve its exact pixel rectangle".into(),
        ));
    }
    Ok(PlannedPass {
        kind: first.kind,
        expected_bytes: expected_bytes(pixels, first.channels, settings.depth)?,
        settings,
        pixels,
        channels: first.channels,
    })
}

fn exact_rectangle_mm(pixels: [u32; 4], dpi: u32, source_mm: [f64; 2]) -> [f64; 4] {
    let mut rectangle = pixels.map(|pixel| f64::from(pixel) * 25.4 / f64::from(dpi));
    for axis in 0..2 {
        let excess = rectangle[axis] + rectangle[axis + 2] - source_mm[axis];
        if excess > 0.0 {
            // The rounded source boundary can be up to half a pixel beyond the
            // physical boundary. Move within the same rounding bins, without
            // changing any wire pixels or clipping a requested frame.
            let origin_adjustment = rectangle[axis].min(excess / 2.0);
            rectangle[axis] -= origin_adjustment;
            rectangle[axis + 2] -= excess - origin_adjustment;
        }
    }
    rectangle
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Gamma, ScanOptions, Source};

    fn capabilities() -> Capabilities {
        let fixture: serde_json::Value =
            serde_json::from_str(include_str!("../../tests/fixtures/v800-identity.json")).unwrap();
        let hex = fixture["extended_identity_hex"].as_str().unwrap();
        let bytes: Vec<u8> = (0..hex.len())
            .step_by(2)
            .map(|index| u8::from_str_radix(&hex[index..index + 2], 16).unwrap())
            .collect();
        Capabilities::parse(&bytes).unwrap()
    }

    fn frame(frame: u32, overage: f64, dpi: u32, mut options: ScanOptions) -> HolderFramePlan {
        let caps = capabilities();
        let model = caps.scanner_model().unwrap();
        let selection = HolderSelection {
            holder: Holder::V800Film35mm,
            frame_format: None,
            frame,
            overage_percent: overage,
        };
        options.holder_selection = Some(selection);
        let settings = ScanSettings {
            dpi,
            rect_mm: model
                .holder_frame(selection.holder, frame, overage)
                .unwrap(),
            ..Default::default()
        };
        HolderFramePlan {
            selection,
            plan: options.plan(&settings, &caps).unwrap(),
        }
    }

    fn assert_exact_crops(frames: &[HolderFramePlan], batches: &[HolderBatchPlan]) {
        for batch in batches {
            let source = &batch.plan.passes[0];
            for &index in &batch.frame_indices {
                let target = &frames[index].plan.passes[0];
                let [x, y, width, height] = target.pixels;
                assert!(x >= source.pixels[0] && y >= source.pixels[1]);
                let crop_x = x - source.pixels[0];
                let crop_y = y - source.pixels[1];
                assert!(crop_x + width <= source.pixels[2]);
                assert!(crop_y + height <= source.pixels[3]);
                assert_eq!(
                    [
                        source.pixels[0] + crop_x,
                        source.pixels[1] + crop_y,
                        width,
                        height
                    ],
                    target.pixels
                );
            }
        }
    }

    #[test]
    fn groups_strips_and_members_in_first_selected_order() {
        let frames: Vec<_> = [13, 6, 8, 1, 18, 12]
            .map(|number| frame(number, -20.0, 2400, ScanOptions::default()))
            .into();
        let batches = plan_holder_batches(&frames, &capabilities()).unwrap();
        assert_eq!(
            batches.iter().map(|batch| batch.strip).collect::<Vec<_>>(),
            [Some(3), Some(1), Some(2)]
        );
        assert_eq!(
            batches
                .iter()
                .map(|batch| batch.frame_indices.clone())
                .collect::<Vec<_>>(),
            [vec![0, 4], vec![1, 3], vec![2, 5]]
        );
        assert_exact_crops(&frames, &batches);
        let left_strip = &batches[1].plan.passes[0];
        assert_eq!(left_strip.pixels[1], frames[3].plan.passes[0].pixels[1]);
        assert!(left_strip.pixels[3] > frames[1].plan.passes[0].pixels[3] * 2);
    }

    #[test]
    fn medium_and_half_frames_group_by_selected_format_and_physical_strip() {
        let caps = capabilities();
        let model = caps.scanner_model().unwrap();
        for (holder, format, expected_batches) in [
            (Holder::V800Film35mm, FrameFormat::Film35mmHalf, 3),
            (Holder::V800MediumFormat, FrameFormat::Film6x45, 1),
            (Holder::V800MediumFormat, FrameFormat::Film6x6, 1),
            (Holder::V800MediumFormat, FrameFormat::Film6x17, 1),
        ] {
            let layout = model
                .holder(holder)
                .unwrap()
                .for_format(Some(format))
                .unwrap();
            let frames: Vec<_> = (1..=layout.frames_mm.len() as u32)
                .map(|frame| {
                    let selection = HolderSelection {
                        holder,
                        frame_format: Some(format),
                        frame,
                        overage_percent: 0.0,
                    };
                    let settings = ScanSettings {
                        rect_mm: layout.frame_rect(frame, 0.0).unwrap(),
                        dpi: 1200,
                        ..Default::default()
                    };
                    let options = ScanOptions {
                        holder_selection: Some(selection),
                        ..Default::default()
                    };
                    HolderFramePlan {
                        selection,
                        plan: options.plan(&settings, &caps).unwrap(),
                    }
                })
                .collect();
            let batches = plan_holder_batches(&frames, &caps).unwrap();
            assert_eq!(batches.len(), expected_batches);
            assert_exact_crops(&frames, &batches);
        }
        // Two presets on the same physical strip must not accidentally become
        // one logical strip selection with inconsistent frame numbering.
        let full = frame(1, 0.0, 300, ScanOptions::default());
        let mut explicit_default = full.clone();
        explicit_default.selection.frame_format = Some(FrameFormat::Film35mm);
        assert_eq!(
            plan_holder_batches(&[full.clone(), explicit_default], &caps)
                .unwrap()
                .len(),
            1
        );
        let mut half = full.clone();
        half.selection.frame_format = Some(FrameFormat::Film35mmHalf);
        let settings = ScanSettings {
            rect_mm: model
                .holder_frame_with_format(
                    half.selection.holder,
                    half.selection.frame_format,
                    1,
                    0.0,
                )
                .unwrap(),
            dpi: 300,
            ..Default::default()
        };
        half.plan = ScanOptions {
            holder_selection: Some(half.selection),
            ..Default::default()
        }
        .plan(&settings, &caps)
        .unwrap();
        assert_eq!(plan_holder_batches(&[full, half], &caps).unwrap().len(), 2);
    }

    #[test]
    fn unions_preserve_exact_pixels_across_dpi_and_signed_overage() {
        for dpi in [25, 100, 300, 302, 1200, 2400, 6400, 12800] {
            for overage in [-50.0, -10.0, 0.0, 5.0] {
                let frames: Vec<_> = [1, 3, 6, 7, 12, 13, 18]
                    .map(|number| frame(number, overage, dpi, ScanOptions::default()))
                    .into();
                let batches = plan_holder_batches(&frames, &capabilities()).unwrap();
                assert_eq!(batches.len(), 3);
                assert_exact_crops(&frames, &batches);
                for batch in batches {
                    batch.settings.validate(&capabilities(), false).unwrap();
                    assert_eq!(
                        batch.settings.pixels().unwrap(),
                        batch.plan.passes[0].pixels
                    );
                }
            }
        }
    }

    #[test]
    fn preview_groups_but_thumbnail_and_ir_plans_remain_separate() {
        let caps = capabilities();
        let mut previews = vec![
            frame(1, 0.0, 100, ScanOptions::default()),
            frame(6, 0.0, 100, ScanOptions::default()),
        ];
        for preview in &mut previews {
            preview.plan.passes[0].settings.preview = true;
        }
        assert_eq!(plan_holder_batches(&previews, &caps).unwrap().len(), 1);
        for options in [
            ScanOptions {
                thumbnail: true,
                ..Default::default()
            },
            ScanOptions {
                infrared: true,
                ..Default::default()
            },
            ScanOptions {
                infrared_only: true,
                ..Default::default()
            },
        ] {
            let frames = vec![
                frame(1, 0.0, 300, options.clone()),
                frame(6, 0.0, 300, options),
            ];
            let batches = plan_holder_batches(&frames, &caps).unwrap();
            assert_eq!(batches.len(), 2);
            for (original, batch) in frames.iter().zip(batches) {
                assert_eq!(
                    serde_json::to_value(&original.plan).unwrap(),
                    serde_json::to_value(batch.plan).unwrap()
                );
            }
        }
    }

    #[test]
    fn different_nongeometry_settings_do_not_share_a_capture() {
        let mut frames = vec![
            frame(1, 0.0, 300, ScanOptions::default()),
            frame(2, 0.0, 600, ScanOptions::default()),
            frame(3, 0.0, 300, ScanOptions::default()),
            frame(4, 0.0, 300, ScanOptions::default()),
        ];
        frames[3].plan.passes[0].settings.gamma = Gamma::IdentityLut;
        let batches = plan_holder_batches(&frames, &capabilities()).unwrap();
        assert_eq!(
            batches
                .iter()
                .map(|batch| batch.frame_indices.clone())
                .collect::<Vec<_>>(),
            [vec![0, 2], vec![1], vec![3]]
        );
        assert_exact_crops(&frames, &batches);
    }

    #[test]
    fn grayscale_frames_share_strip_acquisition_but_do_not_mix_with_rgb() {
        let caps = capabilities();
        for depth in [8, 16] {
            let mut frames: Vec<_> = [1, 2, 3, 4, 7, 12]
                .map(|number| frame(number, -20.0, 2400, ScanOptions::default()))
                .into();
            for (index, frame) in frames.iter_mut().enumerate() {
                let mut settings = frame.plan.passes[0].settings.clone();
                settings.depth = depth;
                if index != 1 {
                    settings.mode = crate::ScanMode::Gray;
                }
                frame.plan = ScanOptions::default().plan(&settings, &caps).unwrap();
            }
            let batches = plan_holder_batches(&frames, &caps).unwrap();
            assert_eq!(
                batches
                    .iter()
                    .map(|batch| batch.frame_indices.clone())
                    .collect::<Vec<_>>(),
                [vec![0, 2, 3], vec![1], vec![4, 5]]
            );
            assert_exact_crops(&frames, &batches);
            for batch in [&batches[0], &batches[2]] {
                let pass = &batch.plan.passes[0];
                assert_eq!(pass.kind, PassKind::Gray);
                assert_eq!(pass.settings.mode, crate::ScanMode::Gray);
                assert_eq!(pass.channels, 1);
                assert_eq!(
                    pass.expected_bytes,
                    u64::from(pass.pixels[2]) * u64::from(pass.pixels[3]) * u64::from(depth / 8)
                );
                assert_eq!(pass.settings.parameters(false).unwrap()[24], 0);
            }
            assert_eq!(batches[1].plan.passes[0].kind, PassKind::Rgb);
            assert_eq!(batches[1].plan.passes[0].channels, 3);
        }
    }

    fn pixel_pass(pixels: [u32; 4], caps: &Capabilities) -> PlannedPass {
        let settings = ScanSettings {
            dpi: 254,
            rect_mm: exact_rectangle_mm(pixels, 254, caps.area_mm(Source::Transparency)),
            ..Default::default()
        };
        settings.validate(caps, false).unwrap();
        assert_eq!(settings.pixels().unwrap(), pixels);
        PlannedPass {
            kind: PassKind::Rgb,
            settings,
            pixels,
            channels: 3,
            expected_bytes: expected_bytes(pixels, 3, 16).unwrap(),
        }
    }

    #[test]
    fn aligned_union_shifts_left_at_source_edge_without_losing_pixels() {
        let caps = capabilities();
        let first = pixel_pass([1482, 0, 8, 10], &caps);
        let second = pixel_pass([1491, 0, 8, 10], &caps);
        let union = union_pass(&[&first, &second], &caps).unwrap();
        assert_eq!(union.pixels, [1475, 0, 24, 10]);
        assert_eq!(union.settings.pixels().unwrap(), union.pixels);
        union.settings.validate(&caps, false).unwrap();
    }

    #[test]
    fn source_edge_shift_rejects_a_crop_extending_one_pixel_past_the_source() {
        let caps = capabilities();
        let first = pixel_pass([1482, 0, 8, 10], &caps);
        let mut second = pixel_pass([1491, 0, 8, 10], &caps);
        // Simulate an individually rounded origin/width endpoint of 1500,
        // beyond the source's 1499-pixel boundary. Moving the union left must
        // fail rather than silently exclude that final requested column.
        second.pixels[0] += 1;
        assert!(matches!(
            union_pass(&[&first, &second], &caps),
            Err(Error::Invalid(message)) if message.contains("cannot contain")
        ));
    }

    #[test]
    fn union_rejects_device_width_limit_and_forged_frame_geometry() {
        let mut caps = capabilities();
        let first = pixel_pass([0, 0, 16, 10], &caps);
        let second = pixel_pass([24, 0, 16, 10], &caps);
        caps.max_width_pixels = 32;
        assert!(union_pass(&[&first, &second], &caps).is_err());
        let mut forged = frame(1, 0.0, 300, ScanOptions::default());
        forged.plan.passes[0].pixels[0] += 1;
        assert!(plan_holder_batches(&[forged], &capabilities()).is_err());
        let mut wrong_holder = frame(1, 0.0, 300, ScanOptions::default());
        wrong_holder.selection.frame = 6;
        assert!(plan_holder_batches(&[wrong_holder], &capabilities()).is_err());
        let mut wrong_kind = frame(1, 0.0, 300, ScanOptions::default());
        wrong_kind.plan.passes[0].kind = PassKind::Gray;
        assert!(plan_holder_batches(&[wrong_kind], &capabilities()).is_err());
        let mut wrong_kind = frame(1, 0.0, 300, ScanOptions::default());
        let settings = ScanSettings {
            mode: crate::ScanMode::Gray,
            ..wrong_kind.plan.passes[0].settings.clone()
        };
        wrong_kind.plan = ScanOptions::default()
            .plan(&settings, &capabilities())
            .unwrap();
        wrong_kind.plan.passes[0].kind = PassKind::Rgb;
        assert!(plan_holder_batches(&[wrong_kind], &capabilities()).is_err());
    }

    #[test]
    fn empty_selection_is_empty_and_single_frame_is_unchanged() {
        let caps = capabilities();
        assert!(plan_holder_batches(&[], &caps).unwrap().is_empty());
        let selected = frame(6, 10.0, 300, ScanOptions::default());
        let batches = plan_holder_batches(std::slice::from_ref(&selected), &caps).unwrap();
        assert_eq!(batches[0].frame_indices, [0]);
        assert_eq!(
            serde_json::to_value(&selected.plan).unwrap(),
            serde_json::to_value(&batches[0].plan).unwrap()
        );
    }
}
