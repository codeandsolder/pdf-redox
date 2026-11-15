//! Processing-mode simplification for grotesquely oversampled native polylines.
//!
//! This deliberately runs after the exact glyph/stroke factoring passes. It only
//! rewrites plain `m`/`l` subpaths that end in a pure fill and rejects mixed,
//! clipped, stroked, malformed, or graphics-state-interleaved paths.
//! Near-straight cubic curves in filled paths may use the shorter line operator
//! when their convex hull stays within a bounded page-space chord envelope.
//! Distances are measured after CTM and `/UserUnit`, so the tolerance is in real
//! page points rather than source-coordinate units.

use crate::{
    EditDocument, Result,
    content::{decoded_content_value, page_user_unit, replace_page_content},
    content_stream::{instruction_operands, operand_numbers},
    geometry::Matrix,
};

pub const MAX_POLYLINE_PAGE_ERROR_PT: f64 = 0.005;
const MAX_CUBIC_CHORD_PAGE_ERROR_PT: f64 = 0.05;
const MIN_CUBIC_CHORD_LENGTH_PT: f64 = 0.5;
const MIN_RUN_POINTS: usize = 8;
const MIN_FLATE_SAVINGS_BYTES: usize = 1024;

type Replacement = (usize, usize, Vec<u8>);
type ScanResult = (Vec<Replacement>, usize, usize);

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PolylineSimplificationStats {
    pub pages_rewritten: usize,
    pub vertices_removed: usize,
    pub cubics_simplified: usize,
    pub decoded_bytes_removed: usize,
    pub estimated_flate_bytes_saved: usize,
}

#[derive(Debug, Clone, Copy)]
struct Vertex {
    start: usize,
    end: usize,
    page_x: f64,
    page_y: f64,
    deletable: bool,
}

#[expect(
    clippy::struct_excessive_bools,
    reason = "separate fail-closed path, curve and graphics-state eligibility predicates"
)]
struct Scanner<'a> {
    input: &'a [u8],
    user_unit: f64,
    ctm: Option<Matrix>,
    graphics_state_valid: bool,
    stack: Vec<Option<Matrix>>,
    run: Vec<Vertex>,
    path_active: bool,
    path_eligible: bool,
    curve_eligible: bool,
    current_point: Option<(f64, f64)>,
    subpath_start: Option<(f64, f64)>,
    curve_pending: Vec<Replacement>,
    pending: Vec<Replacement>,
    committed: Vec<Replacement>,
    vertices_removed: usize,
    cubics_simplified: usize,
}

impl<'a> Scanner<'a> {
    fn new(input: &'a [u8], user_unit: f64) -> Self {
        Self {
            input,
            user_unit,
            ctm: Some(Matrix::default()),
            graphics_state_valid: true,
            stack: Vec::new(),
            run: Vec::new(),
            path_active: false,
            path_eligible: true,
            curve_eligible: true,
            current_point: None,
            subpath_start: None,
            curve_pending: Vec::new(),
            pending: Vec::new(),
            committed: Vec::new(),
            vertices_removed: 0,
            cubics_simplified: 0,
        }
    }

    fn invalidate_path(&mut self) {
        self.run.clear();
        self.pending.clear();
        self.curve_pending.clear();
        self.current_point = None;
        self.subpath_start = None;
        if self.path_active {
            self.path_eligible = false;
            self.curve_eligible = false;
        }
    }

    fn end_path(&mut self, commit: bool) {
        self.flush_run();
        if commit && self.path_eligible {
            self.vertices_removed = self.vertices_removed.saturating_add(self.pending.len());
            self.committed.append(&mut self.pending);
        } else {
            self.pending.clear();
        }
        if commit && self.curve_eligible {
            self.cubics_simplified = self
                .cubics_simplified
                .saturating_add(self.curve_pending.len());
            self.committed.append(&mut self.curve_pending);
        } else {
            self.curve_pending.clear();
        }
        self.run.clear();
        self.current_point = None;
        self.subpath_start = None;
        self.path_active = false;
        self.path_eligible = true;
        self.curve_eligible = true;
    }

