use crate::content::page_content_if_present;
use crate::content_stream::trivia_only;
use crate::{
    EditDocument, ObjectHandle, OwnedDictionary, OwnedObject, Result, StreamData,
    content::{
        effective_page_resources, install_page_resource_bindings, replace_page_content,
        resolved_dictionary,
    },
};
use num_traits::ToPrimitive;
use sha2::Digest as _;
use smallvec::SmallVec;
use std::{
    borrow::Cow,
    collections::{BTreeMap, BTreeSet},
    sync::{Arc, Weak},
};

mod curved_glyphs;

const MAX_SOURCE_PATHS_PER_BATCH: usize = 64;
const MIN_PAGE_CONTENT_BYTES: usize = 64 * 1024;
const MIN_FORM_COLLINEAR_VERTICES_REMOVED: usize = 1024;
const MIN_FORM_COLLINEAR_FLATE_SAVINGS: usize = 4 * 1024;
const MIN_PAINTS_ELIMINATED: usize = 16;
const GEOMETRY_EPSILON: f64 = 1.0e-10;

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PaintBatchStats {
    pub pages_rewritten: usize,
    pub groups_created: usize,
    pub source_paints_batched: usize,
    pub paints_eliminated: usize,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct OutlinedGlyphFactorStats {
    pub pages_rewritten: usize,
    pub fonts_created: usize,
    pub glyphs_created: usize,
    pub occurrences_replaced: usize,
    pub source_paints_replaced: usize,
    pub decoded_bytes_removed: usize,
    pub estimated_flate_bytes_saved: usize,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct MarkedContentCoalesceStats {
    pub pages_rewritten: usize,
    pub boundaries_coalesced: usize,
    pub decoded_bytes_removed: usize,
    pub estimated_flate_bytes_saved: usize,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CollinearPathStats {
    pub pages_rewritten: usize,
    pub forms_rewritten: usize,
    pub vertices_removed: usize,
    pub decoded_bytes_removed: usize,
    pub estimated_flate_bytes_saved: usize,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct StrokeFormFactorStats {
    pub pages_rewritten: usize,
    pub forms_created: usize,
    pub occurrences_replaced: usize,
    pub decoded_bytes_factored: usize,
    pub estimated_flate_bytes_saved: usize,
}

#[derive(Debug, Clone, Copy)]
struct Point {
    x: f64,
    y: f64,
}

#[derive(Debug, Clone, Copy)]
struct Bounds {
    x0: f64,
    y0: f64,
    x1: f64,
    y1: f64,
}

impl Bounds {
    const fn from_point(point: Point) -> Self {
        Self {
            x0: point.x,
            y0: point.y,
            x1: point.x,
            y1: point.y,
        }
    }

    const fn include(&mut self, point: Point) {
        self.x0 = self.x0.min(point.x);
        self.y0 = self.y0.min(point.y);
        self.x1 = self.x1.max(point.x);
        self.y1 = self.y1.max(point.y);
    }

    fn overlaps_interior(self, other: Self) -> bool {
        self.x0 < other.x1 && other.x0 < self.x1 && self.y0 < other.y1 && other.y0 < self.y1
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum FillKind {
    Zero,
    Positive,
    Negative,
    Unknown,
}

/// Only the operand facets consumed by vector passes are retained. Numeric
/// operands are retained only when they can establish a geometry proof.
#[derive(Debug, Clone)]
enum EventOperands {
    Numbers(usize),
    NonFiniteSingle(f64),
    Other,
}

impl EventOperands {
    fn project(
        instruction: &hayro_syntax::content::Instruction<'_, '_>,
        count: usize,
        numbers: &mut Vec<f64>,
    ) -> Self {
        use crate::content_stream::operand_number;
        let start = numbers.len();
        for operand in instruction.operands() {
            let Some(number) = operand_number(operand) else {
                numbers.truncate(start);
                return Self::Other;
            };
            if !number.is_finite() {
                numbers.truncate(start);
                return if count == 1 {
                    Self::NonFiniteSingle(number)
                } else {
                    Self::Other
                };
            }
            numbers.push(number);
        }
        Self::Numbers(start)
    }
}

#[derive(Debug, Clone)]
struct Event {
    start: usize,
    end: usize,
    operator: SmallVec<[u8; 4]>,
    operand_count: usize,
    operands: EventOperands,
}

impl Event {
    fn numbers<'a>(&self, numbers: &'a [f64]) -> &'a [f64] {
        match &self.operands {
            // Projection appends exactly one value per operand from the same
            // parser stack, or rolls the whole instruction back.
            EventOperands::Numbers(start) => &numbers[*start..*start + self.operand_count],
            _ => &[],
        }
    }

    const fn all_operands_numeric(&self) -> bool {
        matches!(self.operands, EventOperands::Numbers(_))
    }

    fn sole_number(&self, numbers: &[f64]) -> Option<f64> {
        match &self.operands {
            EventOperands::Numbers(start) if self.operand_count == 1 => {
                numbers.get(*start).copied()
            }
            EventOperands::NonFiniteSingle(number) => Some(*number),
            _ => None,
        }
    }
}

/// A page-local contiguous numeric arena avoids paying for six inline
/// coordinates in every event or allocating large operand lists individually.
#[derive(Default)]
struct EventTable {
    records: Vec<Event>,
    numbers: Vec<f64>,
}

fn events_for(input: &[u8], forbidden: &[&[u8]]) -> Option<EventTable> {
    let mut events = Some(EventTable::default());
    let incomplete = crate::content_stream::visit_instructions(input, |instruction| {
        let Some(projected) = &mut events else {
            return Ok(());
        };
        if forbidden.contains(&&instruction.operator[..]) {
            // Continue parsing to distinguish a complete disqualified page
            // from an incomplete stream, but retain no unused event table.
            events = None;
            return Ok(());
        }
        let span = instruction.span();
        if &instruction.operator[..] == b"BI" {
            projected.records.push(Event {
                start: span.start,
                end: span.end,
                operator: SmallVec::from_slice(b"__inline_image__"),
                operand_count: 0,
                operands: EventOperands::Other,
            });
        } else {
            let count = instruction.operand_spans().len();
            projected.records.push(Event {
                start: span.start,
                end: instruction.operator_span().end,
                operator: SmallVec::from_slice(&instruction.operator[..]),
                operand_count: count,
                operands: EventOperands::project(instruction, count, &mut projected.numbers),
            });
        }
        Ok(())
    })
    .ok()?;
    (!incomplete).then_some(events.unwrap_or_default())
}

const fn path_operator(operator: &[u8]) -> bool {
    matches!(operator, b"m" | b"l" | b"c" | b"v" | b"y" | b"h" | b"re")
}

fn adjacent(input: &[u8], left: &Event, right: &Event) -> bool {
    left.end <= right.start && input.get(left.end..right.start).is_some_and(trivia_only)
}

#[cfg(test)]
fn page_paint_batching_is_safe(events: &[Event]) -> bool {
    !events
        .iter()
        .any(|event| matches!(event.operator.as_slice(), b"gs" | b"d"))
}

fn cross(a: Point, b: Point, c: Point) -> f64 {
    (b.x - a.x).mul_add(c.y - a.y, -((b.y - a.y) * (c.x - a.x)))
}

fn nearly_zero(value: f64, scale: f64) -> bool {
    value.abs() <= GEOMETRY_EPSILON * scale.max(1.0)
}

const fn points_equal(a: Point, b: Point) -> bool {
    a.x.to_bits() == b.x.to_bits() && a.y.to_bits() == b.y.to_bits()
}

fn orientation(a: Point, b: Point, c: Point) -> Option<i8> {
    let scale = (a.x.abs() + a.y.abs() + b.x.abs() + b.y.abs() + c.x.abs() + c.y.abs()).max(1.0);
    let value = cross(a, b, c);
    if nearly_zero(value, scale * scale) {
        None
    } else if value > 0.0 {
        Some(1)
    } else {
        Some(-1)
    }
}

fn point_on_segment(a: Point, b: Point, p: Point) -> bool {
    let scale = (a.x.abs() + a.y.abs() + b.x.abs() + b.y.abs() + p.x.abs() + p.y.abs()).max(1.0);
    nearly_zero(cross(a, b, p), scale * scale)
        && p.x >= a.x.min(b.x)
        && p.x <= a.x.max(b.x)
        && p.y >= a.y.min(b.y)
        && p.y <= a.y.max(b.y)
}

fn segments_intersect_or_touch(a: Point, b: Point, c: Point, d: Point) -> bool {
    let ab_c = orientation(a, b, c);
    let ab_d = orientation(a, b, d);
    let cd_a = orientation(c, d, a);
    let cd_b = orientation(c, d, b);
    if let (Some(ab_c), Some(ab_d), Some(cd_a), Some(cd_b)) = (ab_c, ab_d, cd_a, cd_b)
        && ab_c != ab_d
        && cd_a != cd_b
    {
        return true;
    }
    (ab_c.is_none() && point_on_segment(a, b, c))
        || (ab_d.is_none() && point_on_segment(a, b, d))
        || (cd_a.is_none() && point_on_segment(c, d, a))
        || (cd_b.is_none() && point_on_segment(c, d, b))
}

fn contour_is_simple(points: &[Point]) -> bool {
    if points.len() < 3 {
        return false;
    }
    let count = points.len();
    for i in 0..count {
        let a = points[i];
        let b = points[(i + 1) % count];
        if points_equal(a, b) {
            continue;
        }
        for j in (i + 1)..count {
            if (i + 1) % count == j || i == (j + 1) % count {
                continue;
            }
            let c = points[j];
            let d = points[(j + 1) % count];
            if points_equal(c, d) {
                continue;
            }
            if segments_intersect_or_touch(a, b, c, d) {
                return false;
            }
        }
    }
    true
}

fn contour_signed_area(points: &[Point]) -> Option<f64> {
    if points.len() < 3 {
        return Some(0.0);
    }
    let mut twice_area = 0.0;
    let mut scale = 1.0f64;
    for (a, b) in points
        .iter()
        .copied()
        .zip(points.iter().copied().cycle().skip(1))
        .take(points.len())
    {
        twice_area += a.x.mul_add(b.y, -(b.x * a.y));
        scale = scale.max(a.x.abs()).max(a.y.abs());
    }
    if !twice_area.is_finite() {
        return None;
    }
    let point_count = f64::from(u32::try_from(points.len()).ok()?);
    if nearly_zero(twice_area, scale * scale * point_count) {
        Some(0.0)
    } else {
        Some(twice_area * 0.5)
    }
}

fn contour_has_area(points: &[Point]) -> bool {
    let Some(first) = points.first().copied() else {
        return false;
    };
    let Some(second) = points
        .iter()
        .copied()
        .find(|point| !points_equal(*point, first))
    else {
        return false;
    };
    points
        .iter()
        .copied()
        .any(|point| orientation(first, second, point).is_some())
}

fn event_points(event: &Event, numbers: &[f64]) -> Option<SmallVec<[Point; 3]>> {
    if !event.all_operands_numeric() {
        return None;
    }
    let mut points = SmallVec::new();
    let args = event.numbers(numbers);
    match (event.operator.as_slice(), args) {
        (b"m" | b"l", [x, y]) => points.push(Point { x: *x, y: *y }),
        (b"c", [_, _, _, _, _, _]) | (b"v" | b"y", [_, _, _, _]) => {
            for [x, y] in args.as_chunks::<2>().0 {
                points.push(Point { x: *x, y: *y });
            }
        }
        (b"re", [x, y, width, height]) => {
            points.push(Point { x: *x, y: *y });
            points.push(Point {
                x: x + width,
                y: y + height,
            });
        }
        (b"h", []) => {}
        _ => return None,
    }
    Some(points)
}

fn path_bounds(events: &[Event], numbers: &[f64]) -> Option<Bounds> {
    let mut bounds: Option<Bounds> = None;
    for event in events {
        let points = event_points(event, numbers)?;
        for point in points {
            if !point.x.is_finite() || !point.y.is_finite() {
                return None;
            }
            match &mut bounds {
                Some(bounds) => bounds.include(point),
                None => bounds = Some(Bounds::from_point(point)),
            }
        }
    }
    bounds
}

fn fill_kind(events: &[Event], numbers: &[f64]) -> FillKind {
    let mut contours = Vec::<Vec<Point>>::new();
    let mut current = Vec::<Point>::new();
    for event in events {
        match (event.operator.as_slice(), event.numbers(numbers)) {
            (b"m", [x, y]) => {
                if !current.is_empty() {
                    contours.push(std::mem::take(&mut current));
                }
                current.push(Point { x: *x, y: *y });
            }
            (b"l", [x, y]) => {
                if current.is_empty() {
                    return FillKind::Unknown;
                }
                current.push(Point { x: *x, y: *y });
            }
            (b"h", _) if event.operand_count == 0 => {
                if !current.is_empty() {
                    contours.push(std::mem::take(&mut current));
                }
            }
            _ => return FillKind::Unknown,
        }
    }
    if !current.is_empty() {
        contours.push(current);
    }
    if contours.is_empty() {
        return FillKind::Zero;
    }

    let mut sign = None;
    for contour in contours {
        if !contour_has_area(&contour) {
            continue;
        }
        if !contour_is_simple(&contour) {
            return FillKind::Unknown;
        }
        let Some(area) = contour_signed_area(&contour) else {
            return FillKind::Unknown;
        };
        if area == 0.0 {
            return FillKind::Unknown;
        }
        let contour_sign = if area > 0.0 { 1i8 } else { -1i8 };
        if let Some(previous) = sign
            && previous != contour_sign
        {
            return FillKind::Unknown;
        }
        sign = Some(contour_sign);
    }

    match sign {
        None => FillKind::Zero,
        Some(1) => FillKind::Positive,
        Some(-1) => FillKind::Negative,
        Some(_) => FillKind::Unknown,
    }
}

#[derive(Debug, Clone)]
struct ClosedFillTransaction {
    start: usize,
    end: usize,
    miter_start: usize,
    miter_end: usize,
    miter_bits: u64,
    body_start: usize,
    body_end: usize,
    body_already_closed: bool,
    bounds: Option<Bounds>,
    fill_kind: FillKind,
}

/// Both closed-fill recognizers require the same adjacent, finite miter
/// assignment inside a saved graphics-state scope.
fn saved_miter_prefix<'a>(
    input: &[u8],
    table: &'a EventTable,
    index: usize,
) -> Option<(&'a Event, &'a Event, u64)> {
    let save = table.records.get(index)?;
    let miter = table.records.get(index + 1)?;
    if save.operator.as_slice() != b"q"
        || save.operand_count != 0
        || miter.operator.as_slice() != b"M"
        || miter.operand_count != 1
        || !adjacent(input, save, miter)
    {
        return None;
    }
    let value = miter.sole_number(&table.numbers)?;
    value.is_finite().then_some((save, miter, value.to_bits()))
}

fn closed_fill_transaction(
    input: &[u8],
    table: &EventTable,
    index: usize,
) -> Option<(ClosedFillTransaction, usize)> {
    let events = &table.records;
    let numbers = &table.numbers;
    let (q, miter, miter_bits) = saved_miter_prefix(input, table, index)?;

    let first_path_index = index + 2;
    let first_path = events.get(first_path_index)?;
    if !path_operator(&first_path.operator) || !adjacent(input, miter, first_path) {
        return None;
    }

    let mut cursor = first_path_index;
    while let Some(next) = events.get(cursor + 1) {
        if path_operator(&next.operator) && adjacent(input, &events[cursor], next) {
            cursor += 1;
        } else {
            break;
        }
    }
    let last_path = events.get(cursor)?;
    let paint = events.get(cursor + 1)?;
    let restore = events.get(cursor + 2)?;
    if paint.operator.as_slice() != b"b"
        || paint.operand_count != 0
        || restore.operator.as_slice() != b"Q"
        || restore.operand_count != 0
        || !adjacent(input, last_path, paint)
        || !adjacent(input, paint, restore)
    {
        return None;
    }

    let path_events = events.get(first_path_index..=cursor)?;
    Some((
        ClosedFillTransaction {
            start: q.start,
            end: restore.end,
            miter_start: miter.start,
            miter_end: miter.end,
            miter_bits,
            body_start: first_path.start,
            body_end: paint.start,
            body_already_closed: last_path.operator.as_slice() == b"h",
            bounds: path_bounds(path_events, numbers),
            fill_kind: fill_kind(path_events, numbers),
        },
        cursor + 3,
    ))
}

fn fill_pair_is_safe(left: &ClosedFillTransaction, right: &ClosedFillTransaction) -> bool {
    match (left.fill_kind, right.fill_kind) {
        (FillKind::Zero, _)
        | (_, FillKind::Zero)
        | (FillKind::Positive, FillKind::Positive)
        | (FillKind::Negative, FillKind::Negative) => true,
        _ => left
            .bounds
            .zip(right.bounds)
            .is_some_and(|(left, right)| !left.overlaps_interior(right)),
    }
}

fn fill_group_accepts(
    group: &[ClosedFillTransaction],
    transaction: &ClosedFillTransaction,
) -> bool {
    group
        .iter()
        .all(|existing| fill_pair_is_safe(existing, transaction))
}

#[derive(Debug, Clone)]
struct StrokeTransaction {
    start: usize,
    end: usize,
    body_end: usize,
}

/// Advance over an entire contiguous path run. If its terminating event
/// cannot be a plain stroke, no suffix of that same run can be one either.
/// Preserve the terminator itself for the next iteration.
fn stroke_transaction(
    input: &[u8],
    events: &[Event],
    index: usize,
) -> (Option<StrokeTransaction>, usize) {
    let Some(first) = events.get(index) else {
        return (None, events.len());
    };
    if !path_operator(&first.operator) {
        return (None, index + 1);
    }
    let mut cursor = index;
    while let Some(next) = events.get(cursor + 1) {
        if path_operator(&next.operator) && adjacent(input, &events[cursor], next) {
            cursor += 1;
        } else {
            break;
        }
    }
    let terminator = cursor + 1;
    let Some(paint) = events.get(terminator) else {
        return (None, terminator);
    };
    if paint.operator.as_slice() != b"S"
        || paint.operand_count != 0
        || !adjacent(input, &events[cursor], paint)
    {
        return (None, terminator);
    }
    (
        Some(StrokeTransaction {
            start: first.start,
            end: paint.end,
            body_end: paint.start,
        }),
        terminator + 1,
    )
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct ContentBatchStats {
    groups_created: usize,
    source_paints_batched: usize,
    paints_eliminated: usize,
}

fn closed_fill_replacement(
    input: &[u8],
    transactions: &[ClosedFillTransaction],
) -> Option<Vec<u8>> {
    let first = transactions.first()?;
    let mut output = Vec::new();
    output.extend_from_slice(b"q\n");
    output.extend_from_slice(input.get(first.miter_start..first.miter_end)?);
    output.extend_from_slice(b"\nq\n");
    for item in transactions {
        output.extend_from_slice(input.get(item.body_start..item.body_end)?);
        if !item.body_already_closed {
            output.extend_from_slice(b" h");
        }
        output.push(b'\n');
    }
    output.extend_from_slice(b"B\nQ\nQ\n");
    Some(output)
}

fn record_closed_fill_group(
    input: &[u8],
    group: &[ClosedFillTransaction],
    replacements: &mut Vec<(usize, usize, Vec<u8>)>,
    stats: &mut ContentBatchStats,
) -> Option<()> {
    if group.len() < 2 {
        return Some(());
    }
    let start = group.first()?.start;
    let end = group.last()?.end;
    replacements.push((start, end, closed_fill_replacement(input, group)?));
    stats.groups_created = stats.groups_created.saturating_add(1);
    stats.source_paints_batched = stats.source_paints_batched.saturating_add(group.len());
    stats.paints_eliminated = stats
        .paints_eliminated
        .saturating_add(group.len().saturating_sub(1));
    Some(())
}

fn batch_closed_fills<'a>(
    input: &'a [u8],
    table: &EventTable,
) -> Option<(Cow<'a, [u8]>, ContentBatchStats)> {
    let mut replacements = Vec::<(usize, usize, Vec<u8>)>::new();
    let mut stats = ContentBatchStats::default();
    let mut index = 0usize;

    while index < table.records.len() {
        let Some((first, mut next_index)) = closed_fill_transaction(input, table, index) else {
            index += 1;
            continue;
        };
        let miter_bits = first.miter_bits;
        let mut group = vec![first];

        while let Some((next, after_next)) = closed_fill_transaction(input, table, next_index) {
            let previous = group.last()?;
            if miter_bits != next.miter_bits
                || !input.get(previous.end..next.start).is_some_and(trivia_only)
            {
                break;
            }
            if group.len() >= MAX_SOURCE_PATHS_PER_BATCH || !fill_group_accepts(&group, &next) {
                record_closed_fill_group(input, &group, &mut replacements, &mut stats)?;
                group = vec![next];
            } else {
                group.push(next);
            }
            next_index = after_next;
        }

        record_closed_fill_group(input, &group, &mut replacements, &mut stats)?;
        index = next_index;
    }

    if replacements.is_empty() {
        return Some((Cow::Borrowed(input), stats));
    }
    let output = crate::content_stream::apply_replacements(input, &replacements)?;
    Some((Cow::Owned(output), stats))
}

fn stroke_replacement(input: &[u8], transactions: &[StrokeTransaction]) -> Option<Vec<u8>> {
    let mut output = Vec::new();
    for chunk in transactions.chunks(MAX_SOURCE_PATHS_PER_BATCH) {
        if chunk.len() == 1 {
            let item = &chunk[0];
            output.extend_from_slice(input.get(item.start..item.end)?);
            continue;
        }
        output.extend_from_slice(b"q\n");
        for item in chunk {
            output.extend_from_slice(input.get(item.start..item.body_end)?);
            output.push(b'\n');
        }
        output.extend_from_slice(b"S\nQ\n");
    }
    Some(output)
}

fn batch_strokes<'a>(
    input: &'a [u8],
    events: &[Event],
) -> Option<(Cow<'a, [u8]>, ContentBatchStats)> {
    let mut replacements = Vec::<(usize, usize, Vec<u8>)>::new();
    let mut stats = ContentBatchStats::default();
    // Discover each path once. Retain instruction indices as well as byte
    // spans so a rejected intervening instruction always breaks the batch.
    let mut cursor = 0usize;
    let mut transactions = std::iter::from_fn(|| {
        while cursor < events.len() {
            let start = cursor;
            let (candidate, next) = stroke_transaction(input, events, start);
            cursor = next;
            if let Some(candidate) = candidate {
                return Some((start, next, candidate));
            }
        }
        None
    })
    .peekable();

    while let Some((_, mut next_index, first)) = transactions.next() {
        let mut run = vec![first];
        while let Some((start_index, _, next)) = transactions.peek() {
            let previous = run.last()?;
            if *start_index != next_index
                || !input.get(previous.end..next.start).is_some_and(trivia_only)
            {
                break;
            }
            let (_, after_next, next) = transactions.next()?;
            run.push(next);
            next_index = after_next;
        }

        if run.len() >= 2 {
            let start = run.first()?.start;
            let end = run.last()?.end;
            let replacement = stroke_replacement(input, &run)?;
            let groups = run
                .chunks(MAX_SOURCE_PATHS_PER_BATCH)
                .filter(|chunk| chunk.len() >= 2)
                .count();
            let output_paints = run.chunks(MAX_SOURCE_PATHS_PER_BATCH).count();
            stats.groups_created = stats.groups_created.saturating_add(groups);
            stats.source_paints_batched = stats.source_paints_batched.saturating_add(run.len());
            stats.paints_eliminated = stats
                .paints_eliminated
                .saturating_add(run.len().saturating_sub(output_paints));
            replacements.push((start, end, replacement));
        }
    }

    if replacements.is_empty() {
        return Some((Cow::Borrowed(input), stats));
    }
    let output = crate::content_stream::apply_replacements(input, &replacements)?;
    Some((Cow::Owned(output), stats))
}

fn batch_content<'a>(
    input: &'a [u8],
    events: &EventTable,
) -> Option<(Cow<'a, [u8]>, ContentBatchStats)> {
    let (filled, fill_stats) = batch_closed_fills(input, events)?;
    let (stroked, stroke_stats) = match filled {
        Cow::Borrowed(filled) => batch_strokes(filled, &events.records)?,
        Cow::Owned(filled) => {
            let stroke_events = events_for(&filled, &[b"gs", b"d"])?;
            let (stroked, stroke_stats) = batch_strokes(&filled, &stroke_events.records)?;
            let stroked = match stroked {
                Cow::Borrowed(_) => Cow::Owned(filled),
                Cow::Owned(stroked) => Cow::Owned(stroked),
            };
            (stroked, stroke_stats)
        }
    };
    Some((
        stroked,
        ContentBatchStats {
            groups_created: fill_stats
                .groups_created
                .saturating_add(stroke_stats.groups_created),
            source_paints_batched: fill_stats
                .source_paints_batched
                .saturating_add(stroke_stats.source_paints_batched),
            paints_eliminated: fill_stats
                .paints_eliminated
                .saturating_add(stroke_stats.paints_eliminated),
        },
    ))
}

fn optional_content_property(
    instruction: &hayro_syntax::content::Instruction<'_, '_>,
) -> Option<Vec<u8>> {
    if &instruction.operator[..] != b"BDC" || instruction.operand_spans().len() != 2 {
        return None;
    }
    let mut operands = instruction.operands();
    if crate::content_stream::operand_name(operands.next()?)? != b"OC" {
        return None;
    }
    Some(crate::content_stream::operand_name(operands.next()?)?.to_vec())
}

/// Only marked-content boundaries require retained state. Other parsed
/// instructions merely terminate adjacency; do not build a page event table.
fn coalesce_adjacent_ocg_content(input: &[u8]) -> Option<(Cow<'_, [u8]>, usize)> {
    let mut stack = Vec::<Option<Vec<u8>>>::new();
    let mut replacements = Vec::<(usize, usize, Vec<u8>)>::new();
    let mut pending_end: Option<std::ops::Range<usize>> = None;
    let incomplete = crate::content_stream::visit_instructions(input, |instruction| {
        let span = instruction.span();
        let operator = &instruction.operator[..];
        let property = optional_content_property(instruction);
        if let Some(end) = pending_end.take() {
            if let (Some(Some(previous)), Some(next)) = (stack.last(), property.as_ref())
                && previous == next
                && input.get(end.end..span.start).is_some_and(trivia_only)
            {
                replacements.push((end.start, instruction.operator_span().end, b"\n".to_vec()));
                return Ok(());
            }
            stack.pop();
        }
        match operator {
            b"BDC" => stack.push(property),
            b"BMC" => stack.push(None),
            b"EMC" => pending_end = Some(span.start..instruction.operator_span().end),
            _ => {}
        }
        Ok(())
    })
    .ok()?;
    if incomplete {
        return None;
    }
    let count = replacements.len();
    if count == 0 {
        return Some((Cow::Borrowed(input), 0));
    }
    Some((
        Cow::Owned(crate::content_stream::apply_replacements(
            input,
            &replacements,
        )?),
        count,
    ))
}

#[derive(Default)]
struct LocalPageRewriteStats {
    pages_rewritten: usize,
    operations: usize,
    decoded_bytes_removed: usize,
    estimated_flate_bytes_saved: usize,
}

#[derive(Clone, Copy)]
enum LocalRewriteKind {
    OptionalContent,
    Collinear,
}

struct LocalRewritePlan<'a> {
    before: Cow<'a, [u8]>,
    after: Vec<u8>,
    operations: usize,
}

