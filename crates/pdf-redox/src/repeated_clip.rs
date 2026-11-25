//! Exact, goal-independent compaction for repeated expensive clipping paths.
//!
//! Some printer drivers emit a complete, identical clipping polygon around every
//! small paint transaction. Deflate compresses that reasonably well, but PDF
//! renderers still have to parse and rebuild the polygon for every transaction.
//! This pass recognizes adjacent top-level `q ... W/W* n ... Q` blocks with an
//! exact, resource-free clip prefix and rewrites a run so the clip is established
//! once while each original body remains isolated in its own `q`/`Q` scope.

use crate::content::page_content_if_present;
use crate::content_stream::trivia_only;

use crate::{EditDocument, OptimizationGoal, Result, content::PageRewriteResult};

const MIN_CLIP_PREFIX_BYTES: usize = 4 * 1024;
const MIN_RUN_BLOCKS: usize = 2;
const MIN_DECODED_SAVINGS: usize = 16 * 1024;

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RepeatedClipStats {
    pub pages_rewritten: usize,
    pub runs_hoisted: usize,
    pub blocks_hoisted: usize,
    pub decoded_bytes_removed: usize,
    pub estimated_flate_bytes_saved: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ClipBlock {
    start: usize,
    end: usize,
    restore_start: usize,
    clip_prefix_end: Option<usize>,
    path_clear_at_end: bool,
    // The scanner proved byte-for-byte equality with the preceding emitted block.
    prefix_reused: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PathState {
    Clear,
    Active,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ClipPrefixState {
    Path,
    AwaitingEnd,
    Done(usize),
    Invalid,
}

struct Scanner {
    depth: usize,
    page_valid: bool,
    block_start: Option<usize>,
    block_valid: bool,
    clip_prefix_state: ClipPrefixState,
    prefix_reused: bool,
    path_state: PathState,
    blocks: Vec<ClipBlock>,
}

impl Scanner {
    const fn new() -> Self {
        Self {
            depth: 0,
            page_valid: true,
            block_start: None,
            block_valid: true,
            clip_prefix_state: ClipPrefixState::Path,
            prefix_reused: false,
            path_state: PathState::Clear,
            blocks: Vec::new(),
        }
    }

    const fn track_path(&mut self, operator: &[u8]) {
        match operator {
            b"m" | b"l" | b"c" | b"v" | b"y" | b"re" => {
                self.path_state = PathState::Active;
            }
            b"n" | b"S" | b"s" | b"f" | b"F" | b"f*" | b"B" | b"B*" | b"b" | b"b*" => {
                self.path_state = PathState::Clear;
            }
            _ => {}
        }
    }

    fn numeric_operands(
        instruction: &hayro_syntax::content::Instruction<'_, '_>,
        expected: usize,
    ) -> bool {
        let mut count = 0usize;
        for operand in instruction.operands() {
            let hayro_syntax::object::Object::Number(number) = operand else {
                return false;
            };
            if !number.as_f64().is_finite() {
                return false;
            }
            count = count.saturating_add(1);
        }
        count == expected
    }

    fn clip_prefix_instruction_valid(
        instruction: &hayro_syntax::content::Instruction<'_, '_>,
        operator: &[u8],
    ) -> bool {
        match operator {
            b"m" | b"l" => Self::numeric_operands(instruction, 2),
            b"c" => Self::numeric_operands(instruction, 6),
            b"v" | b"y" | b"re" => Self::numeric_operands(instruction, 4),
            b"h" | b"W" | b"W*" | b"n" => instruction.operands().next().is_none(),
            _ => false,
        }
    }

    fn track_clip_prefix(
        &mut self,
        instruction: &hayro_syntax::content::Instruction<'_, '_>,
        operator: &[u8],
        end: usize,
    ) {
        if self.depth != 1 {
            return;
        }
        match self.clip_prefix_state {
            ClipPrefixState::Path => {
                if !Self::clip_prefix_instruction_valid(instruction, operator) {
                    self.clip_prefix_state = ClipPrefixState::Invalid;
                    return;
                }
                match operator {
                    b"m" | b"l" | b"c" | b"v" | b"y" | b"h" | b"re" => {}
                    b"W" | b"W*" if matches!(self.path_state, PathState::Active) => {
                        self.clip_prefix_state = ClipPrefixState::AwaitingEnd;
                    }
                    _ => self.clip_prefix_state = ClipPrefixState::Invalid,
                }
            }
            ClipPrefixState::AwaitingEnd => {
                if operator == b"n" && Self::clip_prefix_instruction_valid(instruction, operator) {
                    self.clip_prefix_state = ClipPrefixState::Done(end);
                } else {
                    self.clip_prefix_state = ClipPrefixState::Invalid;
                }
            }
            ClipPrefixState::Done(_) | ClipPrefixState::Invalid => {}
        }
    }

    const fn open_outer_block(&mut self, start: usize) {
        self.depth = 1;
        self.block_start = Some(start);
        self.block_valid = true;
        self.prefix_reused = false;
        self.clip_prefix_state = if matches!(self.path_state, PathState::Clear) {
            ClipPrefixState::Path
        } else {
            ClipPrefixState::Invalid
        };
    }

    fn close_outer_block(&mut self, restore_start: usize, end: usize) {
        let Some(start) = self.block_start.take() else {
            self.page_valid = false;
            return;
        };
        if self.block_valid && end > start {
            self.blocks.push(ClipBlock {
                start,
                end,
                restore_start,
                clip_prefix_end: match self.clip_prefix_state {
                    ClipPrefixState::Done(end) => Some(end),
                    _ => None,
                },
                path_clear_at_end: matches!(self.path_state, PathState::Clear),
                prefix_reused: self.prefix_reused,
            });
        }
        self.block_valid = true;
        self.clip_prefix_state = ClipPrefixState::Path;
    }

    /// Return true only when a new outer block starts with a clear path.
    fn instruction(
        &mut self,
        instruction: &hayro_syntax::content::Instruction<'_, '_>,
        base: usize,
    ) -> bool {
        let operator = &instruction.operator[..];
        let operator_span = instruction.operator_span();
        let end = base + operator_span.end;

        if self.depth == 0 {
            if operator == b"Q" {
                self.page_valid = false;
                return false;
            }
            self.track_path(operator);
            if operator == b"q" && instruction.operand_spans().len() == 0 {
                self.open_outer_block(base + operator_span.start);
                return self.clip_prefix_state == ClipPrefixState::Path;
            }
            return false;
        }

        self.track_path(operator);
        self.track_clip_prefix(instruction, operator, end);
        match operator {
            b"q" => {
                if instruction.operand_spans().len() != 0 {
                    self.block_valid = false;
                }
                self.depth = self.depth.saturating_add(1);
            }
            b"Q" => {
                if instruction.operand_spans().len() != 0 {
                    self.block_valid = false;
                }
                self.depth = self.depth.saturating_sub(1);
                if self.depth == 0 {
                    self.close_outer_block(base + operator_span.start, end);
                }
            }
            _ => {}
        }
        false
    }

    /// Reuse a previous fully parsed prefix only at an independently parsed
    /// outer q with no pending path. The prefix ends in an operand-free n.
    /// Require the following byte to terminate that operator in this copy too.
    fn reuse_clip_prefix(&mut self, input: &[u8]) -> Option<usize> {
        let start = self.block_start?;
        let prefix = clip_prefix(input, self.blocks.last()?)?;
        if prefix.len() < MIN_CLIP_PREFIX_BYTES {
            return None;
        }
        let suffix = input.get(start..)?.strip_prefix(prefix)?;
        if suffix
            .first()
            .is_some_and(|&byte| !crate::content_stream::token_boundary(byte))
        {
            return None;
        }
        let end = input.len() - suffix.len();
        self.clip_prefix_state = ClipPrefixState::Done(end);
        self.prefix_reused = true;
        self.path_state = PathState::Clear;
        Some(end)
    }

    fn finish(self, incomplete: bool) -> Option<Vec<ClipBlock>> {
        if incomplete || !self.page_valid || self.depth != 0 {
            return None;
        }
        Some(self.blocks)
    }

    fn scan(input: &[u8]) -> Option<Vec<ClipBlock>> {
        let mut scanner = Self::new();
        let mut base = 0;
        let mut iter = hayro_syntax::content::UntypedIter::new(input);
        while let Some(instruction) = iter.next() {
            if scanner.instruction(&instruction, base)
                && let Some(end) = scanner.reuse_clip_prefix(input)
            {
                // The validated n ended with an empty operand stack. Restart
                // at that instruction boundary and retain absolute byte spans.
                base = end;
                iter = hayro_syntax::content::UntypedIter::new(&input[base..]);
            }
        }
        scanner.finish(!iter.is_at_end())
    }
}

#[derive(Debug, Clone, Copy)]
struct ClipRun {
    start_block: usize,
    end_block: usize,
}

fn clip_prefix<'a>(input: &'a [u8], block: &ClipBlock) -> Option<&'a [u8]> {
    input.get(block.start..block.clip_prefix_end?)
}

fn blocks_contiguous(input: &[u8], left: &ClipBlock, right: &ClipBlock) -> bool {
    left.end <= right.start && trivia_only(input.get(left.end..right.start).unwrap_or_default())
}

fn repeated_runs(input: &[u8], blocks: &[ClipBlock]) -> Vec<ClipRun> {
    let mut runs = Vec::new();
    let mut cursor = 0usize;
    while cursor < blocks.len() {
        let first = &blocks[cursor];
        let Some(prefix) = clip_prefix(input, first) else {
            cursor += 1;
            continue;
        };
        if prefix.len() < MIN_CLIP_PREFIX_BYTES || !first.path_clear_at_end {
            cursor += 1;
            continue;
        }

        let mut end = cursor.saturating_add(1);
        while end < blocks.len() {
            let previous = &blocks[end - 1];
            let candidate = &blocks[end];
            if !candidate.path_clear_at_end
                || !blocks_contiguous(input, previous, candidate)
                || (!candidate.prefix_reused && clip_prefix(input, candidate) != Some(prefix))
            {
                break;
            }
            end = end.saturating_add(1);
        }
        if end.saturating_sub(cursor) >= MIN_RUN_BLOCKS {
            runs.push(ClipRun {
                start_block: cursor,
                end_block: end,
            });
            cursor = end;
        } else {
            cursor += 1;
        }
    }
    runs
}

type Replacement = (usize, usize, Vec<u8>);

fn hoisted_run(input: &[u8], blocks: &[ClipBlock], run: ClipRun) -> Option<(Replacement, usize)> {
    let selected = blocks.get(run.start_block..run.end_block)?;
    let first = selected.first()?;
    let last = selected.last()?;
    let prefix = clip_prefix(input, first)?;

    // Measure the exact output first. Large repeated prefixes can make the
    // original span orders of magnitude larger than its replacement.
    let output_len = selected
        .iter()
        .try_fold(prefix.len().checked_add(3)?, |total, block| {
            let clip_end = block.clip_prefix_end?;
            if clip_end > block.restore_start || block.restore_start > block.end {
                return None;
            }
            total
                .checked_add(input.get(clip_end..block.restore_start)?.len())?
                .checked_add(5)
        })?;
    let decoded_savings = last.end.checked_sub(first.start)?.checked_sub(output_len)?;
    if decoded_savings < MIN_DECODED_SAVINGS {
        return None;
    }
    let mut output = Vec::new();
    output.try_reserve_exact(output_len).ok()?;
    output.extend_from_slice(prefix);
    output.push(b'\n');
    for block in selected {
        output.extend_from_slice(b"q\n");
        output.extend_from_slice(input.get(block.clip_prefix_end?..block.restore_start)?);
        output.extend_from_slice(b"\nQ\n");
    }
    output.extend_from_slice(b"Q\n");
    Some(((first.start, last.end, output), decoded_savings))
}

/// Size mode only admits strictly smaller compressed page streams. Processing
/// also accepts equal-sized output because eliminating repeated clip work is
/// valuable independently of PDF bytes.
const fn clip_rewrite_budget(before_encoded: usize, goal: OptimizationGoal) -> Option<usize> {
    match goal {
        OptimizationGoal::Size => before_encoded.checked_sub(1),
        OptimizationGoal::Processing => Some(before_encoded),
    }
}

pub fn hoist_repeated_clip_prefixes(
    document: &mut EditDocument,
    pages: &[crate::ObjectHandle],
    flate_level: crate::FlateLevel,
    goal: OptimizationGoal,
    reuse: &mut crate::flate::FlateReuseCache,
) -> Result<PageRewriteResult<RepeatedClipStats>> {
    let mut result: PageRewriteResult<RepeatedClipStats> = PageRewriteResult::default();
    for &page in pages {
        let Some(decoded) = page_content_if_present(document, page)? else {
            continue;
        };
        let Some(blocks) = Scanner::scan(&decoded) else {
            continue;
        };
        let runs = repeated_runs(&decoded, &blocks);
        if runs.is_empty() {
            continue;
        }

        let mut replacements = Vec::new();
        let mut run_count = 0usize;
        let mut block_count = 0usize;
        let mut decoded_savings = 0usize;
        for run in runs {
            let blocks_in_run = run.end_block.saturating_sub(run.start_block);
            let Some((replacement, savings)) = hoisted_run(&decoded, &blocks, run) else {
                continue;
            };
            replacements.push(replacement);
            run_count = run_count.saturating_add(1);
            block_count = block_count.saturating_add(blocks_in_run);
            decoded_savings = decoded_savings.saturating_add(savings);
        }
        if replacements.is_empty() {
            continue;
        }
        let Some(rewritten) = crate::content_stream::apply_replacements(&decoded, &replacements)
        else {
            continue;
        };
        let before_flate = reuse.encoded_len(&decoded, flate_level)?;
        let Some(budget) = clip_rewrite_budget(before_flate, goal) else {
            continue;
        };
        let Some(encoded) = reuse.encode_capped(&rewritten, flate_level, budget)? else {
            continue;
        };

        let after_flate = encoded.len();
        reuse.install_page(document, page, rewritten, encoded, flate_level)?;
        result.stats.pages_rewritten = result.stats.pages_rewritten.saturating_add(1);
        result.rewritten_pages.insert(page);
        result.stats.runs_hoisted = result.stats.runs_hoisted.saturating_add(run_count);
        result.stats.blocks_hoisted = result.stats.blocks_hoisted.saturating_add(block_count);
        result.stats.decoded_bytes_removed = result
            .stats
            .decoded_bytes_removed
            .saturating_add(decoded_savings);
        result.stats.estimated_flate_bytes_saved = result
            .stats
            .estimated_flate_bytes_saved
            .saturating_add(before_flate.saturating_sub(after_flate));
    }
    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::ClassicPdfBuilder;

    #[test]
    fn clip_admission_is_goal_specific_at_encoded_size_equality() {
        assert_eq!(clip_rewrite_budget(100, OptimizationGoal::Size), Some(99));
        assert_eq!(
            clip_rewrite_budget(100, OptimizationGoal::Processing),
            Some(100)
        );
        assert_eq!(clip_rewrite_budget(0, OptimizationGoal::Size), None);
        assert_eq!(
            clip_rewrite_budget(0, OptimizationGoal::Processing),
            Some(0)
        );
    }

    fn large_clip() -> String {
        "0 0 m ".to_owned() + &"1 0 l ".repeat(3000) + "1 1 l h W* n "
    }

    #[test]
    fn cached_clip_prefix_scan_matches_full_parser() -> Result<()> {
        let clip = large_clip();
        let trimmed = clip.trim_end();
        let changed = clip.replacen("1 0 l", "2 0 l", 1);
        let inputs = [
            format!("q {clip}0 0 1 1 re f Q q {clip}2 0 1 1 re f Q q {clip}3 0 1 1 re f Q"),
            format!("q {clip}0 0 1 1 re f Q %comment\nq {trimmed}%comment\n2 0 1 1 re f Q"),
            format!("q {clip}0 0 1 1 re f Q q {trimmed}X 2 0 1 1 re f Q"),
            format!("q {clip}0 0 1 1 re f Q q {changed}2 0 1 1 re f Q"),
            format!("q {clip}0 0 m Q q {clip}2 0 1 1 re f Q"),
            format!("q {clip}0 0 1 1 re f Q q {clip}q 2 0 1 1 re f Q Q"),
            format!("q {clip}0 0 1 1 re f Q q {clip}2 0 1 1 re f"),
        ];
        for input in inputs {
            let mut scanner = Scanner::new();
            let incomplete =
                crate::content_stream::visit_instructions(input.as_bytes(), |instruction| {
                    scanner.instruction(instruction, 0);
                    Ok(())
                })?;
            let expected = scanner.finish(incomplete);
            let mut actual = Scanner::scan(input.as_bytes());
            if let Some(blocks) = &mut actual {
                for index in 0..blocks.len() {
                    if blocks[index].prefix_reused {
                        assert!(index > 0);
                        assert_eq!(
                            clip_prefix(input.as_bytes(), &blocks[index]),
                            clip_prefix(input.as_bytes(), &blocks[index - 1])
                        );
                        blocks[index].prefix_reused = false;
                    }
                }
            }
            assert_eq!(actual, expected);
        }
        Ok(())
    }

    #[test]
    fn hoists_exact_repeated_clip_and_keeps_each_body_isolated() -> Result<()> {
        let clip = large_clip();
        let input = format!("q {clip}0 0 1 1 re f Q q {clip}2 0 1 1 re f Q");
        let Some(blocks) = Scanner::scan(input.as_bytes()) else {
            return Err(crate::Error::Invalid(
                "clip fixture did not parse".to_owned(),
            ));
        };
        assert_eq!(blocks.len(), 2);
        let runs = repeated_runs(input.as_bytes(), &blocks);
        assert_eq!(runs.len(), 1);
        let Some(((start, end, replacement), savings)) =
            hoisted_run(input.as_bytes(), &blocks, runs[0])
        else {
            return Err(crate::Error::Invalid(
                "clip fixture was not hoisted".to_owned(),
            ));
        };
        assert!(savings >= MIN_DECODED_SAVINGS);
        let Some(rewritten) = crate::content_stream::apply_replacements(
            input.as_bytes(),
            &[(start, end, replacement)],
        ) else {
            return Err(crate::Error::Invalid(
                "clip fixture rewrite failed".to_owned(),
            ));
        };
        let rewritten = String::from_utf8_lossy(&rewritten);
        assert_eq!(rewritten.matches("W*").count(), 1);
        assert_eq!(rewritten.matches(" re f").count(), 2);
        Ok(())
    }

    #[test]
    fn reports_exact_rewritten_pages() -> Result<()> {
        let clip = large_clip();
        let repeated = format!("q {clip}0 0 1 1 re f Q q {clip}2 0 1 1 re f Q");
        let mut pdf = ClassicPdfBuilder::new();
        pdf.object(1, b"<< /Type /Catalog /Pages 2 0 R >>")?;
        pdf.object(2, b"<< /Type /Pages /Kids [3 0 R 4 0 R] /Count 2 >>")?;
        pdf.object(
            3,
            b"<< /Type /Page /Parent 2 0 R /MediaBox [0 0 100 100] /Contents 5 0 R >>",
        )?;
        pdf.object(
            4,
            b"<< /Type /Page /Parent 2 0 R /MediaBox [0 0 100 100] /Contents 6 0 R >>",
        )?;
        pdf.stream(5, b"", repeated.as_bytes())?;
        pdf.stream(6, b"", b"0 0 1 1 re f")?;
        let mut document = EditDocument::from_bytes(pdf.finish(1)?)?;
        let pages = document.page_handles()?;
        let rewritten_page = pages[0];

        let result = hoist_repeated_clip_prefixes(
            &mut document,
            &pages,
            crate::FlateLevel::PROCESSING,
            OptimizationGoal::Processing,
            &mut crate::flate::FlateReuseCache::default(),
        )?;

        assert_eq!(result.stats.pages_rewritten, 1);
        assert_eq!(
            result.rewritten_pages,
            std::collections::BTreeSet::from([rewritten_page])
        );
        Ok(())
    }

    #[test]
    fn rejects_run_when_a_body_leaves_a_current_path() -> Result<()> {
        let clip = large_clip();
        let input = format!("q {clip}0 0 m Q q {clip}2 0 1 1 re f Q");
        let Some(blocks) = Scanner::scan(input.as_bytes()) else {
            return Err(crate::Error::Invalid(
                "clip fixture did not parse".to_owned(),
            ));
        };
        assert_eq!(blocks.len(), 2);
        assert!(!blocks[0].path_clear_at_end);
        assert!(repeated_runs(input.as_bytes(), &blocks).is_empty());
        Ok(())
    }

    #[test]
    fn rejects_non_trivia_between_candidate_blocks() -> Result<()> {
        let clip = large_clip();
        let input = format!("q {clip}0 0 1 1 re f Q 1 0 0 rg q {clip}2 0 1 1 re f Q");
        let Some(blocks) = Scanner::scan(input.as_bytes()) else {
            return Err(crate::Error::Invalid(
                "clip fixture did not parse".to_owned(),
            ));
        };
        assert_eq!(blocks.len(), 2);
        assert!(repeated_runs(input.as_bytes(), &blocks).is_empty());
        Ok(())
    }

    #[test]
    fn hoist_is_idempotent() -> Result<()> {
        let clip = large_clip();
        let input = format!("q {clip}0 0 1 1 re f Q q {clip}2 0 1 1 re f Q");
        let Some(blocks) = Scanner::scan(input.as_bytes()) else {
            return Err(crate::Error::Invalid(
                "clip fixture did not parse".to_owned(),
            ));
        };
        let runs = repeated_runs(input.as_bytes(), &blocks);
        let Some((replacement, _)) = hoisted_run(input.as_bytes(), &blocks, runs[0]) else {
            return Err(crate::Error::Invalid(
                "clip fixture was not hoisted".to_owned(),
            ));
        };
        let Some(rewritten) =
            crate::content_stream::apply_replacements(input.as_bytes(), &[replacement])
        else {
            return Err(crate::Error::Invalid(
                "clip fixture reparse failed".to_owned(),
            ));
        };
        let Some(rewritten_blocks) = Scanner::scan(&rewritten) else {
            return Err(crate::Error::Invalid(
                "clip fixture did not reparse".to_owned(),
            ));
        };
        assert!(repeated_runs(&rewritten, &rewritten_blocks).is_empty());
        Ok(())
    }

    #[test]
    fn rejects_malformed_clip_operands() -> Result<()> {
        let clip = "0 0 m ".to_owned() + &"1 0 l ".repeat(3000) + "1 W* n ";
        let input = format!("q {clip}0 0 1 1 re f Q q {clip}2 0 1 1 re f Q");
        let Some(blocks) = Scanner::scan(input.as_bytes()) else {
            return Err(crate::Error::Invalid(
                "clip fixture did not parse".to_owned(),
            ));
        };
        assert!(repeated_runs(input.as_bytes(), &blocks).is_empty());
        Ok(())
    }

    #[test]
    fn rejects_stateful_clip_prefix() -> Result<()> {
        let clip = large_clip();
        let input =
            format!("q 1 0 0 1 0 0 cm {clip}0 0 1 1 re f Q q 1 0 0 1 0 0 cm {clip}2 0 1 1 re f Q");
        let Some(blocks) = Scanner::scan(input.as_bytes()) else {
            return Err(crate::Error::Invalid(
                "clip fixture did not parse".to_owned(),
            ));
        };
        assert!(repeated_runs(input.as_bytes(), &blocks).is_empty());
        Ok(())
    }

    #[test]
    fn rejects_unbalanced_graphics_state() {
        let clip = large_clip();
        let input = format!("q {clip}0 0 1 1 re f");
        assert!(Scanner::scan(input.as_bytes()).is_none());
    }
}
