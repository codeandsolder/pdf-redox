//! Processing-mode compaction for repeated expensive clipping paths.
//!
//! Some printer drivers emit a complete, identical clipping polygon around every
//! small paint transaction. Deflate compresses that reasonably well, but PDF
//! renderers still have to parse and rebuild the polygon for every transaction.
//! This pass recognizes adjacent top-level `q ... W/W* n ... Q` blocks with an
//! exact, resource-free clip prefix and rewrites a run so the clip is established
//! once while each original body remains isolated in its own `q`/`Q` scope.

use crate::{
    EditDocument, Result,
    content::{decoded_content_value, replace_page_content},
};

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

#[derive(Debug, Clone)]
struct ClipBlock {
    start: usize,
    end: usize,
    restore_start: usize,
    clip_prefix_end: Option<usize>,
    path_clear_at_end: bool,
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
            });
        }
        self.block_valid = true;
        self.clip_prefix_state = ClipPrefixState::Path;
    }

    fn instruction(&mut self, instruction: &hayro_syntax::content::Instruction<'_, '_>) {
        let operator = &instruction.operator[..];
        let operator_span = instruction.operator_span();
        let start = instruction
            .operand_spans()
            .next()
            .map_or(operator_span.start, |span| span.start);
        let end = operator_span.end;
        let has_operands = instruction.operands().next().is_some();

        if self.depth == 0 {
            if operator == b"Q" {
                self.page_valid = false;
                return;
            }
            self.track_path(operator);
            if operator == b"q" && !has_operands {
                self.open_outer_block(start);
            }
            return;
        }

        self.track_path(operator);
        self.track_clip_prefix(instruction, operator, end);
        match operator {
            b"q" => {
                if has_operands {
                    self.block_valid = false;
                }
                self.depth = self.depth.saturating_add(1);
            }
            b"Q" => {
                if has_operands {
                    self.block_valid = false;
                }
                self.depth = self.depth.saturating_sub(1);
                if self.depth == 0 {
                    self.close_outer_block(operator_span.start, end);
                }
            }
            _ => {}
        }
    }

    fn scan(input: &[u8]) -> Result<Option<Vec<ClipBlock>>> {
        let mut scanner = Self::new();
        let incomplete = crate::content_stream::visit_instructions(input, |instruction| {
            scanner.instruction(instruction);
            Ok(())
        })?;
        if incomplete || !scanner.page_valid || scanner.depth != 0 {
            return Ok(None);
        }
        Ok(Some(scanner.blocks))
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

const fn gap_is_trivia(bytes: &[u8]) -> bool {
    let mut cursor = 0usize;
    while cursor < bytes.len() {
        match bytes[cursor] {
            0 | b'\t' | b'\n' | 0x0c | b'\r' | b' ' => cursor += 1,
            b'%' => {
                cursor += 1;
                while cursor < bytes.len() && !matches!(bytes[cursor], b'\n' | b'\r') {
                    cursor += 1;
                }
            }
            _ => return false,
        }
    }
    true
}

fn blocks_contiguous(input: &[u8], left: &ClipBlock, right: &ClipBlock) -> bool {
    left.end <= right.start && gap_is_trivia(input.get(left.end..right.start).unwrap_or_default())
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
                || clip_prefix(input, candidate) != Some(prefix)
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

    let mut output = Vec::with_capacity(last.end.saturating_sub(first.start));
    output.extend_from_slice(prefix);
    output.push(b'\n');
    for block in selected {
        let clip_end = block.clip_prefix_end?;
        if clip_end > block.restore_start || block.restore_start > block.end {
            return None;
        }
        output.extend_from_slice(b"q\n");
        output.extend_from_slice(input.get(clip_end..block.restore_start)?);
        output.extend_from_slice(b"\nQ\n");
    }
    output.extend_from_slice(b"Q\n");

    let decoded_savings = last
        .end
        .saturating_sub(first.start)
        .saturating_sub(output.len());
    if decoded_savings < MIN_DECODED_SAVINGS {
        return None;
    }
    Some(((first.start, last.end, output), decoded_savings))
}

fn apply_replacements(input: &[u8], replacements: &[Replacement]) -> Option<Vec<u8>> {
    let mut replacements = replacements.to_vec();
    replacements.sort_unstable_by_key(|(start, _, _)| *start);
    let mut output = Vec::with_capacity(input.len());
    let mut cursor = 0usize;
    for (start, end, replacement) in replacements {
        if start < cursor || start > end || end > input.len() {
            return None;
        }
        output.extend_from_slice(input.get(cursor..start)?);
        output.extend_from_slice(&replacement);
        cursor = end;
    }
    output.extend_from_slice(input.get(cursor..)?);
    Some(output)
}

fn compressed_len(bytes: &[u8], level: crate::FlateLevel) -> Result<usize> {
    Ok(crate::stream_codec::encode_flate(bytes, level)?.len())
}

pub fn hoist_repeated_clip_prefixes(
    document: &mut EditDocument,
    flate_level: crate::FlateLevel,
) -> Result<RepeatedClipStats> {
    let mut stats = RepeatedClipStats::default();
    for page in document.page_handles()? {
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
        let Some(blocks) = Scanner::scan(&decoded)? else {
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
        let Some(rewritten) = apply_replacements(&decoded, &replacements) else {
            continue;
        };
        let before_flate = compressed_len(&decoded, flate_level)?;
        let after_flate = compressed_len(&rewritten, flate_level)?;
        if after_flate > before_flate {
            continue;
        }

        replace_page_content(document, page, rewritten)?;
        stats.pages_rewritten = stats.pages_rewritten.saturating_add(1);
        stats.runs_hoisted = stats.runs_hoisted.saturating_add(run_count);
        stats.blocks_hoisted = stats.blocks_hoisted.saturating_add(block_count);
        stats.decoded_bytes_removed = stats.decoded_bytes_removed.saturating_add(decoded_savings);
        stats.estimated_flate_bytes_saved = stats
            .estimated_flate_bytes_saved
            .saturating_add(before_flate.saturating_sub(after_flate));
    }
    Ok(stats)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn large_clip() -> String {
        "0 0 m ".to_owned() + &"1 0 l ".repeat(3000) + "1 1 l h W* n "
    }

    #[test]
    fn hoists_exact_repeated_clip_and_keeps_each_body_isolated() -> Result<()> {
        let clip = large_clip();
        let input = format!("q {clip}0 0 1 1 re f Q q {clip}2 0 1 1 re f Q");
        let Some(blocks) = Scanner::scan(input.as_bytes())? else {
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
        let Some(rewritten) = apply_replacements(input.as_bytes(), &[(start, end, replacement)])
        else {
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
    fn rejects_run_when_a_body_leaves_a_current_path() -> Result<()> {
        let clip = large_clip();
        let input = format!("q {clip}0 0 m Q q {clip}2 0 1 1 re f Q");
        let Some(blocks) = Scanner::scan(input.as_bytes())? else {
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
        let Some(blocks) = Scanner::scan(input.as_bytes())? else {
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
        let Some(blocks) = Scanner::scan(input.as_bytes())? else {
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
        let Some(rewritten) = apply_replacements(input.as_bytes(), &[replacement]) else {
            return Err(crate::Error::Invalid(
                "clip fixture reparse failed".to_owned(),
            ));
        };
        let Some(rewritten_blocks) = Scanner::scan(&rewritten)? else {
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
        let Some(blocks) = Scanner::scan(input.as_bytes())? else {
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
        let Some(blocks) = Scanner::scan(input.as_bytes())? else {
            return Err(crate::Error::Invalid(
                "clip fixture did not parse".to_owned(),
            ));
        };
        assert!(repeated_runs(input.as_bytes(), &blocks).is_empty());
        Ok(())
    }

    #[test]
    fn rejects_unbalanced_graphics_state() -> Result<()> {
        let clip = large_clip();
        let input = format!("q {clip}0 0 1 1 re f");
        assert!(Scanner::scan(input.as_bytes())?.is_none());
        Ok(())
    }
}
