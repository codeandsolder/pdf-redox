//! Exact sampled stroke footprints, including separable axis-aligned intervals.

use super::Point;
use crate::bilevel::BilevelRaster;
use num_traits::ToPrimitive;
use std::ops::RangeInclusive;

pub(super) struct SampleBounds {
    pub(super) xs: RangeInclusive<u32>,
    pub(super) ys: RangeInclusive<u32>,
}

pub(super) struct StrokeFootprint {
    origin: Point,
    delta: Point,
    inv_len2: f64,
    radius2: f64,
    extension: f64,
    square_cap_cross_limit: f64,
    cap: u8,
}

impl StrokeFootprint {
    pub(super) fn new(start: Point, end: Point, radius: f64, cap: u8) -> Option<Self> {
        let (dx, dy) = (end.0 - start.0, end.1 - start.1);
        let len2 = dy.mul_add(dy, dx * dx);
        if len2 <= f64::EPSILON {
            return None;
        }
        let length = if cap == 2 { len2.sqrt() } else { 0.0 };
        Some(Self {
            origin: start,
            delta: (dx, dy),
            inv_len2: 1.0 / len2,
            radius2: radius * radius,
            extension: if cap == 2 { radius / length } else { 0.0 },
            square_cap_cross_limit: radius * length,
            cap,
        })
    }

    fn projected_parameter(&self, raw_t: f64) -> Option<f64> {
        match self.cap {
            0 => (0.0..=1.0).contains(&raw_t).then_some(raw_t),
            1 => Some(raw_t.clamp(0.0, 1.0)),
            2 if !(raw_t < -self.extension || raw_t > 1.0 + self.extension) => {
                Some(raw_t.clamp(0.0, 1.0))
            }
            _ => None,
        }
    }

    fn covers(&self, px: f64, py: f64) -> bool {
        let (x0, y0) = self.origin;
        let (dx, dy) = self.delta;
        let raw_t = (py - y0).mul_add(dy, (px - x0) * dx) * self.inv_len2;
        let Some(t) = self.projected_parameter(raw_t) else {
            return false;
        };
        let ex = px - t.mul_add(dx, x0);
        let ey = py - t.mul_add(dy, y0);
        if self.cap == 2 {
            ey.mul_add(dx, ex * -dy).abs() <= self.square_cap_cross_limit
        } else {
            ey.mul_add(ey, ex * ex) <= self.radius2
        }
    }

    fn paint_general(&self, mask: &mut BilevelRaster, bounds: &SampleBounds) -> Option<bool> {
        let mut covered = false;
        for y in bounds.ys.clone() {
            let py = sample_center(y);
            for x in bounds.xs.clone() {
                if self.covers(sample_center(x), py) {
                    covered = true;
                    mask.paint(usize::try_from(x).ok()?, usize::try_from(y).ok()?);
                }
            }
        }
        Some(covered)
    }

    /// Along an axis the projection parameter is independent of the
    /// transverse sample. The fused zero term equals an ordinary product
    /// (apart from irrelevant signed zero), and the transverse residual
    /// is simply `sample_center - origin`.
    ///
    /// For fixed axial coordinate, the original predicate is monotone
    /// away from the nearest transverse sample on either side, including
    /// correctly rounded FMA at the boundary. Find the inclusive interval
    /// using that exact predicate rather than a rounded analytic radius.
    fn paint_axis<const HORIZONTAL: bool>(
        &self,
        mask: &mut BilevelRaster,
        bounds: &SampleBounds,
    ) -> Option<bool> {
        let (origin, transverse, delta, axial_range, cross_range) = if HORIZONTAL {
            (
                self.origin.0,
                self.origin.1,
                self.delta.0,
                &bounds.xs,
                &bounds.ys,
            )
        } else {
            (
                self.origin.1,
                self.origin.0,
                self.delta.1,
                &bounds.ys,
                &bounds.xs,
            )
        };
        if cross_range.is_empty() || axial_range.is_empty() {
            return Some(false);
        }
        let nearest = transverse
            .floor()
            .clamp(
                f64::from(*cross_range.start()),
                f64::from(*cross_range.end()),
            )
            .to_u32()?;
        let mut covered = false;
        // Equal absolute axial errors have exactly equal transverse
        // predicates, even with the original FMA operand order. Square
        // caps have no axial-error dependence at all.
        let mut cached_interval = None::<(u64, Option<RangeInclusive<u32>>)>;
        for index in axial_range.clone() {
            let coordinate = sample_center(index);
            let raw_t = ((coordinate - origin) * delta) * self.inv_len2;
            let Some(t) = self.projected_parameter(raw_t) else {
                continue;
            };
            let axial_error = if self.cap == 2 {
                0.0
            } else {
                coordinate - t.mul_add(delta, origin)
            };
            let axial_key = axial_error.abs().to_bits();
            let interval = if let Some((key, interval)) = &cached_interval
                && *key == axial_key
            {
                interval.clone()
            } else {
                let axial_square = axial_error * axial_error;
                let interval = covered_interval(cross_range, nearest, |cross| {
                    let error = sample_center(cross) - transverse;
                    if self.cap == 2 {
                        (error * delta).abs() <= self.square_cap_cross_limit
                    } else if HORIZONTAL {
                        error.mul_add(error, axial_square) <= self.radius2
                    } else {
                        // Retain the original FMA operand order even on a
                        // vertical stroke; swapping axes could change a tie.
                        axial_error.mul_add(axial_error, error * error) <= self.radius2
                    }
                });
                cached_interval = Some((axial_key, interval.clone()));
                interval
            };
            if let Some(interval) = interval {
                covered = true;
                if HORIZONTAL {
                    mask.paint_column_span(index, interval)?;
                } else {
                    mask.paint_row_span(index, interval)?;
                }
            }
        }
        Some(covered)
    }