impl LocalRewriteKind {
    fn plan(self, decoded: &[u8]) -> Option<LocalRewritePlan<'_>> {
        match self {
            Self::OptionalContent => {
                let (after, operations) = coalesce_adjacent_ocg_content(decoded)?;
                let Cow::Owned(after) = after else {
                    return None;
                };
                Some(LocalRewritePlan {
                    before: Cow::Borrowed(decoded),
                    after,
                    operations,
                })
            }
            Self::Collinear => normalized_collinear_plan(decoded),
        }
    }
}

fn apply_local_page_rewrites(
    document: &mut EditDocument,
    pages: &[ObjectHandle],
    flate_level: crate::FlateLevel,
    kind: LocalRewriteKind,
    reuse: &mut crate::flate::FlateReuseCache,
) -> Result<LocalPageRewriteStats> {
    let mut stats = LocalPageRewriteStats::default();
    for &page in pages {
        let Some(decoded) = page_content_if_present(document, page)? else {
            continue;
        };
        if decoded.len() < MIN_PAGE_CONTENT_BYTES {
            continue;
        }
        let Some(LocalRewritePlan {
            before,
            after,
            operations,
        }) = kind.plan(&decoded)
        else {
            continue;
        };
        if operations == 0 || after.as_slice() == before.as_ref() {
            continue;
        }
        let before_flate = reuse.encoded_len(&before, flate_level)?;
        let Some(encoded) =
            crate::stream_codec::encode_flate_capped(&after, before_flate, flate_level)?
        else {
            continue;
        };
        let removed_bytes = before.len().saturating_sub(after.len());
        let after_flate = encoded.len();
        reuse.install_page(document, page, after, encoded, flate_level)?;
        stats.pages_rewritten = stats.pages_rewritten.saturating_add(1);
        stats.operations = stats.operations.saturating_add(operations);
        stats.decoded_bytes_removed = stats.decoded_bytes_removed.saturating_add(removed_bytes);
        stats.estimated_flate_bytes_saved = stats
            .estimated_flate_bytes_saved
            .saturating_add(before_flate.saturating_sub(after_flate));
    }
    Ok(stats)
}