    fn page_point(&self, x: f64, y: f64) -> Option<(f64, f64)> {
        let (x, y) = self.ctm?.transform(x, y);
        let result = (x * self.user_unit, y * self.user_unit);
        (result.0.is_finite() && result.1.is_finite()).then_some(result)
    }

    // A cubic lies within the convex hull of its controls. Their projections
    // must advance monotonically along the chord; the perpendicular envelope
    // then bounds the whole curve and excludes overshooting / looping curves.
    fn near_linear_cubic(
        start: (f64, f64),
        first: (f64, f64),
        second: (f64, f64),
        end: (f64, f64),
    ) -> bool {
        let dx = end.0 - start.0;
        let dy = end.1 - start.1;
        let len = dx.hypot(dy);
        if !len.is_finite() || len < MIN_CUBIC_CHORD_LENGTH_PT {
            return false;
        }
        let denom = len * len;
        if !denom.is_finite() {
            return false;
        }
        let project =
            |point: (f64, f64)| (point.1 - start.1).mul_add(dy, (point.0 - start.0) * dx) / denom;
        let first_t = project(first);
        let second_t = project(second);
        if !(0.0..=1.0).contains(&first_t) || !(first_t..=1.0).contains(&second_t) {
            return false;
        }
        [first, second].into_iter().all(|point| {
            let cross = (point.0 - start.0).mul_add(dy, -(point.1 - start.1) * dx);
            cross.is_finite() && cross.abs() <= MAX_CUBIC_CHORD_PAGE_ERROR_PT * len
        })
    }

    fn curve_operator(&mut self, instruction: &hayro_syntax::content::Instruction<'_, '_>) -> bool {
        let operator = &instruction.operator[..];
        let operands = instruction_operands(self.input, instruction);
        let count = if operator == b"c" { 6 } else { 4 };
        let Some(values) = operand_numbers(&operands)
            .filter(|values| values.len() == count && values.iter().all(|value| value.is_finite()))
        else {
            return false;
        };
        let Some(end) = self.page_point(values[count - 2], values[count - 1]) else {
            return false;
        };
        if operator == b"c"
            && let Some(start) = self.current_point
            && let (Some(first), Some(second)) = (
                self.page_point(values[0], values[1]),
                self.page_point(values[2], values[3]),
            )
            && Self::near_linear_cubic(start, first, second, end)
        {
            let spans = instruction.operand_spans().collect::<Vec<_>>();
            if spans.len() != 6 {
                return false;
            }
            let mut simplified = Vec::new();
            let Some(x) = self.input.get(spans[4].clone()) else {
                return false;
            };
            let Some(y) = self.input.get(spans[5].clone()) else {
                return false;
            };
            simplified.extend_from_slice(x);
            simplified.push(b' ');
            simplified.extend_from_slice(y);
            simplified.extend_from_slice(b" l");
            let end_offset = instruction.operator_span().end;
            if end_offset.saturating_sub(spans[0].start) > simplified.len() {
                self.curve_pending
                    .push((spans[0].start, end_offset, simplified));
            }
        }
        self.current_point = Some(end);
        true
    }

    fn line_vertex(
        &self,
        instruction: &hayro_syntax::content::Instruction<'_, '_>,
        deletable: bool,
    ) -> Option<Vertex> {
        let operands = instruction_operands(self.input, instruction);
        let values = operand_numbers(&operands).filter(|values| values.len() == 2)?;
        let ctm = self.ctm?;
        let (x, y) = ctm.transform(values[0], values[1]);
        let page_x = x * self.user_unit;
        let page_y = y * self.user_unit;
        if !page_x.is_finite() || !page_y.is_finite() {
            return None;
        }
        let operator = instruction.operator_span();
        let start = instruction
            .operand_spans()
            .next()
            .map_or(operator.start, |span| span.start);
        Some(Vertex {
            start,
            end: operator.end,
            page_x,
            page_y,
            deletable,
        })
    }