    pub(super) fn paint(&self, mask: &mut BilevelRaster, bounds: &SampleBounds) -> Option<bool> {
        // The caller's bounded raster geometry guarantees finite coordinates.
        // Retain the full predicate for all non-separable footprints.
        if self.delta.1 == 0.0 {
            self.paint_axis::<true>(mask, bounds)
        } else if self.delta.0 == 0.0 {
            self.paint_axis::<false>(mask, bounds)
        } else {
            self.paint_general(mask, bounds)
        }
    }
}

fn sample_center(index: u32) -> f64 {
    f64::from(index) + 0.5
}

fn covered_interval(
    bounds: &RangeInclusive<u32>,
    nearest: u32,
    mut contains: impl FnMut(u32) -> bool,
) -> Option<RangeInclusive<u32>> {
    if !contains(nearest) {
        return None;
    }
    let mut low = *bounds.start();
    let mut high = nearest;
    while low < high {
        let mid = low.midpoint(high);
        if contains(mid) {
            high = mid;
        } else {
            low = mid + 1;
        }
    }
    let first = low;
    low = nearest;
    high = *bounds.end();
    while low < high {
        let mid = high - (high - low) / 2;
        if contains(mid) {
            low = mid;
        } else {
            high = mid - 1;
        }
    }
    Some(first..=low)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Error, Result};

    #[test]
    fn separable_footprints_match_full_predicate_and_existing_ink() -> Result<()> {
        let bounds = SampleBounds {
            xs: 0..=22,
            ys: 0..=18,
        };
        for cap in 0..=2 {
            for origin in [
                (0.0, 0.0),
                (0.5, 0.5),
                (4.25, 8.75),
                (11.5, 12.5),
                (23.0, 19.0),
            ] {
                for radius in [0.0, 0.125, 0.5, 0.75, 1.0, 2.375, 10.0] {
                    for delta in [-15.0, -1.0, -0.125, 0.125, 1.0, 13.0] {
                        for end in [(origin.0 + delta, origin.1), (origin.0, origin.1 + delta)] {
                            let stroke = StrokeFootprint::new(origin, end, radius, cap)
                                .ok_or_else(|| Error::Invalid("stroke fixture".into()))?;
                            let mut expected = BilevelRaster::transparent(23, 19)
                                .ok_or_else(|| Error::Invalid("raster fixture".into()))?;
                            let mut actual = expected.clone();
                            for _ in 0..2 {
                                assert_eq!(
                                    stroke.paint(&mut actual, &bounds),
                                    stroke.paint_general(&mut expected, &bounds)
                                );
                                assert_eq!(
                                    actual, expected,
                                    "cap={cap}, origin={origin:?}, end={end:?}, radius={radius}"
                                );
                            }
                        }
                    }
                }
            }
        }
        Ok(())
    }

    #[test]
    fn reused_axis_intervals_match_every_pixel_at_rounding_boundaries() -> Result<()> {
        let bounds = SampleBounds {
            xs: 0..=63,
            ys: 0..=63,
        };
        // Exact sample-centre and half-pixel radii exercise inclusive ties,
        // and nearby representable floats exercise either side of the tie.
        for cap in 0..=2 {
            for coordinate in [0.5_f64, 15.5, 32.0, 63.5] {
                for bits in [
                    coordinate.to_bits() - 1,
                    coordinate.to_bits(),
                    coordinate.to_bits() + 1,
                ] {
                    let origin = (f64::from_bits(bits), f64::from_bits(bits));
                    for delta in [-63.125, -1.0, 0.125, 1.0, 63.125] {
                        for radius in [0.5_f64, 1.0, 1.5] {
                            for end in [(origin.0 + delta, origin.1), (origin.0, origin.1 + delta)]
                            {
                                let stroke = StrokeFootprint::new(origin, end, radius, cap)
                                    .ok_or_else(|| Error::Invalid("stroke fixture".into()))?;
                                let mut expected = BilevelRaster::transparent(64, 64)
                                    .ok_or_else(|| Error::Invalid("raster fixture".into()))?;
                                let mut actual = expected.clone();
                                assert_eq!(
                                    stroke.paint(&mut actual, &bounds),
                                    stroke.paint_general(&mut expected, &bounds)
                                );
                                assert_eq!(
                                    actual, expected,
                                    "{cap}, {origin:?}, {end:?}, {radius}"
                                );
                            }
                        }
                    }
                }
            }
        }
        Ok(())
    }

    #[test]
    fn interval_search_matches_all_contiguous_intersections() {
        for start in 0..20u32 {
            for end in start..20 {
                for left in start..=end {
                    for right in left..=end {
                        let range = left..=right;
                        assert_eq!(
                            covered_interval(&(start..=end), left.midpoint(right), |i| range
                                .contains(&i)),
                            Some(range)
                        );
                    }
                }
                assert_eq!(covered_interval(&(start..=end), start, |_| false), None);
            }
        }
    }
}