pub fn coalesce_optional_content(
    document: &mut EditDocument,
    pages: &[ObjectHandle],
    flate_level: crate::FlateLevel,
    reuse: &mut crate::flate::FlateReuseCache,
) -> Result<MarkedContentCoalesceStats> {
    let stats = apply_local_page_rewrites(
        document,
        pages,
        flate_level,
        LocalRewriteKind::OptionalContent,
        reuse,
    )?;
    Ok(MarkedContentCoalesceStats {
        pages_rewritten: stats.pages_rewritten,
        boundaries_coalesced: stats.operations,
        decoded_bytes_removed: stats.decoded_bytes_removed,
        estimated_flate_bytes_saved: stats.estimated_flate_bytes_saved,
    })
}

const STRUCTURAL_GRID_SCALE: f64 = 100.0;
const MAX_OUTLINED_GLYPH_EXTENT_GRID: i64 = 16 * 100;
const MAX_TYPE3_GLYPHS_PER_FONT: usize = 255;
const MIN_OUTLINED_GLYPH_OCCURRENCES: usize = 2;
const MIN_OUTLINED_GLYPH_ESTIMATED_SAVINGS: usize = 16 * 1024;
const MIN_STROKE_FORM_BODY_BYTES: usize = 256;
const MIN_STROKE_FORM_ESTIMATED_SAVINGS: usize = 2 * 1024;
const STROKE_FORM_OVERHEAD_BYTES: usize = 512;
const STROKE_FORM_OCCURRENCE_OVERHEAD_BYTES: usize = 32;

#[derive(Debug, Clone, Copy)]
struct GridBounds {
    x0: i64,
    y0: i64,
    x1: i64,
    y1: i64,
}

impl GridBounds {
    const fn from_point(x: i64, y: i64) -> Self {
        Self {
            x0: x,
            y0: y,
            x1: x,
            y1: y,
        }
    }
    const fn touches(self, other: Self) -> bool {
        self.x0 <= other.x1 && other.x0 <= self.x1 && self.y0 <= other.y1 && other.y0 <= self.y1
    }
    const fn width(self) -> i64 {
        self.x1 - self.x0
    }
    const fn height(self) -> i64 {
        self.y1 - self.y0
    }
    fn include(&mut self, other: Self) {
        self.x0 = self.x0.min(other.x0);
        self.y0 = self.y0.min(other.y0);
        self.x1 = self.x1.max(other.x1);
        self.y1 = self.y1.max(other.y1);
    }
}

#[derive(Debug, Clone, Copy)]
struct GlyphPathPoint {
    operator: u8,
    x: i64,
    y: i64,
}

#[derive(Debug, Clone, Copy)]
struct LinePoint {
    start: usize,
    end: usize,
    x: i64,
    y: i64,
}

fn same_forward_line(a: LinePoint, b: LinePoint, c: LinePoint) -> bool {
    let ab_x = i128::from(b.x) - i128::from(a.x);
    let ab_y = i128::from(b.y) - i128::from(a.y);
    let bc_x = i128::from(c.x) - i128::from(b.x);
    let bc_y = i128::from(c.y) - i128::from(b.y);
    if (ab_x == 0 && ab_y == 0) || (bc_x == 0 && bc_y == 0) {
        return false;
    }
    let cross = ab_x * bc_y - ab_y * bc_x;
    let dot = ab_x * bc_x + ab_y * bc_y;
    cross == 0 && dot > 0
}

fn line_point(
    instruction: &hayro_syntax::content::Instruction<'_, '_>,
    span: std::ops::Range<usize>,
) -> Option<LinePoint> {
    let [x, y] = crate::content_stream::instruction_number_array::<2>(instruction)?;
    Some(LinePoint {
        start: span.start,
        end: span.end,
        x: structural_grid_coordinate(x)?,
        y: structural_grid_coordinate(y)?,
    })
}

/// The retained run is the already simplified path, not a duplicate of
/// every original vertex. Spans refer to either original or normalized bytes.
#[derive(Default)]
struct CollinearScanner {
    run: Vec<LinePoint>,
    replacements: Vec<(usize, usize, &'static [u8])>,
}

impl CollinearScanner {
    fn instruction(
        &mut self,
        instruction: &hayro_syntax::content::Instruction<'_, '_>,
        span: std::ops::Range<usize>,
        contiguous: bool,
    ) {
        match &instruction.operator[..] {
            b"m" => {
                self.run.clear();
                if contiguous && let Some(point) = line_point(instruction, span) {
                    self.run.push(point);
                }
            }
            b"l" if !self.run.is_empty() && contiguous => {
                if let Some(point) = line_point(instruction, span) {
                    self.run.push(point);
                    while let [.., a, b, c] = self.run.as_slice() {
                        if !same_forward_line(*a, *b, *c) {
                            break;
                        }
                        self.replacements.push((b.start, b.end, b""));
                        let length = self.run.len();
                        self.run.remove(length - 2);
                    }
                } else {
                    self.run.clear();
                }
            }
            _ => self.run.clear(),
        }
    }

    fn finish(self, input: &[u8]) -> Option<(Cow<'_, [u8]>, usize)> {
        let count = self.replacements.len();
        if count == 0 {
            return Some((Cow::Borrowed(input), 0));
        }
        Some((
            Cow::Owned(crate::content_stream::apply_replacements(
                input,
                &self.replacements,
            )?),
            count,
        ))
    }
}

fn compact_collinear_line_points(input: &[u8]) -> Option<(Cow<'_, [u8]>, usize)> {
    let mut scanner = CollinearScanner::default();
    let mut previous_end = None;
    let incomplete = crate::content_stream::visit_instructions(input, |instruction| {
        let span = instruction.span();
        let contiguous =
            previous_end.is_none_or(|end| input.get(end..span.start).is_some_and(trivia_only));
        scanner.instruction(
            instruction,
            span.start..instruction.operator_span().end,
            contiguous,
        );
        previous_end = Some(span.end);
        Ok(())
    })
    .ok()?;
    if incomplete {
        return None;
    }
    scanner.finish(input)
}

/// Processing compares against normalized source bytes. Reuse the parsed
/// instructions and emitted offsets rather than reparsing all normalized
/// tokens. Escaped operator spelling can change tokenization after emission;
/// such recovery cases (and aborted inline-image normalization) retain the
/// authoritative standalone parse of the actual emitted representation.
fn normalized_collinear_plan(input: &[u8]) -> Option<LocalRewritePlan<'_>> {
    let mut scanner = CollinearScanner::default();
    let mut identical_operators = true;
    let normalized =
        crate::content_stream::normalize_content_stream(input, |instruction, emitted| {
            identical_operators &=
                input.get(instruction.operator_span()) == Some(&instruction.operator[..]);
            if identical_operators {
                scanner.instruction(instruction, emitted, true);
            }
        });
    let before = normalized.map_or(Cow::Borrowed(input), Cow::Owned);
    let (after, operations) = if matches!(before, Cow::Owned(_)) && identical_operators {
        scanner.finish(&before)?
    } else {
        compact_collinear_line_points(&before)?
    };
    let Cow::Owned(after) = after else {
        return None;
    };
    Some(LocalRewritePlan {
        before,
        after,
        operations,
    })
}

#[derive(Debug)]
struct FormCollinearCandidate {
    handle: ObjectHandle,
    encoded: Vec<u8>,
    vertices_removed: usize,
    decoded_bytes_removed: usize,
    estimated_flate_bytes_saved: usize,
}

const fn form_collinear_material(vertices_removed: usize, flate_bytes_saved: usize) -> bool {
    vertices_removed >= MIN_FORM_COLLINEAR_VERTICES_REMOVED
        || flate_bytes_saved >= MIN_FORM_COLLINEAR_FLATE_SAVINGS
}

fn compact_collinear_form_paths(
    document: &mut EditDocument,
    flate_level: crate::FlateLevel,
) -> Result<CollinearPathStats> {
    let mut candidates = Vec::new();
    let mut vertices_removed = 0usize;
    let mut flate_bytes_saved = 0usize;
    for handle in document.reachable_streams_with_subtype(b"Form")? {
        let Some(object) = document.current_value(handle)? else {
            continue;
        };
        let decoded = document.decoded_stream_value(&object)?;
        if decoded.len() < MIN_PAGE_CONTENT_BYTES {
            continue;
        }
        let Some((compacted, candidate_vertices_removed)) = compact_collinear_line_points(&decoded)
        else {
            continue;
        };
        if candidate_vertices_removed == 0 || compacted.as_ref() == decoded.as_slice() {
            continue;
        }
        let before_flate = crate::stream_codec::flate_encoded_len(&decoded, flate_level)?;
        let Some(encoded) =
            crate::stream_codec::encode_flate_capped(&compacted, before_flate, flate_level)?
        else {
            continue;
        };
        let encoded_len = encoded.len();
        let candidate_flate_bytes_saved = before_flate.saturating_sub(encoded_len);
        vertices_removed = vertices_removed.saturating_add(candidate_vertices_removed);
        flate_bytes_saved = flate_bytes_saved.saturating_add(candidate_flate_bytes_saved);
        candidates.push(FormCollinearCandidate {
            handle,
            encoded,
            vertices_removed: candidate_vertices_removed,
            decoded_bytes_removed: decoded.len().saturating_sub(compacted.len()),
            estimated_flate_bytes_saved: candidate_flate_bytes_saved,
        });
    }

    if !form_collinear_material(vertices_removed, flate_bytes_saved) {
        return Ok(CollinearPathStats::default());
    }

    let mut stats = CollinearPathStats::default();
    for candidate in candidates {
        let object = document.edit_handle(candidate.handle)?;
        let OwnedObject::Stream { dictionary, data } = object else {
            continue;
        };
        crate::stream_codec::set_plain_flate(dictionary);
        *data = StreamData::Owned(candidate.encoded.into());
        document.record_prepared_encoding(
            candidate.handle,
            crate::source::PreparedEncoding::Flate(flate_level),
        );
        stats.forms_rewritten = stats.forms_rewritten.saturating_add(1);
        stats.vertices_removed = stats
            .vertices_removed
            .saturating_add(candidate.vertices_removed);
        stats.decoded_bytes_removed = stats
            .decoded_bytes_removed
            .saturating_add(candidate.decoded_bytes_removed);
        stats.estimated_flate_bytes_saved = stats
            .estimated_flate_bytes_saved
            .saturating_add(candidate.estimated_flate_bytes_saved);
    }
    Ok(stats)
}

pub fn compact_collinear_paths(
    document: &mut EditDocument,
    pages: &[ObjectHandle],
    flate_level: crate::FlateLevel,
    reuse: &mut crate::flate::FlateReuseCache,
) -> Result<CollinearPathStats> {
    let page_stats = apply_local_page_rewrites(
        document,
        pages,
        flate_level,
        LocalRewriteKind::Collinear,
        reuse,
    )?;
    let form_stats = compact_collinear_form_paths(document, flate_level)?;
    Ok(CollinearPathStats {
        pages_rewritten: page_stats.pages_rewritten,
        forms_rewritten: form_stats.forms_rewritten,
        vertices_removed: page_stats
            .operations
            .saturating_add(form_stats.vertices_removed),
        decoded_bytes_removed: page_stats
            .decoded_bytes_removed
            .saturating_add(form_stats.decoded_bytes_removed),
        estimated_flate_bytes_saved: page_stats
            .estimated_flate_bytes_saved
            .saturating_add(form_stats.estimated_flate_bytes_saved),
    })
}

#[derive(Debug, Clone)]
struct GlyphTransaction {
    start: usize,
    end: usize,
    miter_bits: u64,
    bounds: GridBounds,
    ops: Vec<GlyphPathPoint>,
    closes: Vec<usize>,
}

fn structural_grid_coordinate(value: f64) -> Option<i64> {
    if !value.is_finite() {
        return None;
    }
    let scaled = value * STRUCTURAL_GRID_SCALE;
    if !scaled.is_finite() {
        return None;
    }
    let rounded = scaled.round();
    // Restrict to f64's exact-integer domain before the intentional grid conversion.
    if (scaled - rounded).abs() > 1.0e-6 || rounded.abs() > 9_007_199_254_740_991.0 {
        return None;
    }
    let coordinate = rounded.to_i64()?;
    Some(coordinate)
}

#[derive(Debug, Clone, Copy)]
struct StrokeGraphicsState {
    line_width: Option<f64>,
    miter_limit: Option<f64>,
}

impl Default for StrokeGraphicsState {
    fn default() -> Self {
        Self {
            line_width: Some(1.0),
            miter_limit: Some(10.0),
        }
    }
}

impl StrokeGraphicsState {
    fn bbox_margin_grid(self) -> Option<i64> {
        let width = self.line_width?;
        let miter = self.miter_limit?;
        if !width.is_finite() || width < 0.0 || !miter.is_finite() || miter <= 0.0 {
            return None;
        }
        let scaled = width * 0.5 * miter.max(1.0) * STRUCTURAL_GRID_SCALE;
        if !scaled.is_finite() || scaled < 0.0 || scaled > 9_007_199_254_740_990.0 {
            return None;
        }
        let rounded_up = scaled.ceil().to_i64()?;
        rounded_up.checked_add(1)
    }
}

#[derive(Debug, Clone)]
struct StrokeFormOccurrence {
    start: usize,
    end: usize,
    body_start: usize,
    body_end: usize,
    origin_x: i64,
    origin_y: i64,
    bounds: GridBounds,
    bbox_margin_grid: i64,
    signature: Vec<u8>,
}

fn stroke_signature_value(signature: &mut Vec<u8>, value: i64) {
    signature.extend_from_slice(&value.to_be_bytes());
}

