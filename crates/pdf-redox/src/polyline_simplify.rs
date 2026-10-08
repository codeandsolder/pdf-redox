//! Processing-mode simplification for grotesquely oversampled native polylines.
//!
//! This deliberately runs after the exact glyph/stroke factoring passes. It only
//! rewrites plain `m`/`l` subpaths that end in a pure fill and rejects mixed,
//! curved, clipped, stroked, malformed, or graphics-state-interleaved paths.
//! Distances are measured after CTM and `/UserUnit`, so the tolerance is in real
//! page points rather than source-coordinate units.

use crate::{
    EditDocument, Result,
    content::{decoded_content_value, page_user_unit, replace_page_content},
    content_stream::{instruction_operands, operand_numbers},
    geometry::Matrix,
};

pub const MAX_POLYLINE_PAGE_ERROR_PT: f64 = 0.005;
const MIN_RUN_POINTS: usize = 8;
const MIN_FLATE_SAVINGS_BYTES: usize = 1024;

type Replacement = (usize, usize, Vec<u8>);
type ScanResult = (Vec<Replacement>, usize);

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PolylineSimplificationStats {
    pub pages_rewritten: usize,
    pub vertices_removed: usize,
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

struct Scanner<'a> {
    input: &'a [u8],
    user_unit: f64,
    ctm: Option<Matrix>,
    graphics_state_valid: bool,
    stack: Vec<Option<Matrix>>,
    run: Vec<Vertex>,
    path_active: bool,
    path_eligible: bool,
    pending: Vec<Replacement>,
    committed: Vec<Replacement>,
    vertices_removed: usize,
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
            pending: Vec::new(),
            committed: Vec::new(),
            vertices_removed: 0,
        }
    }

    fn invalidate_path(&mut self) {
        self.run.clear();
        self.pending.clear();
        if self.path_active {
            self.path_eligible = false;
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
        self.run.clear();
        self.path_active = false;
        self.path_eligible = true;
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
                if !self.path_eligible {
                    return;
                }
                if let Some(vertex) = self.line_vertex(instruction, false) {
                    self.run.push(vertex);
                } else {
                    self.invalidate_path();
                }
            }
            b"l" if self.path_active && self.path_eligible => {
                if let Some(vertex) = self.line_vertex(instruction, true) {
                    self.run.push(vertex);
                } else {
                    self.invalidate_path();
                }
            }
            b"h" if self.path_active => self.flush_run(),
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
            b"c" | b"v" | b"y" | b"re" | b"W" | b"W*" if self.path_active => {
                self.invalidate_path();
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
        Ok(Some((self.committed, self.vertices_removed)))
    }
}

fn apply_replacements(input: &[u8], replacements: &[Replacement]) -> Option<Vec<u8>> {
    let mut replacements = replacements.to_vec();
    replacements.sort_unstable_by_key(|(start, _, _)| *start);
    let mut output = Vec::with_capacity(input.len());
    let mut cursor = 0usize;
    for (start, end, replacement) in replacements {
        if start < cursor || end > input.len() || start > end {
            return None;
        }
        output.extend_from_slice(&input[cursor..start]);
        output.extend_from_slice(&replacement);
        cursor = end;
    }
    output.extend_from_slice(&input[cursor..]);
    Some(output)
}

fn compressed_len(bytes: &[u8], level: crate::FlateLevel) -> Result<usize> {
    Ok(crate::stream_codec::encode_flate(bytes, level)?.len())
}

pub fn simplify_page_polylines(
    document: &mut EditDocument,
    flate_level: crate::FlateLevel,
) -> Result<PolylineSimplificationStats> {
    let mut stats = PolylineSimplificationStats::default();
    for page in document.page_handles()? {
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
        let Some((replacements, vertices_removed)) = Scanner::new(&decoded, user_unit).scan()?
        else {
            continue;
        };
        if replacements.is_empty() || vertices_removed == 0 {
            continue;
        }
        let Some(simplified) = apply_replacements(&decoded, &replacements) else {
            continue;
        };
        let before_flate = compressed_len(&decoded, flate_level)?;
        let after_flate = compressed_len(&simplified, flate_level)?;
        let savings = before_flate.saturating_sub(after_flate);
        if savings < MIN_FLATE_SAVINGS_BYTES {
            continue;
        }
        replace_page_content(document, page, simplified.clone())?;
        stats.pages_rewritten = stats.pages_rewritten.saturating_add(1);
        stats.vertices_removed = stats.vertices_removed.saturating_add(vertices_removed);
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
        let Some((fill_replacements, fill_removed)) = Scanner::new(fill, 1.0).scan()? else {
            return Err(crate::Error::Invalid(
                "fill scan unexpectedly incomplete".to_owned(),
            ));
        };
        assert!(fill_removed > 0);
        assert_eq!(fill_replacements.len(), fill_removed);

        let stroke = b"0 0 m 1 0.001 l 2 0 l 3 0 l 4 0 l 5 0 l 6 0 l 7 0 l S";
        let Some((stroke_replacements, stroke_removed)) = Scanner::new(stroke, 1.0).scan()? else {
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
        let Some((replacements, removed)) = Scanner::new(input, 1.0).scan()? else {
            return Err(crate::Error::Invalid(
                "scaled scan unexpectedly incomplete".to_owned(),
            ));
        };
        assert!(removed > 0);
        let simplified = apply_replacements(input, &replacements)
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
        let input = b"0 0 m 1 0.001 l 2 0 l 3 0 l 4 0 l 5 0 l 6 0 l 7 0 l 7 0 8 0 9 0 c f";
        let Some((replacements, removed)) = Scanner::new(input, 1.0).scan()? else {
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
        let Some((replacements, removed)) = Scanner::new(input, 100.0).scan()? else {
            return Err(crate::Error::Invalid(
                "UserUnit scan unexpectedly incomplete".to_owned(),
            ));
        };
        assert!(removed > 0);
        let simplified = apply_replacements(input, &replacements)
            .ok_or_else(|| crate::Error::Invalid("UserUnit replacements overlap".to_owned()))?;
        assert!(
            simplified
                .windows(b"1 0.001 l".len())
                .any(|window| window == b"1 0.001 l")
        );
        Ok(())
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
