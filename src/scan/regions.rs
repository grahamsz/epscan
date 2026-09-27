// SPDX-License-Identifier: MIT
//! Plan continuous-strip acquisitions from freely positioned crop rectangles.
use super::{PassKind, ScanOptions, ScanPlan, holder::union_pass};
use crate::{Capabilities, Error, Result, ScanSettings};
use serde::Serialize;

/// A bounding-box acquisition and the requested regions it contains.
/// Members retain their original request order, including duplicate rectangles.
#[derive(Clone, Debug, Serialize)]
pub struct RegionBatchPlan {
    pub region_indices: Vec<usize>,
    pub settings: ScanSettings,
    pub plan: ScanPlan,
}

/// Independently validated output regions and the captures needed to supply them.
/// Both regions and batches retain first-requested order.
#[derive(Clone, Debug, Serialize)]
pub struct RegionScanPlan {
    pub region_plans: Vec<ScanPlan>,
    pub batches: Vec<RegionBatchPlan>,
}

/// Group nearby, vertically arranged regions into continuous strip captures.
///
/// Regions use physical `[x, y, width, height]` millimetres and replace the base
/// settings' rectangle. Vertical neighbors must overlap by at least half the
/// narrower region's width and have at most `max_gap_mm` between them. Connected
/// neighbors share a capture regardless of input order. This infers strips from
/// the edited geometry without requiring an exact registered holder layout.
///
/// Only single RGB or grayscale plans are merged; infrared, thumbnail, and
/// multipass plans remain independent, preserving every requested pass. Each
/// region is validated before any batch is returned. Bounding boxes contain
/// every region's exact aligned wire pixels, so extraction needs no resampling.
pub fn plan_region_batches(
    regions: &[[f64; 4]],
    settings: &ScanSettings,
    options: &ScanOptions,
    caps: &Capabilities,
    max_gap_mm: f64,
) -> Result<RegionScanPlan> {
    if regions.is_empty() {
        return Err(Error::Invalid(
            "At least one scan region is required".into(),
        ));
    }
    if !max_gap_mm.is_finite() || max_gap_mm < 0.0 {
        return Err(Error::Invalid(
            "Maximum region gap must be finite and nonnegative".into(),
        ));
    }
    let region_plans = regions
        .iter()
        .map(|&rect_mm| {
            options.plan(
                &ScanSettings {
                    rect_mm,
                    ..settings.clone()
                },
                caps,
            )
        })
        .collect::<Result<Vec<_>>>()?;

    let mergeable: Vec<_> = region_plans
        .iter()
        .map(|plan| {
            plan.passes.len() == 1 && matches!(plan.passes[0].kind, PassKind::Rgb | PassKind::Gray)
        })
        .collect();
    let mut roots: Vec<_> = (0..regions.len()).collect();
    for left in 0..regions.len() {
        if !mergeable[left] {
            continue;
        }
        for right in left + 1..regions.len() {
            if mergeable[right] && strip_neighbors(regions[left], regions[right], max_gap_mm) {
                let left_root = find_root(&mut roots, left);
                let right_root = find_root(&mut roots, right);
                roots[right_root] = left_root;
            }
        }
    }
    let mut groups: Vec<(usize, Vec<usize>)> = Vec::new();
    for index in 0..regions.len() {
        let root = find_root(&mut roots, index);
        if let Some((_, members)) = groups.iter_mut().find(|(candidate, _)| *candidate == root) {
            members.push(index);
        } else {
            groups.push((root, vec![index]));
        }
    }
    let batches = groups
        .into_iter()
        .map(|(_, region_indices)| {
            let batch_settings = if region_indices.len() == 1 {
                Some(ScanSettings {
                    rect_mm: regions[region_indices[0]],
                    ..settings.clone()
                })
            } else {
                None
            };
            let plan = if region_indices.len() == 1 {
                region_plans[region_indices[0]].clone()
            } else {
                let passes: Vec<_> = region_indices
                    .iter()
                    .map(|&index| &region_plans[index].passes[0])
                    .collect();
                ScanPlan {
                    passes: vec![union_pass(&passes, caps)?],
                }
            };
            Ok(RegionBatchPlan {
                region_indices,
                settings: batch_settings.unwrap_or_else(|| plan.passes[0].settings.clone()),
                plan,
            })
        })
        .collect::<Result<Vec<_>>>()?;
    Ok(RegionScanPlan {
        region_plans,
        batches,
    })
}