fn stroke_form_path_event(
    event: &Event,
    numbers: &[f64],
    origin_x: i64,
    origin_y: i64,
    signature: &mut Vec<u8>,
    bounds: &mut Option<GridBounds>,
) -> Option<()> {
    let expected = match event.operator.as_slice() {
        b"m" | b"l" => 2,
        b"c" => 6,
        b"v" | b"y" | b"re" => 4,
        b"h" => 0,
        _ => return None,
    };
    let args = event.numbers(numbers);
    if !event.all_operands_numeric() || args.len() != expected {
        return None;
    }
    signature.push(u8::try_from(event.operator.len()).ok()?);
    signature.extend_from_slice(&event.operator);

    if event.operator.as_slice() == b"h" {
        return Some(());
    }

    let values = args
        .iter()
        .copied()
        .map(structural_grid_coordinate)
        .collect::<Option<SmallVec<[i64; 6]>>>()?;
    if event.operator.as_slice() == b"re" {
        let x = values[0];
        let y = values[1];
        let width = values[2];
        let height = values[3];
        stroke_signature_value(signature, x.checked_sub(origin_x)?);
        stroke_signature_value(signature, y.checked_sub(origin_y)?);
        stroke_signature_value(signature, width);
        stroke_signature_value(signature, height);
        let x1 = x.checked_add(width)?;
        let y1 = y.checked_add(height)?;
        for (px, py) in [(x, y), (x1, y1)] {
            let point = GridBounds::from_point(px, py);
            match bounds {
                Some(bounds) => bounds.include(point),
                None => *bounds = Some(point),
            }
        }
        return Some(());
    }

    for pair in values.as_slice().as_chunks::<2>().0 {
        let x = pair[0];
        let y = pair[1];
        stroke_signature_value(signature, x.checked_sub(origin_x)?);
        stroke_signature_value(signature, y.checked_sub(origin_y)?);
        let point = GridBounds::from_point(x, y);
        match bounds {
            Some(bounds) => bounds.include(point),
            None => *bounds = Some(point),
        }
    }
    Some(())
}

fn fenced_stroke_form_occurrence(
    input: &[u8],
    table: &EventTable,
    index: usize,
    caller_path_empty: bool,
    state: StrokeGraphicsState,
) -> Option<(StrokeFormOccurrence, usize)> {
    let events = &table.records;
    let numbers = &table.numbers;
    if !caller_path_empty {
        return None;
    }
    let save = events.get(index)?;
    let first = events.get(index + 1)?;
    let [x, y] = first.numbers(numbers) else {
        return None;
    };
    if save.operator.as_slice() != b"q"
        || save.operand_count != 0
        || first.operator.as_slice() != b"m"
        || !adjacent(input, save, first)
    {
        return None;
    }
    let origin_x = structural_grid_coordinate(*x)?;
    let origin_y = structural_grid_coordinate(*y)?;
    let mut signature = b"PdfRedoxStrokeForm1".to_vec();
    let mut bounds = None::<GridBounds>;
    let mut cursor = index + 1;
    let mut previous = save;

    loop {
        let event = events.get(cursor)?;
        if !adjacent(input, previous, event) {
            return None;
        }
        if matches!(
            event.operator.as_slice(),
            b"m" | b"l" | b"c" | b"v" | b"y" | b"h" | b"re"
        ) {
            stroke_form_path_event(
                event,
                numbers,
                origin_x,
                origin_y,
                &mut signature,
                &mut bounds,
            )?;
            previous = event;
            cursor += 1;
            continue;
        }
        break;
    }

    let stroke = events.get(cursor)?;
    let restore = events.get(cursor + 1)?;
    if stroke.operator.as_slice() != b"S"
        || stroke.operand_count != 0
        || restore.operator.as_slice() != b"Q"
        || restore.operand_count != 0
        || !adjacent(input, previous, stroke)
        || !adjacent(input, stroke, restore)
    {
        return None;
    }
    signature.push(1);
    signature.extend_from_slice(b"S");

    Some((
        StrokeFormOccurrence {
            start: save.start,
            end: restore.end,
            body_start: first.start,
            body_end: stroke.end,
            origin_x,
            origin_y,
            bounds: bounds?,
            bbox_margin_grid: state.bbox_margin_grid()?,
            signature,
        },
        cursor + 2,
    ))
}

fn stroke_form_occurrences(input: &[u8]) -> Option<Vec<StrokeFormOccurrence>> {
    let events = events_for(input, &[b"gs", b"d"])?;
    Some(project_stroke_form_occurrences(input, &events))
}

fn project_stroke_form_occurrences(input: &[u8], events: &EventTable) -> Vec<StrokeFormOccurrence> {
    let numbers = &events.numbers;
    let mut occurrences = Vec::new();
    let mut state = StrokeGraphicsState::default();
    let mut state_stack = Vec::<StrokeGraphicsState>::new();
    let mut path_nonempty = false;
    let mut index = 0usize;

    while index < events.records.len() {
        let event = &events.records[index];
        if event.operator.as_slice() == b"q"
            && let Some((occurrence, next)) =
                fenced_stroke_form_occurrence(input, events, index, !path_nonempty, state)
        {
            occurrences.push(occurrence);
            path_nonempty = false;
            index = next;
            continue;
        }

        match event.operator.as_slice() {
            b"q" => state_stack.push(state),
            b"Q" => {
                state = state_stack.pop().unwrap_or(StrokeGraphicsState {
                    line_width: None,
                    miter_limit: None,
                });
            }
            b"w" if event.operand_count == 1 => {
                state.line_width = event
                    .sole_number(numbers)
                    .filter(|value| value.is_finite() && *value >= 0.0);
            }
            b"M" if event.operand_count == 1 => {
                state.miter_limit = event
                    .sole_number(numbers)
                    .filter(|value| value.is_finite() && *value > 0.0);
            }
            b"m" | b"l" | b"c" | b"v" | b"y" | b"h" | b"re" => {
                path_nonempty = true;
            }
            b"S" | b"s" | b"f" | b"F" | b"f*" | b"B" | b"B*" | b"b" | b"b*" | b"n" => {
                path_nonempty = false;
            }
            _ => {}
        }
        index += 1;
    }
    occurrences
}

fn outlined_glyph_transaction(
    input: &[u8],
    table: &EventTable,
    index: usize,
) -> Option<(GlyphTransaction, usize)> {
    let events = &table.records;
    let numbers = &table.numbers;
    let (save, _, miter_bits) = saved_miter_prefix(input, table, index)?;

    let mut cursor = index + 2;
    let mut ops = Vec::new();
    let mut closes = Vec::new();
    let mut bounds = None::<GridBounds>;
    let mut saw_path = false;
    loop {
        let event = events.get(cursor)?;
        if cursor > index + 2 && !adjacent(input, &events[cursor - 1], event) {
            return None;
        }
        match (event.operator.as_slice(), event.numbers(numbers)) {
            (b"m" | b"l", [x, y]) => {
                let x = structural_grid_coordinate(*x)?;
                let y = structural_grid_coordinate(*y)?;
                let point = GridBounds::from_point(x, y);
                match &mut bounds {
                    Some(bounds) => bounds.include(point),
                    None => bounds = Some(point),
                }
                ops.push(GlyphPathPoint {
                    operator: event.operator[0],
                    x,
                    y,
                });
                saw_path = true;
                cursor += 1;
            }
            (b"h", _) if event.operand_count == 0 => {
                closes.push(ops.len());
                cursor += 1;
            }
            _ => break,
        }
    }
    if !saw_path {
        return None;
    }
    let paint = events.get(cursor)?;
    let restore = events.get(cursor + 1)?;
    if paint.operator.as_slice() != b"b"
        || paint.operand_count != 0
        || restore.operator.as_slice() != b"Q"
        || restore.operand_count != 0
        || !adjacent(input, &events[cursor - 1], paint)
        || !adjacent(input, paint, restore)
    {
        return None;
    }
    Some((
        GlyphTransaction {
            start: save.start,
            end: restore.end,
            miter_bits,
            bounds: bounds?,
            ops,
            closes,
        },
        cursor + 2,
    ))
}

#[derive(Debug)]
struct GlyphUnionFind {
    parents: Vec<usize>,
    sizes: Vec<usize>,
}

impl GlyphUnionFind {
    fn new(count: usize) -> Self {
        Self {
            parents: (0..count).collect(),
            sizes: vec![1; count],
        }
    }
    fn find(&mut self, mut index: usize) -> usize {
        while self.parents[index] != index {
            self.parents[index] = self.parents[self.parents[index]];
            index = self.parents[index];
        }
        index
    }
    fn union(&mut self, left: usize, right: usize) {
        let mut left = self.find(left);
        let mut right = self.find(right);
        if left == right {
            return;
        }
        if self.sizes[left] < self.sizes[right] {
            std::mem::swap(&mut left, &mut right);
        }
        self.parents[right] = left;
        self.sizes[left] = self.sizes[left].saturating_add(self.sizes[right]);
    }
}

#[derive(Debug, Clone)]
struct GlyphComponent {
    start: usize,
    end: usize,
    origin_x: i64,
    origin_y: i64,
    width: i64,
    height: i64,
    source_paints: usize,
    signature: Vec<u8>,
}

fn outlined_glyph_component_signature(
    run: &[GlyphTransaction],
    indices: &[usize],
    bounds: GridBounds,
) -> Vec<u8> {
    let mut signature = Vec::new();
    signature.extend_from_slice(b"PdfRedoxOutlinedGlyph1");
    for &index in indices {
        let transaction = &run[index];
        signature.push(0xff);
        signature.extend_from_slice(&transaction.miter_bits.to_be_bytes());
        let mut close_cursor = 0usize;
        for (op_index, op) in transaction.ops.iter().enumerate() {
            while transaction
                .closes
                .get(close_cursor)
                .is_some_and(|&position| position == op_index)
            {
                signature.push(b'h');
                close_cursor += 1;
            }
            signature.push(op.operator);
            signature.extend_from_slice(&(op.x - bounds.x0).to_be_bytes());
            signature.extend_from_slice(&(op.y - bounds.y0).to_be_bytes());
        }
        while close_cursor < transaction.closes.len() {
            signature.push(b'h');
            close_cursor += 1;
        }
    }
    signature
}

fn outlined_glyph_components_for_run(run: &[GlyphTransaction]) -> Vec<GlyphComponent> {
    if run.is_empty() {
        return Vec::new();
    }

    let cell_size = MAX_OUTLINED_GLYPH_EXTENT_GRID.max(1);
    let mut cells = BTreeMap::<(i64, i64), Vec<usize>>::new();
    let mut union = GlyphUnionFind::new(run.len());
    for index in 0..run.len() {
        let bounds = run[index].bounds;
        if bounds.width() > MAX_OUTLINED_GLYPH_EXTENT_GRID
            || bounds.height() > MAX_OUTLINED_GLYPH_EXTENT_GRID
        {
            continue;
        }
        let x0 = bounds.x0.div_euclid(cell_size);
        let x1 = bounds.x1.div_euclid(cell_size);
        let y0 = bounds.y0.div_euclid(cell_size);
        let y1 = bounds.y1.div_euclid(cell_size);
        let mut candidates = BTreeSet::new();
        for x in x0..=x1 {
            for y in y0..=y1 {
                if let Some(indices) = cells.get(&(x, y)) {
                    candidates.extend(indices.iter().copied());
                }
            }
        }
        for candidate in candidates {
            if run[index].bounds.touches(run[candidate].bounds) {
                union.union(index, candidate);
            }
        }
        for x in x0..=x1 {
            for y in y0..=y1 {
                cells.entry((x, y)).or_default().push(index);
            }
        }
    }

    let mut groups = BTreeMap::<usize, Vec<usize>>::new();
    for index in 0..run.len() {
        let root = union.find(index);
        groups.entry(root).or_default().push(index);
    }

    let mut components = Vec::new();
    for indices in groups.into_values() {
        let Some(&first) = indices.first() else {
            continue;
        };
        let Some(&last) = indices.last() else {
            continue;
        };
        if last.saturating_sub(first).saturating_add(1) != indices.len() {
            continue;
        }
        let mut bounds = run[first].bounds;
        for &index in &indices[1..] {
            bounds.include(run[index].bounds);
        }
        if bounds.width() > MAX_OUTLINED_GLYPH_EXTENT_GRID
            || bounds.height() > MAX_OUTLINED_GLYPH_EXTENT_GRID
        {
            continue;
        }
        components.push(GlyphComponent {
            start: run[first].start,
            end: run[last].end,
            origin_x: bounds.x0,
            origin_y: bounds.y0,
            width: bounds.width(),
            height: bounds.height(),
            source_paints: indices.len(),
            signature: outlined_glyph_component_signature(run, &indices, bounds),
        });
    }
    components
}

fn outlined_glyph_components(input: &[u8]) -> Option<Vec<GlyphComponent>> {
    let events = events_for(input, &[b"BT", b"ET"])?;
    let mut components = Vec::new();
    let mut index = 0usize;
    while index < events.records.len() {
        let Some((first, mut next_index)) = outlined_glyph_transaction(input, &events, index)
        else {
            index += 1;
            continue;
        };
        let mut run = vec![first];
        while let Some((next, after_next)) = outlined_glyph_transaction(input, &events, next_index)
        {
            let previous = run.last()?;
            if !input.get(previous.end..next.start).is_some_and(trivia_only) {
                break;
            }
            run.push(next);
            next_index = after_next;
        }
        let run_components = outlined_glyph_components_for_run(&run);
        components.extend(run_components);
        index = next_index;
    }
    Some(components)
}

#[derive(Debug, Clone)]
struct GlyphShapePlan {
    canonical: GlyphComponent,
    occurrences: Vec<GlyphComponent>,
    charproc: Vec<u8>,
}

#[derive(Debug, Clone)]
struct GlyphFontPlan {
    name: Vec<u8>,
    shapes: Vec<GlyphShapePlan>,
}

fn structural_pdf_number(value: i64) -> String {
    let negative = value < 0;
    let magnitude = i128::from(value).abs();
    let whole = magnitude / 100;
    let fraction = magnitude % 100;
    let body = if fraction == 0 {
        whole.to_string()
    } else if fraction % 10 == 0 {
        format!("{whole}.{}", fraction / 10)
    } else {
        format!("{whole}.{fraction:02}")
    };
    if negative && magnitude != 0 {
        format!("-{body}")
    } else {
        body
    }
}

fn outlined_glyph_charproc(input: &[u8], component: &GlyphComponent) -> Option<Vec<u8>> {
    let x = component.origin_x.checked_neg()?;
    let y = component.origin_y.checked_neg()?;
    let mut output = Vec::new();
    output.extend_from_slice(b"0 0 d0\n1 0 0 1 ");
    output.extend_from_slice(structural_pdf_number(x).as_bytes());
    output.push(b' ');
    output.extend_from_slice(structural_pdf_number(y).as_bytes());
    output.extend_from_slice(b" cm\n");
    output.extend_from_slice(input.get(component.start..component.end)?);
    output.push(b'\n');
    Some(output)
}