    fn concat_ctm(&mut self, instruction: &hayro_syntax::content::Instruction<'_, '_>) {
        let operands = instruction_operands(self.input, instruction);
        let matrix = operand_numbers(&operands)
            .filter(|values| values.len() == 6)
            .filter(|values| values.iter().all(|value| value.is_finite()))
            .map(|values| {
                Matrix::new(
                    values[0], values[1], values[2], values[3], values[4], values[5],
                )
            });
        match (&mut self.ctm, matrix) {
            (Some(ctm), Some(matrix)) => ctm.concat(matrix),
            _ => self.ctm = None,
        }
    }

    fn point_segment_distance(point: Vertex, first: Vertex, last: Vertex) -> f64 {
        let dx = last.page_x - first.page_x;
        let dy = last.page_y - first.page_y;
        let length_sq = dx.mul_add(dx, dy * dy);
        if length_sq <= f64::EPSILON {
            return (point.page_x - first.page_x).hypot(point.page_y - first.page_y);
        }
        let dot = (point.page_y - first.page_y).mul_add(dy, (point.page_x - first.page_x) * dx);
        let t = (dot / length_sq).clamp(0.0, 1.0);
        let nearest_x = dx.mul_add(t, first.page_x);
        let nearest_y = dy.mul_add(t, first.page_y);
        (point.page_x - nearest_x).hypot(point.page_y - nearest_y)
    }

    fn simplified_keep(run: &[Vertex]) -> Vec<bool> {
        let mut keep = vec![false; run.len()];
        if run.is_empty() {
            return keep;
        }
        keep[0] = true;
        if run.len() == 1 {
            return keep;
        }
        keep[run.len() - 1] = true;
        let mut stack = vec![(0usize, run.len() - 1)];
        while let Some((first, last)) = stack.pop() {
            if last <= first + 1 {
                continue;
            }
            let mut best_index = None;
            let mut best_distance = -1.0f64;
            for index in first + 1..last {
                let distance = Self::point_segment_distance(run[index], run[first], run[last]);
                if distance > best_distance {
                    best_distance = distance;
                    best_index = Some(index);
                }
            }
            if best_distance > MAX_POLYLINE_PAGE_ERROR_PT
                && let Some(index) = best_index
            {
                keep[index] = true;
                stack.push((first, index));
                stack.push((index, last));
            }
        }
        keep
    }

    fn flush_run(&mut self) {
        if self.run.len() < MIN_RUN_POINTS || !self.path_eligible {
            self.run.clear();
            return;
        }
        let keep = Self::simplified_keep(&self.run);
        for (vertex, keep) in self.run.drain(..).zip(keep) {
            if vertex.deletable && !keep {
                self.pending.push((vertex.start, vertex.end, Vec::new()));
            }
        }
    }

    fn process(&mut self, instruction: &hayro_syntax::content::Instruction<'_, '_>) {
        let operator = &instruction.operator[..];
        if operator == b"BI" {
            self.invalidate_path();
            return;
        }
        match operator {
            b"m" => {
                self.flush_run();
                self.path_active = true;
                if !self.curve_eligible {
                    return;
                }
                if let Some(vertex) = self.line_vertex(instruction, false) {
                    let point = (vertex.page_x, vertex.page_y);
                    self.current_point = Some(point);
                    self.subpath_start = Some(point);
                    if self.path_eligible {
                        self.run.push(vertex);
                    }
                } else {
                    self.invalidate_path();
                }
            }
            b"l" if self.path_active && self.curve_eligible => {
                if let Some(vertex) = self.line_vertex(instruction, true) {
                    self.current_point = Some((vertex.page_x, vertex.page_y));
                    if self.path_eligible {
                        self.run.push(vertex);
                    }
                } else {
                    self.invalidate_path();
                }
            }
            b"h" if self.path_active => {
                self.flush_run();
                self.current_point = self.subpath_start;
            }
            b"f" | b"F" | b"f*" => self.end_path(true),
            b"S" | b"s" | b"B" | b"B*" | b"b" | b"b*" | b"n" => self.end_path(false),
            b"q" => {
                self.invalidate_path();
                self.stack.push(self.ctm);
            }
            b"Q" => {
                self.invalidate_path();
                if let Some(ctm) = self.stack.pop() {
                    self.ctm = ctm;
                } else {
                    self.ctm = None;
                    self.graphics_state_valid = false;
                }
            }
            b"cm" => {
                self.invalidate_path();
                self.concat_ctm(instruction);
            }
            b"c" | b"v" | b"y" if self.path_active => {
                // The old RDP pass rejects mixed curves. Preserve that behavior
                // while independently collecting safe cubic-to-line rewrites.
                self.run.clear();
                self.pending.clear();
                self.path_eligible = false;
                if self.curve_eligible && !self.curve_operator(instruction) {
                    self.invalidate_path();
                }
            }
            _ if self.path_active => self.invalidate_path(),
            _ => {}
        }
    }