fn find_root(roots: &mut [usize], mut index: usize) -> usize {
    while roots[index] != index {
        roots[index] = roots[roots[index]];
        index = roots[index];
    }
    index
}

fn strip_neighbors(left: [f64; 4], right: [f64; 4], max_gap_mm: f64) -> bool {
    // Match physical rectangle validation: absorb decimal arithmetic noise
    // without making a meaningful change to the user's overlap or gap limit.
    const TOLERANCE_MM: f64 = 1e-6;
    let overlap = (left[0] + left[2]).min(right[0] + right[2]) - left[0].max(right[0]);
    let vertical_gap = (left[1] - right[1] - right[3])
        .max(right[1] - left[1] - left[3])
        .max(0.0);
    overlap + TOLERANCE_MM >= left[2].min(right[2]) * 0.5
        && vertical_gap <= max_gap_mm + TOLERANCE_MM
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{ScanMode, Source, capabilities::Holder};

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

    fn holder_regions() -> Vec<[f64; 4]> {
        capabilities()
            .scanner_model()
            .unwrap()
            .holder(Holder::V800Film35mm)
            .unwrap()
            .frames_mm
            .to_vec()
    }

    fn plan(regions: &[[f64; 4]]) -> RegionScanPlan {
        plan_region_batches(
            regions,
            &ScanSettings::default(),
            &ScanOptions::default(),
            &capabilities(),
            10.0,
        )
        .unwrap()
    }

    fn groups(plan: &RegionScanPlan) -> Vec<Vec<usize>> {
        plan.batches
            .iter()
            .map(|batch| batch.region_indices.clone())
            .collect()
    }

    fn assert_exact_coverage(plan: &RegionScanPlan) {
        for batch in &plan.batches {
            let source = &batch.plan.passes[0];
            for &index in &batch.region_indices {
                let target = &plan.region_plans[index].passes[0];
                let [x, y, width, height] = target.pixels;
                let crop_x = x.checked_sub(source.pixels[0]).unwrap();
                let crop_y = y.checked_sub(source.pixels[1]).unwrap();
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
                assert_eq!(
                    target.expected_bytes,
                    u64::from(width)
                        * u64::from(height)
                        * u64::from(target.channels)
                        * u64::from(target.settings.depth / 8)
                );
            }
            assert_eq!(source.settings.pixels().unwrap(), source.pixels);
            source.settings.validate(&capabilities(), false).unwrap();
        }
    }

    #[test]
    fn normal_eighteen_frame_holder_requires_three_strip_captures() {
        let result = plan(&holder_regions());
        assert_eq!(
            groups(&result),
            [
                (0..6).collect::<Vec<_>>(),
                (6..12).collect(),
                (12..18).collect()
            ]
        );
        assert_eq!(result.region_plans.len(), 18);
        assert_exact_coverage(&result);
    }

    #[test]
    fn edited_positions_and_sizes_group_by_geometry() {
        let mut regions = holder_regions();
        for (index, region) in regions.iter_mut().enumerate() {
            region[0] += (index % 3) as f64 * 0.7;
            region[1] += 1.0 + (index % 2) as f64 * 0.25;
            region[2] -= 1.2;
            region[3] -= 3.0;
        }
        let result = plan(&regions);
        assert_eq!(result.batches.len(), 3);
        assert_exact_coverage(&result);
        for (region, individual) in regions.iter().zip(&result.region_plans) {
            assert_eq!(*region, individual.passes[0].settings.rect_mm);
        }
    }

    #[test]
    fn unsorted_regions_keep_first_request_batch_and_member_order() {
        let holder = holder_regions();
        let regions: Vec<_> = [17, 6, 3, 12, 5, 0, 7, 15, 14, 13, 16, 1, 2, 4, 8, 9, 10, 11]
            .map(|index| holder[index])
            .into();
        let result = plan(&regions);
        assert_eq!(
            groups(&result),
            [
                vec![0, 3, 7, 8, 9, 10],
                vec![1, 6, 14, 15, 16, 17],
                vec![2, 4, 5, 11, 12, 13]
            ]
        );
        assert_exact_coverage(&result);
    }

    #[test]
    fn missing_frame_splits_a_strip_when_the_gap_is_too_large() {
        let mut regions = holder_regions();
        regions.remove(2);
        let result = plan(&regions);
        assert_eq!(
            groups(&result),
            [
                vec![0, 1],
                vec![2, 3, 4],
                (5..11).collect(),
                (11..17).collect()
            ]
        );
        assert_exact_coverage(&result);
    }

    #[test]
    fn configurable_gap_includes_exact_boundary_and_splits_beyond_it() {
        let regions = [[10.0, 10.0, 24.0, 30.0], [10.0, 50.0, 24.0, 30.0]];
        assert_eq!(plan(&regions).batches.len(), 1);
        let result = plan_region_batches(
            &regions,
            &ScanSettings::default(),
            &ScanOptions::default(),
            &capabilities(),
            9.99,
        )
        .unwrap();
        assert_eq!(result.batches.len(), 2);
    }

    #[test]
    fn decimal_gap_and_overlap_boundaries_ignore_arithmetic_noise() {
        // The exact physical gap is 10 mm, but subtraction yields
        // 10.000000000000004 in binary floating point.
        let regions = [[10.0, 0.3, 24.0, 28.2], [10.0, 38.5, 24.0, 28.2]];
        assert!(regions[1][1] - regions[0][1] - regions[0][3] > 10.0);
        assert_eq!(plan(&regions).batches.len(), 1);
        let mut beyond_gap = regions;
        beyond_gap[1][1] += 0.00001;
        assert_eq!(plan(&beyond_gap).batches.len(), 2);

        // The second rectangle overlaps exactly half the narrower width;
        // its decimal endpoint is inexact for the same reason.
        let overlap_regions = [[0.1, 10.0, 4.2, 36.0], [2.2, 48.0, 4.2, 36.0]];
        assert!(
            overlap_regions[0][0] + overlap_regions[0][2] - overlap_regions[1][0]
                < overlap_regions[0][2] * 0.5
        );
        assert_eq!(plan(&overlap_regions).batches.len(), 1);
        let mut beyond_overlap = overlap_regions;
        beyond_overlap[1][0] += 0.00001;
        assert_eq!(plan(&beyond_overlap).batches.len(), 2);
    }

    #[test]
    fn overlapping_and_duplicate_regions_are_preserved() {
        let regions = [
            [10.0, 10.0, 24.0, 36.0],
            [10.0, 10.0, 24.0, 36.0],
            [12.0, 30.0, 22.0, 36.0],
        ];
        let result = plan(&regions);
        assert_eq!(groups(&result), [vec![0, 1, 2]]);
        assert_eq!(
            result.region_plans[0].passes[0].pixels,
            result.region_plans[1].passes[0].pixels
        );
        assert_exact_coverage(&result);
    }

    #[test]
    fn side_by_side_columns_do_not_share_a_capture() {
        let regions = [
            [10.0, 10.0, 24.0, 36.0],
            [29.0, 10.0, 24.0, 36.0],
            [10.0, 48.0, 24.0, 36.0],
            [29.0, 48.0, 24.0, 36.0],
        ];
        assert_eq!(groups(&plan(&regions)), [vec![0, 2], vec![1, 3]]);
    }

    #[test]
    fn invalid_final_region_rejects_the_entire_job() {
        for invalid in [
            [0.0, 0.0, 0.0, 36.0],
            [0.0, 0.0, -1.0, 36.0],
            [f64::NAN, 0.0, 24.0, 36.0],
            [0.0, f64::INFINITY, 24.0, 36.0],
            [-1.0, 0.0, 24.0, 36.0],
            [140.0, 0.0, 24.0, 36.0],
            [0.0, 230.0, 24.0, 36.0],
        ] {
            let mut regions = holder_regions();
            regions.push(invalid);
            assert!(
                plan_region_batches(
                    &regions,
                    &ScanSettings::default(),
                    &ScanOptions::default(),
                    &capabilities(),
                    10.0
                )
                .is_err()
            );
        }
    }

    #[test]
    fn empty_regions_and_invalid_gap_are_rejected() {
        assert!(
            plan_region_batches(
                &[],
                &ScanSettings::default(),
                &ScanOptions::default(),
                &capabilities(),
                10.0
            )
            .is_err()
        );
        for gap in [-1.0, f64::NAN, f64::INFINITY] {
            assert!(
                plan_region_batches(
                    &holder_regions(),
                    &ScanSettings::default(),
                    &ScanOptions::default(),
                    &capabilities(),
                    gap
                )
                .is_err()
            );
        }
    }

    #[test]
    fn infrared_and_multipass_regions_retain_their_original_plans() {
        for options in [
            ScanOptions {
                infrared: true,
                ..Default::default()
            },
            ScanOptions {
                infrared_only: true,
                ..Default::default()
            },
            ScanOptions {
                thumbnail: true,
                ..Default::default()
            },
            ScanOptions {
                thumbnail: true,
                infrared: true,
                ..Default::default()
            },
        ] {
            let regions = holder_regions();
            let result = plan_region_batches(
                &regions,
                &ScanSettings::default(),
                &options,
                &capabilities(),
                10.0,
            )
            .unwrap();
            assert_eq!(result.batches.len(), regions.len());
            for (index, batch) in result.batches.iter().enumerate() {
                assert_eq!(batch.region_indices, [index]);
                assert_eq!(
                    serde_json::to_value(&batch.plan).unwrap(),
                    serde_json::to_value(&result.region_plans[index]).unwrap()
                );
                assert_eq!(
                    serde_json::to_value(options.plan(&batch.settings, &capabilities()).unwrap())
                        .unwrap(),
                    serde_json::to_value(&result.region_plans[index]).unwrap()
                );
            }
        }
    }

    #[test]
    fn grayscale_and_preview_regions_still_use_three_captures() {
        for mode in [ScanMode::Rgb, ScanMode::Gray] {
            for depth in [8, 16] {
                for preview in [false, true] {
                    let settings = ScanSettings {
                        mode,
                        depth,
                        preview,
                        ..Default::default()
                    };
                    let result = plan_region_batches(
                        &holder_regions(),
                        &settings,
                        &ScanOptions::default(),
                        &capabilities(),
                        10.0,
                    )
                    .unwrap();
                    assert_eq!(result.batches.len(), 3);
                    assert_exact_coverage(&result);
                }
            }
        }
    }

    #[test]
    fn exact_pixel_coverage_survives_resolution_and_width_alignment() {
        for dpi in [25, 100, 300, 302, 600, 2400, 6400, 12800] {
            let mut regions = holder_regions();
            for (index, region) in regions.iter_mut().enumerate() {
                region[0] += 0.13 * (index % 4) as f64;
                region[2] -= 0.23 * (index % 3) as f64;
            }
            let settings = ScanSettings {
                dpi,
                ..Default::default()
            };
            let result = plan_region_batches(
                &regions,
                &settings,
                &ScanOptions::default(),
                &capabilities(),
                10.0,
            )
            .unwrap();
            assert_eq!(result.batches.len(), 3);
            assert_exact_coverage(&result);
        }
    }

    #[test]
    fn aligned_union_at_source_edge_preserves_all_requested_pixels() {
        let caps = capabilities();
        let max_x = caps.area_mm(Source::Transparency)[0];
        let regions = [
            [max_x - 2.51, 10.0, 2.5, 36.0],
            [max_x - 1.6, 48.0, 1.6, 36.0],
        ];
        let settings = ScanSettings {
            dpi: 254,
            ..Default::default()
        };
        let result =
            plan_region_batches(&regions, &settings, &ScanOptions::default(), &caps, 10.0).unwrap();
        assert_eq!(result.batches.len(), 1);
        assert_exact_coverage(&result);
        let union = &result.batches[0].plan.passes[0];
        assert!(union.pixels[0] < result.region_plans[0].passes[0].pixels[0]);
    }
}