fn outlined_glyph_placement(
    font_name: &[u8],
    code: usize,
    component: &GlyphComponent,
) -> Option<Vec<u8>> {
    let code = u8::try_from(code).ok()?;
    if code == 0 {
        return None;
    }
    let mut output = Vec::new();
    output.extend_from_slice(b"BT /");
    output.extend_from_slice(font_name);
    output.extend_from_slice(b" 1 Tf 1 0 0 1 ");
    output.extend_from_slice(structural_pdf_number(component.origin_x).as_bytes());
    output.push(b' ');
    output.extend_from_slice(structural_pdf_number(component.origin_y).as_bytes());
    output.extend_from_slice(b" Tm <");
    output.extend_from_slice(format!("{code:02X}").as_bytes());
    output.extend_from_slice(b"> Tj ET\n");
    Some(output)
}

fn outlined_glyph_font_name(index: usize, occupied: &mut BTreeSet<Vec<u8>>) -> Vec<u8> {
    let mut serial = index;
    loop {
        let name = format!("PdfRedoxGlyph{serial}").into_bytes();
        if occupied.insert(name.clone()) {
            return name;
        }
        serial = serial.saturating_add(1);
    }
}

fn stroke_form_name(index: usize, occupied: &mut BTreeSet<Vec<u8>>) -> Vec<u8> {
    let mut serial = index;
    loop {
        let name = format!("PdfRedoxStroke{serial}").into_bytes();
        if occupied.insert(name.clone()) {
            return name;
        }
        serial = serial.saturating_add(1);
    }
}

fn structural_grid_real(value: i64) -> Option<f64> {
    crate::source::exact_i64_to_f64(value).map(|value| value / STRUCTURAL_GRID_SCALE)
}

fn stroke_form_dictionary(bounds: GridBounds, margin_grid: i64) -> Option<OwnedDictionary> {
    let x0 = bounds.x0.checked_sub(margin_grid)?;
    let y0 = bounds.y0.checked_sub(margin_grid)?;
    let x1 = bounds.x1.checked_add(margin_grid)?;
    let y1 = bounds.y1.checked_add(margin_grid)?;
    let mut dictionary = OwnedDictionary::new();
    dictionary.insert(b"Type".to_vec(), OwnedObject::Name(b"XObject".to_vec()));
    dictionary.insert(b"Subtype".to_vec(), OwnedObject::Name(b"Form".to_vec()));
    dictionary.insert(b"FormType".to_vec(), OwnedObject::Integer(1));
    dictionary.insert(
        b"BBox".to_vec(),
        OwnedObject::Array(vec![
            OwnedObject::Real(structural_grid_real(x0)?),
            OwnedObject::Real(structural_grid_real(y0)?),
            OwnedObject::Real(structural_grid_real(x1)?),
            OwnedObject::Real(structural_grid_real(y1)?),
        ]),
    );
    dictionary.insert(
        b"Resources".to_vec(),
        OwnedObject::Dictionary(OwnedDictionary::new()),
    );
    Some(dictionary)
}

fn stroke_form_replacement(
    name: &[u8],
    canonical: &StrokeFormOccurrence,
    occurrence: &StrokeFormOccurrence,
) -> Option<Vec<u8>> {
    let dx = occurrence.origin_x.checked_sub(canonical.origin_x)?;
    let dy = occurrence.origin_y.checked_sub(canonical.origin_y)?;
    let mut output = Vec::new();
    output.extend_from_slice(b"q\n1 0 0 1 ");
    output.extend_from_slice(structural_pdf_number(dx).as_bytes());
    output.push(b' ');
    output.extend_from_slice(structural_pdf_number(dy).as_bytes());
    output.extend_from_slice(b" cm\n/");
    output.extend_from_slice(name);
    output.extend_from_slice(b" Do\nQ\n");
    Some(output)
}

/// A scan is reusable only for the exact immutable content allocation it
/// inspected. A weak reference keeps allocation identity alive without
/// retaining the decoded byte buffer or preventing ordinary cache eviction.
struct StrokeFormPageScan {
    snapshot: Weak<Vec<u8>>,
    occurrences: Option<Vec<StrokeFormOccurrence>>,
}

#[derive(Default)]
pub struct StrokeFormScanEvidence {
    pages: BTreeMap<ObjectHandle, StrokeFormPageScan>,
}

impl StrokeFormScanEvidence {
    fn remember(
        &mut self,
        page: ObjectHandle,
        decoded: &Arc<Vec<u8>>,
        events: Option<&EventTable>,
    ) {
        // A concatenated/uncached page with no other owner cannot keep this
        // allocation alive until the next stage. Avoid speculative discovery.
        if Arc::strong_count(decoded) <= 1 {
            return;
        }
        let occurrences = events.map(|events| project_stroke_form_occurrences(decoded, events));
        self.pages.insert(
            page,
            StrokeFormPageScan {
                snapshot: Arc::downgrade(decoded),
                occurrences,
            },
        );
    }

    fn take_for(
        &mut self,
        page: ObjectHandle,
        decoded: &Arc<Vec<u8>>,
    ) -> Option<StrokeFormPageScan> {
        self.pages
            .remove(&page)
            .filter(|scan| scan.snapshot.ptr_eq(&Arc::downgrade(decoded)))
    }
}

#[derive(Debug, Clone)]
struct StrokeFormPageOccurrence {
    page_index: usize,
    occurrence: StrokeFormOccurrence,
}

#[derive(Debug, Clone)]
struct StrokeFormPlan {
    name: Vec<u8>,
    body: Vec<u8>,
    bounds: GridBounds,
    bbox_margin_grid: i64,
    canonical: StrokeFormOccurrence,
    occurrences: Vec<StrokeFormPageOccurrence>,
}

#[expect(
    clippy::too_many_lines,
    reason = "keeps the all-or-nothing form factoring plan and mutation sequence together"
)]
pub fn factor_repeated_stroke_forms(
    document: &mut EditDocument,
    pages: &[ObjectHandle],
    flate_level: crate::FlateLevel,
    evidence: &mut StrokeFormScanEvidence,
    reuse: &mut crate::flate::FlateReuseCache,
) -> Result<StrokeFormFactorStats> {
    let mut page_data = Vec::<ObjectHandle>::new();
    let mut groups = BTreeMap::<Vec<u8>, Vec<StrokeFormPageOccurrence>>::new();
    let mut occupied_names = BTreeSet::<Vec<u8>>::new();

    for &page in pages {
        let Some(decoded) = page_content_if_present(document, page)? else {
            continue;
        };
        if decoded.len() < MIN_PAGE_CONTENT_BYTES {
            continue;
        }
        let occurrences = evidence.take_for(page, &decoded).map_or_else(
            || stroke_form_occurrences(&decoded),
            |scan| scan.occurrences,
        );
        let Some(occurrences) = occurrences else {
            continue;
        };
        let page_index = page_data.len();
        for occurrence in occurrences {
            groups
                .entry(occurrence.signature.clone())
                .or_default()
                .push(StrokeFormPageOccurrence {
                    page_index,
                    occurrence,
                });
        }
        page_data.push(page);
    }

    // Keep only signatures and spans during discovery. Resolve resource names
    // across all complete eligible pages exactly as before, but only if a
    // repeated group can actually produce a Form.
    groups.retain(|_, occurrences| {
        occurrences.len() >= 2
            && occurrences[0]
                .occurrence
                .body_end
                .saturating_sub(occurrences[0].occurrence.body_start)
                >= MIN_STROKE_FORM_BODY_BYTES
    });
    if groups.is_empty() {
        return Ok(StrokeFormFactorStats::default());
    }

    for &page in &page_data {
        let resources = effective_page_resources(document, page)?;
        if let Some(xobjects) = resolved_dictionary(document, resources.get(b"XObject".as_slice()))?
        {
            occupied_names.extend(xobjects.keys().cloned());
        }
    }

    let candidate_pages = groups
        .values()
        .flatten()
        .map(|item| item.page_index)
        .collect::<BTreeSet<_>>();
    let mut payloads = BTreeMap::new();
    for index in candidate_pages {
        let Some(decoded) = page_content_if_present(document, page_data[index])? else {
            return Ok(StrokeFormFactorStats::default());
        };
        payloads.insert(index, decoded);
    }

    let mut plans = Vec::<StrokeFormPlan>::new();
    let mut form_index = 0usize;
    for occurrences in groups.into_values() {
        let canonical_page = occurrences[0].page_index;
        let canonical = occurrences[0].occurrence.clone();
        let Some(body) = payloads
            .get(&canonical_page)
            .and_then(|decoded| decoded.get(canonical.body_start..canonical.body_end))
            .map(ToOwned::to_owned)
        else {
            continue;
        };
        let max_margin_grid = occurrences
            .iter()
            .map(|item| item.occurrence.bbox_margin_grid)
            .max()
            .unwrap_or(canonical.bbox_margin_grid);
        let name = stroke_form_name(form_index, &mut occupied_names);
        form_index = form_index.saturating_add(1);
        plans.push(StrokeFormPlan {
            name,
            body,
            bounds: canonical.bounds,
            bbox_margin_grid: max_margin_grid,
            canonical,
            occurrences,
        });
    }
    if plans.is_empty() {
        return Ok(StrokeFormFactorStats::default());
    }

    let mut replacements_by_page = vec![Vec::<(usize, usize, Vec<u8>)>::new(); page_data.len()];
    let mut occurrence_count = 0usize;
    let mut decoded_factored = 0usize;
    for plan in &plans {
        for item in &plan.occurrences {
            let Some(replacement) =
                stroke_form_replacement(&plan.name, &plan.canonical, &item.occurrence)
            else {
                return Ok(StrokeFormFactorStats::default());
            };
            replacements_by_page[item.page_index].push((
                item.occurrence.start,
                item.occurrence.end,
                replacement,
            ));
            occurrence_count = occurrence_count.saturating_add(1);
            decoded_factored = decoded_factored
                .saturating_add(item.occurrence.end.saturating_sub(item.occurrence.start));
        }
    }

    let pages = replacements_by_page
        .iter()
        .enumerate()
        .filter(|(_, replacements)| !replacements.is_empty())
        .map(|(index, replacements)| (index, payloads[&index].as_slice(), replacements.as_slice()));
    let overhead = plans
        .len()
        .saturating_mul(STROKE_FORM_OVERHEAD_BYTES)
        .saturating_add(occurrence_count.saturating_mul(STROKE_FORM_OCCURRENCE_OVERHEAD_BYTES));
    let Some(measured) = crate::content_stream::measure_form_rewrites(
        pages,
        plans.iter().map(|plan| plan.body.as_slice()),
        overhead,
        MIN_STROKE_FORM_ESTIMATED_SAVINGS,
        flate_level,
        |bytes, cap| crate::stream_codec::flate_capped_encoded_len(bytes, cap, flate_level),
    )?
    else {
        return Ok(StrokeFormFactorStats::default());
    };
    let pages_rewritten = measured.pages.len();

    let mut bindings = Vec::new();
    for (plan, encoded) in plans.iter_mut().zip(measured.bodies) {
        let Some(dictionary) = stroke_form_dictionary(plan.bounds, plan.bbox_margin_grid) else {
            return Ok(StrokeFormFactorStats::default());
        };
        let digest = sha2::Sha256::digest(&plan.body).into();
        let form = ObjectHandle::New(document.add_object(OwnedObject::Stream {
            dictionary,
            data: StreamData::Owned(std::mem::take(&mut plan.body).into()),
        }));
        reuse.insert_with_digest(form, flate_level, digest, encoded);
        bindings.extend(
            plan.occurrences
                .iter()
                .map(|item| item.page_index)
                .collect::<BTreeSet<_>>()
                .into_iter()
                .map(|page_index| (page_data[page_index], plan.name.as_slice(), form)),
        );
    }
    install_page_resource_bindings(document, b"XObject", bindings)?;
    for (page_index, rewritten, encoded) in measured.pages {
        reuse.install_page(
            document,
            page_data[page_index],
            rewritten,
            encoded,
            flate_level,
        )?;
    }

    Ok(StrokeFormFactorStats {
        pages_rewritten,
        forms_created: plans.len(),
        occurrences_replaced: occurrence_count,
        decoded_bytes_factored: decoded_factored,
        estimated_flate_bytes_saved: measured.savings,
    })
}

fn outlined_glyph_shape_weight(input: &[u8], occurrences: &[GlyphComponent]) -> usize {
    occurrences
        .iter()
        .map(|component| component.end.saturating_sub(component.start))
        .sum::<usize>()
        .saturating_sub(
            occurrences
                .first()
                .map_or(0, |component| component.end.saturating_sub(component.start)),
        )
        .min(input.len())
}

fn outlined_glyph_plans(
    document: &EditDocument,
    page: ObjectHandle,
    input: &[u8],
) -> Result<Vec<GlyphFontPlan>> {
    let Some(components) = outlined_glyph_components(input) else {
        return Ok(Vec::new());
    };
    let mut grouped = BTreeMap::<Vec<u8>, Vec<GlyphComponent>>::new();
    for component in components {
        grouped
            .entry(component.signature.clone())
            .or_default()
            .push(component);
    }
    let mut groups = grouped
        .into_values()
        .filter(|occurrences| occurrences.len() >= MIN_OUTLINED_GLYPH_OCCURRENCES)
        .collect::<Vec<_>>();
    groups.sort_unstable_by_key(|occurrences| {
        std::cmp::Reverse(outlined_glyph_shape_weight(input, occurrences))
    });
    if groups.is_empty() {
        return Ok(Vec::new());
    }

    let resources = effective_page_resources(document, page)?;
    let mut occupied = resolved_dictionary(document, resources.get(b"Font".as_slice()))?
        .map_or_default(|fonts| fonts.keys().cloned().collect::<BTreeSet<_>>());

    let mut plans = Vec::new();
    for (font_index, chunk) in groups.chunks(MAX_TYPE3_GLYPHS_PER_FONT).enumerate() {
        let mut shapes = Vec::with_capacity(chunk.len());
        for occurrences in chunk {
            let Some(canonical) = occurrences.first().cloned() else {
                continue;
            };
            let Some(charproc) = outlined_glyph_charproc(input, &canonical) else {
                continue;
            };
            shapes.push(GlyphShapePlan {
                canonical,
                occurrences: occurrences.clone(),
                charproc,
            });
        }
        if !shapes.is_empty() {
            plans.push(GlyphFontPlan {
                name: outlined_glyph_font_name(font_index, &mut occupied),
                shapes,
            });
        }
    }
    Ok(plans)
}