    fn scan(mut self) -> Result<Option<ScanResult>> {
        let incomplete = crate::content_stream::visit_instructions(self.input, |instruction| {
            self.process(instruction);
            Ok(())
        })?;
        if incomplete || !self.stack.is_empty() || !self.graphics_state_valid {
            return Ok(None);
        }
        self.end_path(false);
        Ok(Some((
            self.committed,
            self.vertices_removed,
            self.cubics_simplified,
        )))
    }
}

pub fn simplify_page_polylines(
    document: &mut EditDocument,
    pages: &[crate::ObjectHandle],
    flate_level: crate::FlateLevel,
) -> Result<PolylineSimplificationStats> {
    let mut stats = PolylineSimplificationStats::default();
    for &page in pages {
        let Some(user_unit) = page_user_unit(document, page)? else {
            continue;
        };
        let Some(object) = document.current_owned_object(page)? else {
            continue;
        };
        let Some(dictionary) = object.as_dictionary() else {
            continue;
        };
        let Some(contents) = dictionary.get(b"Contents".as_slice()) else {
            continue;
        };
        let mut decoded = Vec::new();
        decoded_content_value(document, contents, &mut decoded)?;
        let Some((replacements, vertices_removed, cubics_simplified)) =
            Scanner::new(&decoded, user_unit).scan()?
        else {
            continue;
        };
        if replacements.is_empty() || (vertices_removed == 0 && cubics_simplified == 0) {
            continue;
        }
        let Some(simplified) = crate::content_stream::apply_replacements(&decoded, &replacements)
        else {
            continue;
        };
        let before_flate = crate::stream_codec::flate_encoded_len(&decoded, flate_level)?;
        let after_flate = crate::stream_codec::flate_encoded_len(&simplified, flate_level)?;
        let savings = before_flate.saturating_sub(after_flate);
        if savings < MIN_FLATE_SAVINGS_BYTES {
            continue;
        }
        replace_page_content(document, page, simplified.clone())?;
        stats.pages_rewritten = stats.pages_rewritten.saturating_add(1);
        stats.vertices_removed = stats.vertices_removed.saturating_add(vertices_removed);
        stats.cubics_simplified = stats.cubics_simplified.saturating_add(cubics_simplified);
        stats.decoded_bytes_removed = stats
            .decoded_bytes_removed
            .saturating_add(decoded.len().saturating_sub(simplified.len()));
        stats.estimated_flate_bytes_saved =
            stats.estimated_flate_bytes_saved.saturating_add(savings);
    }
    Ok(stats)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rdp_removes_near_collinear_vertices_within_page_error() {
        let run = vec![
            Vertex {
                start: 0,
                end: 1,
                page_x: 0.0,
                page_y: 0.0,
                deletable: false,
            },
            Vertex {
                start: 1,
                end: 2,
                page_x: 1.0,
                page_y: 0.001,
                deletable: true,
            },
            Vertex {
                start: 2,
                end: 3,
                page_x: 2.0,
                page_y: 0.0,
                deletable: true,
            },
        ];
        assert_eq!(Scanner::simplified_keep(&run), vec![true, false, true]);
    }

    #[test]
    fn rdp_keeps_vertex_beyond_page_error() {
        let run = vec![
            Vertex {
                start: 0,
                end: 1,
                page_x: 0.0,
                page_y: 0.0,
                deletable: false,
            },
            Vertex {
                start: 1,
                end: 2,
                page_x: 1.0,
                page_y: 0.01,
                deletable: true,
            },
            Vertex {
                start: 2,
                end: 3,
                page_x: 2.0,
                page_y: 0.0,
                deletable: true,
            },
        ];
        assert_eq!(Scanner::simplified_keep(&run), vec![true, true, true]);
    }

    #[test]
    fn scanner_commits_only_filled_path_simplification() -> Result<()> {
        let fill = b"0 0 m 1 0.001 l 2 0 l 3 0 l 4 0 l 5 0 l 6 0 l 7 0 l f";
        let Some((fill_replacements, fill_removed, _cubics)) = Scanner::new(fill, 1.0).scan()?
        else {
            return Err(crate::Error::Invalid(
                "fill scan unexpectedly incomplete".to_owned(),
            ));
        };
        assert!(fill_removed > 0);
        assert_eq!(fill_replacements.len(), fill_removed);

        let stroke = b"0 0 m 1 0.001 l 2 0 l 3 0 l 4 0 l 5 0 l 6 0 l 7 0 l S";
        let Some((stroke_replacements, stroke_removed, _cubics)) =
            Scanner::new(stroke, 1.0).scan()?
        else {
            return Err(crate::Error::Invalid(
                "stroke scan unexpectedly incomplete".to_owned(),
            ));
        };
        assert_eq!(stroke_removed, 0);
        assert_eq!(stroke_replacements, Vec::<Replacement>::new());
        Ok(())
    }

    #[test]
    fn scanner_applies_ctm_before_error_bound() -> Result<()> {
        let input = b"100 0 0 100 0 0 cm 0 0 m 1 0.001 l 2 0 l 3 0 l 4 0 l 5 0 l 6 0 l 7 0 l f";
        let Some((replacements, removed, _cubics)) = Scanner::new(input, 1.0).scan()? else {
            return Err(crate::Error::Invalid(
                "scaled scan unexpectedly incomplete".to_owned(),
            ));
        };
        assert!(removed > 0);
        let simplified = crate::content_stream::apply_replacements(input, &replacements)
            .ok_or_else(|| crate::Error::Invalid("scaled replacements overlap".to_owned()))?;
        assert!(
            simplified
                .windows(b"1 0.001 l".len())
                .any(|window| window == b"1 0.001 l")
        );
        Ok(())
    }
    #[test]
    fn scanner_rejects_mixed_curve_path() -> Result<()> {
        let input = b"0 0 m 1 0.001 l 2 0 l 3 0 l 4 0 l 5 0 l 6 0 l 7 0 l 7 1 8 2 9 0 c f";
        let Some((replacements, removed, _cubics)) = Scanner::new(input, 1.0).scan()? else {
            return Err(crate::Error::Invalid(
                "curve scan unexpectedly incomplete".to_owned(),
            ));
        };
        assert_eq!(removed, 0);
        assert_eq!(replacements, Vec::<Replacement>::new());
        Ok(())
    }

    #[test]
    fn scanner_applies_user_unit_before_error_bound() -> Result<()> {
        let input = b"0 0 m 1 0.001 l 2 0 l 3 0 l 4 0 l 5 0 l 6 0 l 7 0 l f";
        let Some((replacements, removed, _cubics)) = Scanner::new(input, 100.0).scan()? else {
            return Err(crate::Error::Invalid(
                "UserUnit scan unexpectedly incomplete".to_owned(),
            ));
        };
        assert!(removed > 0);
        let simplified = crate::content_stream::apply_replacements(input, &replacements)
            .ok_or_else(|| crate::Error::Invalid("UserUnit replacements overlap".to_owned()))?;
        assert!(
            simplified
                .windows(b"1 0.001 l".len())
                .any(|window| window == b"1 0.001 l")
        );
        Ok(())
    }

    #[test]
    fn near_linear_cubic_becomes_a_shorter_line_only_on_fill() -> Result<()> {
        let fill = b"0 0 m 1 0.025 2 0.035 3 0 c f";
        let Some((replacements, vertices, cubics)) = Scanner::new(fill, 1.0).scan()? else {
            return Err(crate::Error::Invalid(
                "cubic fill scan incomplete".to_owned(),
            ));
        };
        assert_eq!((vertices, cubics), (0, 1));
        let rewritten = crate::content_stream::apply_replacements(fill, &replacements)
            .ok_or_else(|| crate::Error::Invalid("cubic replacement overlaps".to_owned()))?;
        assert_eq!(rewritten, b"0 0 m 3 0 l f");

        for finish in [b"S".as_slice(), b"B", b"b", b"n"] {
            let mut source = b"0 0 m 1 0.025 2 0.035 3 0 c ".to_vec();
            source.extend_from_slice(finish);
            let Some((replacements, _, cubics)) = Scanner::new(&source, 1.0).scan()? else {
                return Err(crate::Error::Invalid("stroke scan incomplete".to_owned()));
            };
            assert_eq!(cubics, 0);
            assert_eq!(replacements, Vec::<Replacement>::new());
        }
        Ok(())
    }

    #[test]
    fn cubic_page_error_uses_ctm_and_user_unit() -> Result<()> {
        let curve = b"0 0 m 1 0.025 2 0.035 3 0 c f";
        for (source, user_unit, expected) in [
            (curve.as_slice(), 1.0, 1),
            (
                b"10 0 0 10 0 0 cm 0 0 m 1 0.025 2 0.035 3 0 c f".as_slice(),
                1.0,
                0,
            ),
            (curve.as_slice(), 10.0, 0),
        ] {
            let Some((_, _, curves)) = Scanner::new(source, user_unit).scan()? else {
                return Err(crate::Error::Invalid(
                    "scaled curve scan incomplete".to_owned(),
                ));
            };
            assert_eq!(curves, expected);
        }
        Ok(())
    }

    #[test]
    fn cubic_rejects_overshoot_loop_and_clipping() -> Result<()> {
        let invalid = [
            b"0 0 m 4 0 1 0 3 0 c f".as_slice(),
            b"0 0 m 1 0 2 0 0.1 0 c f",
            b"0 0 m 1 0.2 2 0.2 3 0 c f",
            b"0 0 m 1 0.01 2 0.01 3 0 c W n",
            b"0 0 m 1 0.01 2 0.01 3 0 c q Q f",
        ];
        for source in invalid {
            let Some((replacements, _, cubics)) = Scanner::new(source, 1.0).scan()? else {
                return Err(crate::Error::Invalid(
                    "invalid curve scan incomplete".to_owned(),
                ));
            };
            assert_eq!(
                cubics,
                0,
                "unexpected candidate: {}",
                String::from_utf8_lossy(source)
            );
            assert_eq!(replacements, Vec::<Replacement>::new());
        }
        Ok(())
    }

    #[test]
    fn enormous_page_coordinates_cannot_bypass_projection_bounds() {
        assert!(!Scanner::near_linear_cubic(
            (0.0, 0.0),
            (1.2e308, 0.0),
            (1.3e308, 0.0),
            (1.0e308, 0.0),
        ));
    }

    #[test]
    fn scanner_rejects_unbalanced_graphics_state() -> Result<()> {
        let input = b"0 0 m 1 0.001 l 2 0 l 3 0 l 4 0 l 5 0 l 6 0 l 7 0 l f Q";
        assert!(Scanner::new(input, 1.0).scan()?.is_none());

        let input = b"q 0 0 m 1 0.001 l 2 0 l 3 0 l 4 0 l 5 0 l 6 0 l 7 0 l f";
        assert!(Scanner::new(input, 1.0).scan()?.is_none());
        Ok(())
    }
}