#[derive(Debug)]
struct GlyphReplacementPlan {
    replacements: Vec<(usize, usize, Vec<u8>)>,
    occurrences_replaced: usize,
    source_paints_replaced: usize,
}

fn outlined_glyph_replacements(plans: &[GlyphFontPlan]) -> Option<GlyphReplacementPlan> {
    let mut replacements = Vec::new();
    let mut occurrences_replaced = 0usize;
    let mut source_paints_replaced = 0usize;
    for plan in plans {
        for (shape_index, shape) in plan.shapes.iter().enumerate() {
            let code = shape_index.saturating_add(1);
            for occurrence in &shape.occurrences {
                replacements.push((
                    occurrence.start,
                    occurrence.end,
                    outlined_glyph_placement(&plan.name, code, occurrence)?,
                ));
                occurrences_replaced = occurrences_replaced.saturating_add(1);
                source_paints_replaced =
                    source_paints_replaced.saturating_add(occurrence.source_paints);
            }
        }
    }
    Some(GlyphReplacementPlan {
        replacements,
        occurrences_replaced,
        source_paints_replaced,
    })
}

fn prepare_glyph_encodings(
    plans: &[GlyphFontPlan],
    flate_level: crate::FlateLevel,
) -> Result<(usize, Vec<Vec<Vec<u8>>>)> {
    let mut total = plans.len().saturating_mul(512);
    let encodings = plans
        .iter()
        .map(|plan| {
            plan.shapes
                .iter()
                .map(|shape| {
                    let bytes = crate::stream_codec::encode_flate(&shape.charproc, flate_level)?;
                    total = total.saturating_add(bytes.len()).saturating_add(128);
                    Ok(bytes)
                })
                .collect::<Result<Vec<_>>>()
        })
        .collect::<Result<Vec<_>>>()?;
    Ok((total, encodings))
}

fn outlined_glyph_font_dictionary(
    document: &mut EditDocument,
    plan: &GlyphFontPlan,
    encoded: Vec<Vec<u8>>,
    level: crate::FlateLevel,
    reuse: &mut crate::flate::FlateReuseCache,
) -> ObjectHandle {
    let mut charprocs = OwnedDictionary::new();
    let mut differences = vec![OwnedObject::Integer(1)];
    let mut widths = Vec::with_capacity(plan.shapes.len());
    let mut max_width = 0i64;
    let mut max_height = 0i64;

    for (index, (shape, encoded)) in plan.shapes.iter().zip(encoded).enumerate() {
        let code = index.saturating_add(1);
        let glyph_name = format!("G{code}").into_bytes();
        let stream = ObjectHandle::New(document.add_object(OwnedObject::Stream {
            dictionary: OwnedDictionary::new(),
            data: StreamData::Owned(shape.charproc.clone().into()),
        }));
        reuse.insert(stream, level, &shape.charproc, encoded);
        charprocs.insert(glyph_name.clone(), OwnedObject::Reference(stream));
        differences.push(OwnedObject::Name(glyph_name));
        widths.push(OwnedObject::Integer(0));
        max_width = max_width.max(shape.canonical.width);
        max_height = max_height.max(shape.canonical.height);
    }

    let mut encoding = OwnedDictionary::new();
    encoding.insert(b"Type".to_vec(), OwnedObject::Name(b"Encoding".to_vec()));
    encoding.insert(b"Differences".to_vec(), OwnedObject::Array(differences));

    let mut dictionary = OwnedDictionary::new();
    dictionary.insert(b"Type".to_vec(), OwnedObject::Name(b"Font".to_vec()));
    dictionary.insert(b"Subtype".to_vec(), OwnedObject::Name(b"Type3".to_vec()));
    dictionary.insert(
        b"FontBBox".to_vec(),
        OwnedObject::Array(vec![
            OwnedObject::Real(-1.0),
            OwnedObject::Real(-1.0),
            OwnedObject::Real(
                f64::from(i32::try_from(max_width).unwrap_or(i32::MAX)) / STRUCTURAL_GRID_SCALE
                    + 1.0,
            ),
            OwnedObject::Real(
                f64::from(i32::try_from(max_height).unwrap_or(i32::MAX)) / STRUCTURAL_GRID_SCALE
                    + 1.0,
            ),
        ]),
    );
    dictionary.insert(
        b"FontMatrix".to_vec(),
        OwnedObject::Array(vec![
            OwnedObject::Integer(1),
            OwnedObject::Integer(0),
            OwnedObject::Integer(0),
            OwnedObject::Integer(1),
            OwnedObject::Integer(0),
            OwnedObject::Integer(0),
        ]),
    );
    dictionary.insert(b"CharProcs".to_vec(), OwnedObject::Dictionary(charprocs));
    dictionary.insert(b"Encoding".to_vec(), OwnedObject::Dictionary(encoding));
    dictionary.insert(b"FirstChar".to_vec(), OwnedObject::Integer(1));
    dictionary.insert(
        b"LastChar".to_vec(),
        OwnedObject::Integer(i64::try_from(plan.shapes.len()).unwrap_or(i64::MAX)),
    );
    dictionary.insert(b"Widths".to_vec(), OwnedObject::Array(widths));
    dictionary.insert(
        b"Resources".to_vec(),
        OwnedObject::Dictionary(OwnedDictionary::new()),
    );
    ObjectHandle::New(document.add_object(OwnedObject::Dictionary(dictionary)))
}

fn factor_polyline_outlined_glyphs(
    document: &mut EditDocument,
    pages: &[ObjectHandle],
    flate_level: crate::FlateLevel,
    reuse: &mut crate::flate::FlateReuseCache,
) -> Result<OutlinedGlyphFactorStats> {
    let mut stats = OutlinedGlyphFactorStats::default();
    for &page in pages {
        let Some(decoded) = page_content_if_present(document, page)? else {
            continue;
        };
        if decoded.len() < MIN_PAGE_CONTENT_BYTES {
            continue;
        }

        let plans = outlined_glyph_plans(document, page, &decoded)?;
        if plans.is_empty() {
            continue;
        }
        let Some(replacement_plan) = outlined_glyph_replacements(&plans) else {
            continue;
        };
        let Some(rewritten) =
            crate::content_stream::apply_replacements(&decoded, &replacement_plan.replacements)
        else {
            continue;
        };
        let before_flate = reuse.encoded_len(&decoded, flate_level)?;
        let page_encoding = crate::stream_codec::encode_flate(&rewritten, flate_level)?;
        let (font_cost, glyph_encodings) = prepare_glyph_encodings(&plans, flate_level)?;
        let after_flate = page_encoding.len().saturating_add(font_cost);
        if before_flate <= after_flate
            || before_flate.saturating_sub(after_flate) < MIN_OUTLINED_GLYPH_ESTIMATED_SAVINGS
        {
            continue;
        }

        let bindings = plans
            .iter()
            .zip(glyph_encodings)
            .map(|(plan, encoded)| {
                let font =
                    outlined_glyph_font_dictionary(document, plan, encoded, flate_level, reuse);
                (page, plan.name.as_slice(), font)
            })
            .collect::<Vec<_>>();
        install_page_resource_bindings(document, b"Font", bindings)?;
        let rewritten_len = rewritten.len();
        reuse.install_page(document, page, rewritten, page_encoding, flate_level)?;

        let glyphs_created = plans.iter().map(|plan| plan.shapes.len()).sum::<usize>();
        stats.pages_rewritten = stats.pages_rewritten.saturating_add(1);
        stats.fonts_created = stats.fonts_created.saturating_add(plans.len());
        stats.glyphs_created = stats.glyphs_created.saturating_add(glyphs_created);
        stats.occurrences_replaced = stats
            .occurrences_replaced
            .saturating_add(replacement_plan.occurrences_replaced);
        stats.source_paints_replaced = stats
            .source_paints_replaced
            .saturating_add(replacement_plan.source_paints_replaced);
        stats.decoded_bytes_removed = stats
            .decoded_bytes_removed
            .saturating_add(decoded.len().saturating_sub(rewritten_len));
        stats.estimated_flate_bytes_saved = stats
            .estimated_flate_bytes_saved
            .saturating_add(before_flate.saturating_sub(after_flate));
    }
    Ok(stats)
}

pub fn factor_outlined_glyphs(
    doc: &mut EditDocument,
    pages: &[ObjectHandle],
    flate_level: crate::FlateLevel,
    catalog: Option<&mut crate::GlyphCatalog>,
    reuse: &mut crate::flate::FlateReuseCache,
) -> Result<OutlinedGlyphFactorStats> {
    let mut stats = factor_polyline_outlined_glyphs(doc, pages, flate_level, reuse)?;
    let curves = curved_glyphs::factor(doc, pages, flate_level, catalog, reuse)?;
    stats.pages_rewritten += curves.pages_rewritten;
    stats.fonts_created += curves.fonts_created;
    stats.glyphs_created += curves.glyphs_created;
    stats.occurrences_replaced += curves.occurrences_replaced;
    stats.source_paints_replaced += curves.source_paints_replaced;
    stats.decoded_bytes_removed += curves.decoded_bytes_removed;
    stats.estimated_flate_bytes_saved += curves.estimated_flate_bytes_saved;
    Ok(stats)
}

pub fn batch_page_paints(
    document: &mut EditDocument,
    pages: &[ObjectHandle],
    evidence: &mut StrokeFormScanEvidence,
) -> Result<PaintBatchStats> {
    let mut stats = PaintBatchStats::default();
    for &page in pages {
        evidence.pages.remove(&page);
        let Some(decoded) = page_content_if_present(document, page)? else {
            continue;
        };
        if decoded.len() < MIN_PAGE_CONTENT_BYTES {
            continue;
        }
        let events = events_for(&decoded, &[b"gs", b"d"]);
        let candidate = events
            .as_ref()
            .and_then(|events| batch_content(&decoded, events));
        let Some((batched, page_stats)) = candidate.filter(|(batched, stats)| {
            stats.paints_eliminated >= MIN_PAINTS_ELIMINATED
                && batched.as_ref() != decoded.as_slice()
        }) else {
            evidence.remember(page, &decoded, events.as_ref());
            continue;
        };
        replace_page_content(document, page, batched.into_owned())?;
        stats.pages_rewritten = stats.pages_rewritten.saturating_add(1);
        stats.groups_created = stats
            .groups_created
            .saturating_add(page_stats.groups_created);
        stats.source_paints_batched = stats
            .source_paints_batched
            .saturating_add(page_stats.source_paints_batched);
        stats.paints_eliminated = stats
            .paints_eliminated
            .saturating_add(page_stats.paints_eliminated);
    }
    Ok(stats)
}

#[cfg(test)]
mod tests {
    use super::*;

    macro_rules! some {
        ($value:expr, $message:literal) => {{
            let value = $value;
            assert!(value.is_some(), $message);
            let Some(value) = value else {
                return;
            };
            value
        }};
    }

    fn batch_content_test(input: &[u8]) -> Option<(Cow<'_, [u8]>, ContentBatchStats)> {
        let events = events_for(input, &[b"gs", b"d"])?;
        batch_content(input, &events)
    }

    fn batch_content_reparsed(input: &[u8]) -> Option<(Vec<u8>, ContentBatchStats)> {
        let fill_events = events_for(input, &[])?;
        if !page_paint_batching_is_safe(&fill_events.records) {
            return Some((input.to_vec(), ContentBatchStats::default()));
        }
        let (filled, fill_stats) = batch_closed_fills(input, &fill_events)?;

        let stroke_events = events_for(&filled, &[])?;
        let (stroked, stroke_stats) = if page_paint_batching_is_safe(&stroke_events.records) {
            batch_strokes(&filled, &stroke_events.records)?
        } else {
            (filled, ContentBatchStats::default())
        };
        Some((
            stroked.into_owned(),
            ContentBatchStats {
                groups_created: fill_stats
                    .groups_created
                    .saturating_add(stroke_stats.groups_created),
                source_paints_batched: fill_stats
                    .source_paints_batched
                    .saturating_add(stroke_stats.source_paints_batched),
                paints_eliminated: fill_stats
                    .paints_eliminated
                    .saturating_add(stroke_stats.paints_eliminated),
            },
        ))
    }

    fn operator_count(output: &[u8], operator: &[u8]) -> usize {
        let events = events_for(output, &[]);
        assert!(events.is_some(), "parse output");
        let Some(events) = events else {
            return 0;
        };
        events
            .records
            .iter()
            .filter(|event| event.operator.as_slice() == operator)
            .count()
    }

    fn assert_known_operators(output: &[u8]) {
        let events = events_for(output, &[]);
        assert!(events.is_some(), "parse output");
        let Some(events) = events else {
            return;
        };
        assert!(
            events.records.iter().all(|event| {
                matches!(
                    event.operator.as_slice(),
                    b"q" | b"Q"
                        | b"M"
                        | b"m"
                        | b"l"
                        | b"c"
                        | b"v"
                        | b"y"
                        | b"h"
                        | b"re"
                        | b"B"
                        | b"b"
                        | b"S"
                        | b"w"
                        | b"J"
                        | b"j"
                )
            }),
            "generated an unexpected glued/unknown operator"
        );
    }

    #[test]
    fn compact_events_preserve_operator_and_operand_facets() -> Result<()> {
        let input = b"1 2 #6d /OC /Layer#20one BDC 9007199254740993 0 l 2 /Other l q 1 2 3 4 5 6 7 scn unknownLongOperator EMC";
        let events = events_for(input, &[]).ok_or_else(|| {
            crate::Error::Invalid("event projection fixture did not parse".into())
        })?;
        assert_eq!(events.records.len(), 8);
        assert_eq!(
            events.numbers,
            [1.0, 2.0, 1.0, 2.0, 3.0, 4.0, 5.0, 6.0, 7.0]
        );
        assert_eq!(events.records[0].operator.as_slice(), b"m");
        assert_eq!(events.records[0].numbers(&events.numbers), &[1.0, 2.0]);
        assert!(!events.records[1].all_operands_numeric());
        for event in &events.records[2..4] {
            assert!(!event.all_operands_numeric());
            assert_eq!(event.operand_count, 2);
            assert_eq!(event.numbers(&events.numbers), &[] as &[f64]);
        }
        assert!(events.records[4].all_operands_numeric());
        assert_eq!(events.records[4].operand_count, 0);
        assert_eq!(
            events.records[5].numbers(&events.numbers),
            &[1.0, 2.0, 3.0, 4.0, 5.0, 6.0, 7.0]
        );
        assert_eq!(
            events.records[6].operator.as_slice(),
            b"unknownLongOperator"
        );
        assert_eq!(
            &input[events.records[0].start..events.records[0].end],
            b"1 2 #6d"
        );
        Ok(())
    }

    #[test]
    fn stroke_scan_evidence_requires_live_exact_snapshot() -> Result<()> {
        let page = ObjectHandle::Existing(crate::ObjectId::new(1, 0));
        let mut decoded = Arc::new(b"q 0 0 m 1 1 l S Q".to_vec());
        let owner = Arc::clone(&decoded);
        let events = events_for(&decoded, &[b"gs", b"d"])
            .ok_or_else(|| crate::Error::Invalid("scan fixture did not parse".into()))?;
        let mut evidence = StrokeFormScanEvidence::default();
        evidence.remember(page, &decoded, Some(&events));
        let original = evidence
            .take_for(page, &decoded)
            .and_then(|scan| scan.occurrences)
            .ok_or_else(|| crate::Error::Invalid("exact snapshot should reuse scan".into()))?;
        let reparsed = stroke_form_occurrences(&decoded)
            .ok_or_else(|| crate::Error::Invalid("fixture did not reparse".into()))?;
        assert_eq!(original.len(), 1);
        assert_eq!(original[0].signature, reparsed[0].signature);
        assert_eq!(original[0].body_start, reparsed[0].body_start);
        assert!(evidence.take_for(page, &decoded).is_none());

        evidence.remember(page, &decoded, Some(&events));
        let same_bytes_new_allocation = Arc::new(decoded.as_ref().clone());
        assert!(
            evidence
                .take_for(page, &same_bytes_new_allocation)
                .is_none()
        );

        evidence.remember(page, &decoded, Some(&events));
        drop(owner);
        Arc::make_mut(&mut decoded).extend_from_slice(b" q Q");
        assert!(evidence.take_for(page, &decoded).is_none());
        evidence.remember(page, &decoded, None);
        assert!(
            evidence.pages.is_empty(),
            "do not retain uniquely owned decoded content"
        );
        Ok(())
    }

    #[test]
    fn stroke_scan_evidence_distinguishes_disqualified_and_incomplete_streams() -> Result<()> {
        let page = ObjectHandle::Existing(crate::ObjectId::new(1, 0));
        let mut evidence = StrokeFormScanEvidence::default();
        for (input, complete) in [(b"/GS gs q Q".as_slice(), true), (b"/GS gs [1 2", false)] {
            let decoded = Arc::new(input.to_vec());
            let owner = Arc::clone(&decoded);
            let events = events_for(&decoded, &[b"gs", b"d"]);
            evidence.remember(page, &decoded, events.as_ref());
            let scan = evidence
                .take_for(page, &decoded)
                .ok_or_else(|| crate::Error::Invalid("scan evidence missing".into()))?;
            assert_eq!(scan.occurrences.is_some(), complete);
            assert!(scan.occurrences.is_none_or(|items| items.is_empty()));
            evidence.remember(page, &decoded, events.as_ref());
            drop(decoded);
            drop(owner);
            assert!(evidence.pages[&page].snapshot.upgrade().is_none());
        }
        Ok(())
    }

    #[test]
    fn same_snapshot_event_reuse_matches_reparsed_reference() {
        let inputs: [&[u8]; 2] = [
            b"0 0 m 1 0 l S 2 0 m 3 0 l S 4 0 m 5 0 l S",
            b"q 1 M 0 0 m 1 0 l 1 1 l h b Q q 1 M 2 0 m 3 0 l 3 1 l h b Q 4 0 m 5 0 l S 6 0 m 7 0 l S",
        ];
        for input in inputs {
            let optimized = some!(batch_content_test(input), "optimized parse");
            let reparsed = some!(batch_content_reparsed(input), "reference parse");
            assert_eq!(optimized.0.as_ref(), reparsed.0.as_slice());
            assert_eq!(optimized.1, reparsed.1);
        }
    }

    #[test]
    fn rejected_long_path_advances_once_to_its_terminator() -> Result<()> {
        let mut input = b"0 0 m ".to_vec();
        input.extend_from_slice(&b"1 1 l ".repeat(20_000));
        input.extend_from_slice(b"f 0 0 m 1 0 l S 2 0 m 3 0 l S");
        let events =
            events_for(&input, &[]).ok_or_else(|| crate::Error::Invalid("path fixture".into()))?;
        let (candidate, next) = stroke_transaction(&input, &events.records, 0);
        assert!(candidate.is_none());
        assert_eq!(next, 20_001);
        assert_eq!(events.records[next].operator.as_slice(), b"f");
        let (output, stats) = batch_strokes(&input, &events.records)
            .ok_or_else(|| crate::Error::Invalid("stroke fixture".into()))?;
        assert_eq!(stats.paints_eliminated, 1);
        assert_eq!(operator_count(&output, b"f"), 1);
        assert_eq!(operator_count(&output, b"S"), 1);
        assert!(output.starts_with(&input[..events.records[next].end]));
        Ok(())
    }

    #[test]
    fn event_disqualification_discards_projection_but_preserves_parse_outcome() {
        let input = b"0 0 m 1 1 l S /GS #67s 2 2 m 3 3 l S";
        assert!(
            events_for(input, &[b"gs", b"d"])
                .is_some_and(|events| events.records.is_empty() && events.numbers.is_empty())
        );
        assert!(events_for(input, &[]).is_some_and(|events| events.records.len() == 7));
        let malformed = b"/GS gs [1 2";
        assert_eq!(
            events_for(malformed, &[b"gs"]).is_none(),
            events_for(malformed, &[]).is_none()
        );
    }

    #[test]
    fn batches_consecutive_strokes_without_reordering() {
        let input = b"0 0 m 1 0 l S 2 0 m 3 0 l S 4 0 m 5 0 l S";
        let (output, stats) = some!(batch_content_test(input), "parse");
        assert_eq!(stats.source_paints_batched, 3);
        assert_eq!(stats.paints_eliminated, 2);
        assert_eq!(operator_count(&output, b"S"), 1);
        assert!(output.windows(b"0 0 m".len()).any(|w| w == b"0 0 m"));
        assert!(output.windows(b"4 0 m".len()).any(|w| w == b"4 0 m"));
        assert_known_operators(&output);
    }

    #[test]
    fn graphics_state_change_splits_stroke_batches() {
        let input = b"0 0 m 1 0 l S 2 w 2 0 m 3 0 l S";
        let (output, stats) = some!(batch_content_test(input), "parse");
        assert_eq!(stats.paints_eliminated, 0);
        assert_eq!(output.as_ref(), &input[..]);
    }

    #[test]
    fn ext_gstate_disables_page_paint_batching() {
        let input = b"/GS1 gs 0 0 m 1 0 l S 2 0 m 3 0 l S";
        let (output, stats) = some!(batch_content_test(input), "parse");
        assert_eq!(stats.paints_eliminated, 0);
        assert_eq!(output.as_ref(), &input[..]);
    }

    #[test]
    fn batches_same_winding_closed_fill_stroke_transactions() {
        let input = b"q 1 M 0 0 m 1 0 l 1 1 l h b Q q 1 M 0.5 0 m 1.5 0 l 1.5 1 l h b Q";
        let (output, stats) = some!(batch_content_test(input), "parse");
        assert_eq!(stats.source_paints_batched, 2);
        assert_eq!(stats.paints_eliminated, 1);
        assert_eq!(operator_count(&output, b"B"), 1);
        assert_known_operators(&output);
    }

    #[test]
    fn opposite_winding_overlap_is_not_batched() {
        let input = b"q 1 M 0 0 m 1 0 l 1 1 l h b Q q 1 M 0.5 0 m 0.5 1 l 1.5 1 l h b Q";
        let (output, stats) = some!(batch_content_test(input), "parse");
        assert_eq!(stats.paints_eliminated, 0);
        assert_eq!(output.as_ref(), &input[..]);
    }

    #[test]
    fn opposite_winding_disjoint_paths_are_batched() {
        let input = b"q 1 M 0 0 m 1 0 l 1 1 l h b Q q 1 M 2 0 m 2 1 l 3 1 l h b Q";
        let (output, stats) = some!(batch_content_test(input), "parse");
        assert_eq!(stats.paints_eliminated, 1);
        assert_known_operators(&output);
    }

    #[test]
    fn zero_area_fill_can_join_any_winding() {
        let input = b"q 1 M 0 0 m 1 0 l h b Q q 1 M 0 0 m 1 0 l 1 1 l h b Q";
        let (output, stats) = some!(batch_content_test(input), "parse");
        assert_eq!(stats.paints_eliminated, 1);
        assert_known_operators(&output);
    }

    #[test]
    fn differing_miter_limit_splits_closed_fill_batches() {
        let input = b"q 1 M 0 0 m 1 0 l 1 1 l h b Q q 2 M 2 0 m 3 0 l 3 1 l h b Q";
        let (output, stats) = some!(batch_content_test(input), "parse");
        assert_eq!(stats.paints_eliminated, 0);
        assert_eq!(output.as_ref(), &input[..]);
    }

    #[test]
    fn processing_page_batching_accepts_structural_win_even_if_flate_grows() -> Result<()> {
        use crate::test_support::ClassicPdfBuilder;
        use std::io::Write as _;

        let mut content = Vec::new();
        for index in 0..17 {
            let x = index % 10;
            write!(&mut content, "{x} 0 m {x} 1 l S ")?;
        }
        content.resize(MIN_PAGE_CONTENT_BYTES + 512, b' ');

        let Some((batched, candidate_stats)) = batch_content_test(&content) else {
            return Err(crate::Error::Invalid(
                "test paint stream did not parse".to_owned(),
            ));
        };
        assert!(candidate_stats.paints_eliminated >= MIN_PAINTS_ELIMINATED);
        assert!(
            crate::stream_codec::flate_encoded_len(&batched, crate::FlateLevel::PROCESSING)?
                > crate::stream_codec::flate_encoded_len(&content, crate::FlateLevel::PROCESSING)?,
            "fixture must exercise the structural-over-size policy"
        );

        let mut pdf = ClassicPdfBuilder::new();
        pdf.object(1, b"<< /Type /Catalog /Pages 2 0 R >>")?;
        pdf.object(2, b"<< /Type /Pages /Kids [3 0 R] /Count 1 >>")?;
        pdf.object(
            3,
            b"<< /Type /Page /Parent 2 0 R /MediaBox [0 0 100 100] /Resources << >> /Contents 4 0 R >>",
        )?;
        pdf.stream(4, b"", &content)?;

        let mut document = EditDocument::from_bytes(pdf.finish(1)?)?;
        let pages = document.page_handles()?;
        let stats = batch_page_paints(
            &mut document,
            &pages,
            &mut StrokeFormScanEvidence::default(),
            &mut crate::flate::FlateReuseCache::default(),
        )?;
        assert_eq!(stats.pages_rewritten, 1);
        assert!(stats.paints_eliminated >= MIN_PAINTS_ELIMINATED);
        Ok(())
    }

    #[test]
    fn batching_is_capped_to_bounded_compound_paths() {
        let mut input = Vec::new();
        for index in 0..=MAX_SOURCE_PATHS_PER_BATCH {
            input.extend_from_slice(format!("{index} 0 m {index} 1 l S ").as_bytes());
        }
        let (output, stats) = some!(batch_content_test(&input), "parse");
        assert_eq!(stats.source_paints_batched, MAX_SOURCE_PATHS_PER_BATCH + 1);
        assert_eq!(stats.paints_eliminated, MAX_SOURCE_PATHS_PER_BATCH - 1);
        assert_eq!(operator_count(&output, b"S"), 2);
        assert_known_operators(&output);
    }
    #[test]
    fn outlined_glyph_components_match_after_translation() {
        let input = b"q 1 M 0 0 m 1 0 l 1 1 l h b Q q 1 M 1 0 m 2 0 l 2 1 l h b Q q 1 M 10 0 m 11 0 l 11 1 l h b Q q 1 M 11 0 m 12 0 l 12 1 l h b Q";
        let components = some!(outlined_glyph_components(input), "parse");
        assert_eq!(components.len(), 2);
        assert_eq!(components[0].source_paints, 2);
        assert_eq!(components[1].source_paints, 2);
        assert_eq!(components[0].signature, components[1].signature);
    }

    #[test]
    fn outlined_glyph_factor_rejects_noncontiguous_components() {
        let input = b"q 1 M 0 0 m 1 0 l 1 1 l h b Q q 1 M 10 0 m 11 0 l 11 1 l h b Q q 1 M 1 0 m 2 0 l 2 1 l h b Q";
        let components = some!(outlined_glyph_components(input), "parse");
        assert_eq!(components.len(), 1);
        assert_eq!(components[0].source_paints, 1);
        assert_eq!(components[0].origin_x, 1000);
    }

    #[test]
    fn outlined_glyph_factor_leaves_existing_text_pages_alone() {
        let input = b"BT ET q 1 M 0 0 m 1 0 l 1 1 l h b Q q 1 M 1 0 m 2 0 l 2 1 l h b Q";
        let components = some!(outlined_glyph_components(input), "parse");
        assert!(components.is_empty());
    }
    #[test]
    fn coalesces_adjacent_identical_optional_content() {
        let input = b"/OC /MC0 BDC 0 0 m 1 0 l S EMC /OC /MC0 BDC 2 0 m 3 0 l S EMC";
        let (output, count) = some!(coalesce_adjacent_ocg_content(input), "parse");
        assert_eq!(count, 1);
        assert_eq!(operator_count(&output, b"BDC"), 1);
        assert_eq!(operator_count(&output, b"EMC"), 1);
        assert!(
            output
                .windows(b"2 0 m".len())
                .any(|window| window == b"2 0 m")
        );
    }

    #[test]
    fn streams_nested_ocg_boundaries_and_preserves_escaped_names() {
        let input = b"/OC /Outer BDC /OC /Layer#20A BDC EMC %gap\n/OC /Layer#20A #42DC EMC /OC /Layer#20A BDC EMC EMC";
        let (output, count) = some!(coalesce_adjacent_ocg_content(input), "parse");
        assert_eq!(count, 2);
        assert_eq!(operator_count(&output, b"BDC"), 2);
        assert_eq!(operator_count(&output, b"EMC"), 2);
    }

    #[test]
    fn keeps_different_optional_content_boundaries() {
        let input = b"/OC /MC0 BDC 0 0 m 1 0 l S EMC /OC /MC1 BDC 2 0 m 3 0 l S EMC";
        let (output, count) = some!(coalesce_adjacent_ocg_content(input), "parse");
        assert_eq!(count, 0);
        assert_eq!(output.as_ref(), input);
        assert!(matches!(output, Cow::Borrowed(_)));
    }

    #[test]
    fn keeps_non_oc_marked_content_boundaries() {
        let input = b"/Span /P0 BDC 0 0 m 1 0 l S EMC /Span /P0 BDC 2 0 m 3 0 l S EMC";
        let (output, count) = some!(coalesce_adjacent_ocg_content(input), "parse");
        assert_eq!(count, 0);
        assert_eq!(output.as_ref(), input);
        assert!(matches!(output, Cow::Borrowed(_)));
    }
    #[test]
    fn form_collinear_material_gate_requires_structural_or_encoded_win() {
        assert!(!form_collinear_material(1023, 4095));
        assert!(form_collinear_material(1024, 0));
        assert!(form_collinear_material(0, 4096));
    }

    #[test]
    fn fused_collinear_normalization_matches_authoritative_reparse() {
        let cases: &[&[u8]] = &[
            b"0 0 m 1 0 l % trivia\n2 0 l S",
            b"0 0 m 1 1 l 2 2 l 3 1 l 4 0 l S",
            b"0 0 m 1 0 #6c 2 0 l S",
            b"0 0 m 1 0 l 2 0 l m#20S 4 4 m 5 5 l 6 6 l f",
            b"0 0 m 1 0 l 2 0 l S BI /W 1 /H 1 /BPC 8 /CS /G ID x EI q Q",
            b"0 0 m 1 0 l 2 0 l S [1 2",
            b"0 0 m 1 1 l 2 0 l S",
            b"9007199254740993 0 m 1 0 l 2 0 l S",
            b"0 0 m 1.00 -0.0 l 2 0 l S /Span <</ActualText(hello)>> BDC EMC",
        ];
        for &input in cases {
            let normalized = crate::content_stream::normalize_content_stream(input, |_, _| {})
                .unwrap_or_else(|| input.to_vec());
            let expected = compact_collinear_line_points(&normalized);
            let plan = normalized_collinear_plan(input);
            match expected {
                Some((rewritten, count)) if count != 0 => {
                    let plan = some!(plan, "fused plan");
                    assert_eq!(plan.before.as_ref(), normalized.as_slice());
                    assert_eq!(plan.after.as_slice(), rewritten.as_ref());
                    assert_eq!(plan.operations, count);
                }
                _ => assert!(plan.is_none()),
            }
        }
    }

    #[test]
    fn page_collinear_compaction_uses_processing_normalized_representation() -> Result<()> {
        use crate::test_support::ClassicPdfBuilder;
        use std::io::Write as _;

        let mut content = Vec::new();
        for y in 0..4000 {
            writeln!(
                &mut content,
                "0 {y} m   1 {y} l % removable trivia {y}\n   2 {y} l S"
            )?;
        }
        assert!(content.len() >= MIN_PAGE_CONTENT_BYTES);

        let mut pdf = ClassicPdfBuilder::new();
        pdf.object(1, b"<< /Type /Catalog /Pages 2 0 R >>")?;
        pdf.object(2, b"<< /Type /Pages /Kids [3 0 R] /Count 1 >>")?;
        pdf.object(
            3,
            b"<< /Type /Page /Parent 2 0 R /MediaBox [0 0 10 4000] /Resources << >> /Contents 4 0 R >>",
        )?;
        pdf.stream(4, b"", &content)?;

        let mut document = EditDocument::from_bytes(pdf.finish(1)?)?;
        let pages = document.page_handles()?;
        let stats = compact_collinear_paths(
            &mut document,
            &pages,
            crate::FlateLevel::PROCESSING,
            &mut crate::flate::FlateReuseCache::default(),
        )?;
        assert_eq!(stats.pages_rewritten, 1);
        assert_eq!(stats.forms_rewritten, 0);
        assert_eq!(stats.vertices_removed, 4000);

        let pages = document.page_handles()?;
        assert_eq!(pages.len(), 1);
        let Some(object) = document.current_owned_object(pages[0])? else {
            return Err(crate::Error::Invalid("test page disappeared".to_owned()));
        };
        let Some(dictionary) = object.as_dictionary() else {
            return Err(crate::Error::Invalid(
                "test page is not a dictionary".to_owned(),
            ));
        };
        let Some(contents) = dictionary.get(b"Contents".as_slice()) else {
            return Err(crate::Error::Invalid("test page lost Contents".to_owned()));
        };
        let decoded = crate::content::decoded_content_value(
            &document,
            crate::source::CurrentValue::Owned(contents),
        )?;
        assert!(!decoded.contains(&b'%'));
        assert_eq!(operator_count(&decoded, b"l"), 4000);
        Ok(())
    }

    #[test]
    fn compacts_large_collinear_form_xobject() -> Result<()> {
        use crate::test_support::ClassicPdfBuilder;
        use std::io::Write as _;

        let mut form = b"0 0 m ".to_vec();
        for x in 1..=12_000 {
            write!(&mut form, "{x} 0 l ")?;
        }
        form.extend_from_slice(b"S");

        let mut pdf = ClassicPdfBuilder::new();
        pdf.object(1, b"<< /Type /Catalog /Pages 2 0 R >>")?;
        pdf.object(2, b"<< /Type /Pages /Kids [3 0 R] /Count 1 >>")?;
        pdf.object(
            3,
            b"<< /Type /Page /Parent 2 0 R /MediaBox [0 0 12000 10] /Resources << /XObject << /Fm0 5 0 R >> >> /Contents 4 0 R >>",
        )?;
        pdf.stream(4, b"", b"q /Fm0 Do Q")?;
        pdf.stream(
            5,
            b"/Type /XObject /Subtype /Form /FormType 1 /BBox [0 0 12000 10] /Resources << >>",
            &form,
        )?;

        let mut document = EditDocument::from_bytes(pdf.finish(1)?)?;
        let pages = document.page_handles()?;
        let stats = compact_collinear_paths(
            &mut document,
            &pages,
            crate::FlateLevel::PROCESSING,
            &mut crate::flate::FlateReuseCache::default(),
        )?;
        assert_eq!(stats.pages_rewritten, 0);
        assert_eq!(stats.forms_rewritten, 1);
        assert!(stats.vertices_removed > 10_000);

        let forms = document.reachable_streams_with_subtype(b"Form")?;
        assert_eq!(forms.len(), 1);
        let Some(object) = document.current_owned_object(forms[0])? else {
            return Err(crate::Error::Invalid("test Form disappeared".to_owned()));
        };
        let decoded =
            document.decoded_stream_value(&crate::source::CurrentValue::Owned(&object))?;
        assert_eq!(operator_count(&decoded, b"l"), 1);

        let output = document.write_compact()?;
        let reparsed = EditDocument::from_bytes(output)?;
        assert_eq!(reparsed.reachable_streams_with_subtype(b"Form")?.len(), 1);
        Ok(())
    }

    #[test]
    fn removes_exact_forward_collinear_vertex() {
        let input = b"0 0 m 1 0 l 2 0 l S";
        let (output, removed) = some!(compact_collinear_line_points(input), "parse");
        assert_eq!(removed, 1);
        assert!(
            !output
                .windows(b"1 0 l".len())
                .any(|window| window == b"1 0 l")
        );
        assert!(
            output
                .windows(b"2 0 l".len())
                .any(|window| window == b"2 0 l")
        );
    }

    #[test]
    fn keeps_corner_reversal_and_duplicate_vertices() {
        for input in [
            b"0 0 m 1 0 l 1 1 l S".as_slice(),
            b"0 0 m 1 0 l 0 0 l S".as_slice(),
            b"0 0 m 1 0 l 1 0 l S".as_slice(),
        ] {
            let (output, removed) = some!(compact_collinear_line_points(input), "parse");
            assert_eq!(removed, 0);
            assert_eq!(output.as_ref(), input);
            assert!(matches!(output, Cow::Borrowed(_)));
        }
    }

    #[test]
    fn keeps_off_grid_collinear_vertices() {
        let input = b"0 0 m 0.001 0 l 0.002 0 l S";
        let (output, removed) = some!(compact_collinear_line_points(input), "parse");
        assert_eq!(removed, 0);
        assert_eq!(output.as_ref(), input);
        assert!(matches!(output, Cow::Borrowed(_)));
    }

    #[test]
    fn stroke_form_occurrences_match_translation_and_inherited_state() {
        let input =
            b"1 w 2 M q 0 0 m 1 0 l 1 1 l 0 1 l h S Q 4 w 5 M q 10 0 m 11 0 l 11 1 l 10 1 l h S Q";
        let occurrences = some!(stroke_form_occurrences(input), "parse");
        assert_eq!(occurrences.len(), 2);
        assert_eq!(occurrences[0].signature, occurrences[1].signature);
        assert_eq!((occurrences[0].origin_x, occurrences[0].origin_y), (0, 0));
        assert_eq!(
            (occurrences[1].origin_x, occurrences[1].origin_y),
            (1000, 0)
        );
        assert_eq!(occurrences[0].bbox_margin_grid, 101);
        assert_eq!(occurrences[1].bbox_margin_grid, 1001);

        let replacement = some!(
            stroke_form_replacement(b"PdfRedoxStroke0", &occurrences[0], &occurrences[1]),
            "translation"
        );
        assert_eq!(replacement, b"q\n1 0 0 1 10 0 cm\n/PdfRedoxStroke0 Do\nQ\n");
    }

    #[test]
    fn stroke_admission_preserves_names_on_pages_without_candidates() -> Result<()> {
        use crate::test_support::ClassicPdfBuilder;
        use std::io::Write as _;

        let mut strokes = Vec::new();
        for translation in 0..100 {
            let x = translation * 1013;
            let y = translation * 97;
            writeln!(&mut strokes, "q {x} {y} m")?;
            for point in 1..200 {
                let dx = (point * 163) % 997;
                let dy = (point * point * 47) % 991;
                writeln!(&mut strokes, "{} {} l", x + dx, y + dy)?;
            }
            writeln!(&mut strokes, "S Q")?;
        }
        let other = b"q Q\n".repeat(MIN_PAGE_CONTENT_BYTES / 4 + 1);
        let mut pdf = ClassicPdfBuilder::new();
        pdf.object(1, b"<< /Type /Catalog /Pages 2 0 R >>")?;
        pdf.object(2, b"<< /Type /Pages /Kids [3 0 R 4 0 R] /Count 2 >>")?;
        pdf.object(3, b"<< /Type /Page /Parent 2 0 R /MediaBox [0 0 120000 12000] /Resources << >> /Contents 5 0 R >>")?;
        pdf.object(4, b"<< /Type /Page /Parent 2 0 R /MediaBox [0 0 100 100] /Resources << /XObject << /PdfRedoxStroke0 7 0 R >> >> /Contents 6 0 R >>")?;
        pdf.stream(5, b"", &strokes)?;
        pdf.stream(6, b"", &other)?;
        pdf.stream(7, b"/Type /XObject /Subtype /Form /BBox [0 0 1 1]", b"q Q")?;

        let fixture = pdf.finish(1)?;
        let mut cold = EditDocument::from_bytes(fixture.clone())?;
        let cold_pages = cold.page_handles()?;
        let cold_stats = factor_repeated_stroke_forms(
            &mut cold,
            &cold_pages,
            crate::FlateLevel::PROCESSING,
            &mut StrokeFormScanEvidence::default(),
        )?;

        let mut document = EditDocument::from_bytes(fixture)?;
        let pages = document.page_handles()?;
        let mut evidence = StrokeFormScanEvidence::default();
        let batch_stats = batch_page_paints(&mut document, &pages, &mut evidence)?;
        assert_eq!(batch_stats.pages_rewritten, 0);
        assert_eq!(evidence.pages.len(), 2);
        let stats = factor_repeated_stroke_forms(
            &mut document,
            &pages,
            crate::FlateLevel::PROCESSING,
            &mut evidence,
            &mut crate::flate::FlateReuseCache::default(),
        )?;
        assert!(evidence.pages.is_empty());
        assert_eq!(stats, cold_stats);
        assert_eq!(document.write_compact()?, cold.write_compact()?);
        assert_eq!(stats.pages_rewritten, 1);
        assert_eq!(stats.forms_created, 1);
        assert_eq!(stats.occurrences_replaced, 100);
        assert!(stats.estimated_flate_bytes_saved >= MIN_STROKE_FORM_ESTIMATED_SAVINGS);
        let rewritten = page_content_if_present(&document, pages[0])?
            .ok_or_else(|| crate::Error::Invalid("missing rewritten stroke page".into()))?;
        assert_eq!(operator_count(&rewritten, b"Do"), 100);
        assert!(
            rewritten
                .windows(b"/PdfRedoxStroke1".len())
                .any(|name| name == b"/PdfRedoxStroke1")
        );
        let untouched = page_content_if_present(&document, pages[1])?
            .ok_or_else(|| crate::Error::Invalid("missing non-candidate page".into()))?;
        assert_eq!(untouched.as_slice(), other.as_slice());
        Ok(())
    }

    #[test]
    fn stroke_form_occurrence_rejects_open_caller_path() {
        let input = b"0 0 m q 10 0 m 11 0 l S Q";
        let occurrences = some!(stroke_form_occurrences(input), "parse");
        assert!(occurrences.is_empty());
    }

    #[test]
    fn stroke_form_occurrence_rejects_unsafe_extended_graphics_state() {
        let input = b"/GS0 gs q 0 0 m 1 0 l 1 1 l 0 1 l h S Q q 10 0 m 11 0 l 11 1 l 10 1 l h S Q";
        let occurrences = some!(stroke_form_occurrences(input), "parse");
        assert!(occurrences.is_empty());
    }

    #[test]
    fn paint_batching_is_idempotent() {
        let strokes = b"0 0 m 1 0 l S 2 0 m 3 0 l S 4 0 m 5 0 l S";
        let (first, first_stats) = some!(batch_content_test(strokes), "parse");
        assert!(first_stats.paints_eliminated > 0);
        let (second, second_stats) = some!(batch_content_test(&first), "parse generated output");
        assert_eq!(second_stats.paints_eliminated, 0);
        assert_eq!(second, first);

        let fills = b"q 1 M 0 0 m 1 0 l 1 1 l h b Q q 1 M 2 0 m 3 0 l 3 1 l h b Q";
        let (first, first_stats) = some!(batch_content_test(fills), "parse");
        assert!(first_stats.paints_eliminated > 0);
        let (second, second_stats) = some!(batch_content_test(&first), "parse generated output");
        assert_eq!(second_stats.paints_eliminated, 0);
        assert_eq!(second, first);
    }
}
