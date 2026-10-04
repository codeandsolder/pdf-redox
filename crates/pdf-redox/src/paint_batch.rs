use crate::{
    EditDocument, ObjectHandle, OwnedDictionary, OwnedObject, Result, StreamData,
    content::{decoded_content_value, replace_page_content, resolved_dictionary},
};
use flate2::{Compression, write::ZlibEncoder};
use flpdf::content_stream::ContentScalar;
use flpdf::{ObjectHandle as FlObjectHandle, ObjectHandleParserCallbacks, ParseControl};
use smallvec::SmallVec;
use std::{
    collections::{BTreeMap, BTreeSet},
    io::Write as _,
};

const MAX_SOURCE_PATHS_PER_BATCH: usize = 64;
const MIN_PAGE_CONTENT_BYTES: usize = 64 * 1024;
const MIN_PAINTS_ELIMINATED: usize = 16;
const GEOMETRY_EPSILON: f64 = 1.0e-10;

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PaintBatchStats {
    pub pages_rewritten: usize,
    pub groups_created: usize,
    pub source_paints_batched: usize,
    pub paints_eliminated: usize,
    pub decoded_bytes_removed: usize,
    pub estimated_flate_bytes_saved: usize,
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
    pub vertices_removed: usize,
    pub decoded_bytes_removed: usize,
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

#[derive(Debug, Clone)]
struct Event {
    start: usize,
    end: usize,
    operator: Vec<u8>,
    operand_count: usize,
    sole_number: Option<f64>,
    numbers: SmallVec<[f64; 6]>,
    all_operands_numeric: bool,
    two_names: Option<[Vec<u8>; 2]>,
}

#[derive(Debug, Clone)]
struct Operand {
    offset: usize,
    number: Option<f64>,
    name: Option<Vec<u8>>,
}

#[derive(Default)]
struct EventScanner {
    operands: Vec<Operand>,
    events: Vec<Event>,
}

impl EventScanner {
    fn push_operator(&mut self, operator: &[u8], offset: usize, length: usize) {
        let start = self
            .operands
            .first()
            .map_or(offset, |operand| operand.offset);
        let sole_number = (self.operands.len() == 1)
            .then(|| self.operands[0].number)
            .flatten();
        let all_operands_numeric = self
            .operands
            .iter()
            .all(|operand| operand.number.is_some_and(f64::is_finite));
        let mut numbers = SmallVec::new();
        if all_operands_numeric {
            numbers.extend(self.operands.iter().filter_map(|operand| operand.number));
        }
        let two_names = if self.operands.len() == 2 {
            match (&self.operands[0].name, &self.operands[1].name) {
                (Some(first), Some(second)) => Some([first.clone(), second.clone()]),
                _ => None,
            }
        } else {
            None
        };
        self.events.push(Event {
            start,
            end: offset.saturating_add(length),
            operator: operator.to_vec(),
            operand_count: self.operands.len(),
            sole_number,
            numbers,
            all_operands_numeric,
            two_names,
        });
        self.operands.clear();
    }

    fn push_scalar(&mut self, scalar: &ContentScalar, offset: usize) {
        self.operands.push(Operand {
            offset,
            number: scalar
                .as_integer()
                .and_then(crate::source::exact_i64_to_f64)
                .or_else(|| scalar.as_real()),
            name: scalar.as_name().map(ToOwned::to_owned),
        });
    }

    fn push_object(&mut self, object: &FlObjectHandle, offset: usize) {
        self.operands.push(Operand {
            offset,
            number: object
                .as_integer()
                .and_then(crate::source::exact_i64_to_f64)
                .or_else(|| object.as_real()),
            name: object.as_name(),
        });
    }
}

impl ObjectHandleParserCallbacks for EventScanner {
    const HANDLES_CONTENT_SCALARS: bool = true;

    fn handle_scalar(
        &mut self,
        scalar: ContentScalar,
        offset: usize,
        length: usize,
    ) -> flpdf::Result<ParseControl> {
        if let Some(operator) = scalar.as_operator() {
            self.push_operator(operator, offset, length);
        } else {
            self.push_scalar(&scalar, offset);
        }
        Ok(ParseControl::Continue)
    }

    fn handle_operator(
        &mut self,
        operator: &[u8],
        offset: usize,
        length: usize,
    ) -> flpdf::Result<ParseControl> {
        self.push_operator(operator, offset, length);
        Ok(ParseControl::Continue)
    }

    fn handle_object(
        &mut self,
        object: FlObjectHandle,
        offset: usize,
        length: usize,
    ) -> flpdf::Result<ParseControl> {
        if let Some(operator) = object.as_operator() {
            self.push_operator(&operator, offset, length);
        } else if object.as_inline_image().is_some() {
            self.operands.clear();
            self.events.push(Event {
                start: offset,
                end: offset.saturating_add(length),
                operator: b"__inline_image__".to_vec(),
                operand_count: 0,
                sole_number: None,
                numbers: SmallVec::new(),
                all_operands_numeric: false,
                two_names: None,
            });
        } else {
            self.push_object(&object, offset);
        }
        Ok(ParseControl::Continue)
    }

    fn handle_eof(&mut self) -> flpdf::Result<()> {
        Ok(())
    }
}

const fn trivia_only(bytes: &[u8]) -> bool {
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

const fn path_operator(operator: &[u8]) -> bool {
    matches!(operator, b"m" | b"l" | b"c" | b"v" | b"y" | b"h" | b"re")
}

fn adjacent(input: &[u8], left: &Event, right: &Event) -> bool {
    left.end <= right.start && input.get(left.end..right.start).is_some_and(trivia_only)
}

fn events_for(input: &[u8], context: &str) -> Option<Vec<Event>> {
    let mut scanner = EventScanner::default();
    flpdf::parse_detached_content_stream(input, context, &mut scanner).ok()?;
    Some(scanner.events)
}

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

fn event_points(event: &Event) -> Option<SmallVec<[Point; 3]>> {
    if !event.all_operands_numeric {
        return None;
    }
    let mut points = SmallVec::new();
    match event.operator.as_slice() {
        b"m" | b"l" if event.numbers.len() == 2 => points.push(Point {
            x: event.numbers[0],
            y: event.numbers[1],
        }),
        b"c" if event.numbers.len() == 6 => {
            for pair in event.numbers.as_slice().as_chunks::<2>().0 {
                points.push(Point {
                    x: pair[0],
                    y: pair[1],
                });
            }
        }
        b"v" | b"y" if event.numbers.len() == 4 => {
            for pair in event.numbers.as_slice().as_chunks::<2>().0 {
                points.push(Point {
                    x: pair[0],
                    y: pair[1],
                });
            }
        }
        b"re" if event.numbers.len() == 4 => {
            let x = event.numbers[0];
            let y = event.numbers[1];
            let width = event.numbers[2];
            let height = event.numbers[3];
            points.push(Point { x, y });
            points.push(Point {
                x: x + width,
                y: y + height,
            });
        }
        b"h" if event.numbers.is_empty() => {}
        _ => return None,
    }
    Some(points)
}

fn path_bounds(events: &[Event]) -> Option<Bounds> {
    let mut bounds: Option<Bounds> = None;
    for event in events {
        let points = event_points(event)?;
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

fn fill_kind(events: &[Event]) -> FillKind {
    let mut contours = Vec::<Vec<Point>>::new();
    let mut current = Vec::<Point>::new();
    for event in events {
        match event.operator.as_slice() {
            b"m" if event.all_operands_numeric && event.numbers.len() == 2 => {
                if !current.is_empty() {
                    contours.push(std::mem::take(&mut current));
                }
                current.push(Point {
                    x: event.numbers[0],
                    y: event.numbers[1],
                });
            }
            b"l" if event.all_operands_numeric && event.numbers.len() == 2 => {
                if current.is_empty() {
                    return FillKind::Unknown;
                }
                current.push(Point {
                    x: event.numbers[0],
                    y: event.numbers[1],
                });
            }
            b"h" if event.operand_count == 0 => {
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

fn closed_fill_transaction(
    input: &[u8],
    events: &[Event],
    index: usize,
) -> Option<(ClosedFillTransaction, usize)> {
    let q = events.get(index)?;
    let miter = events.get(index + 1)?;
    if q.operator != b"q"
        || q.operand_count != 0
        || miter.operator != b"M"
        || miter.operand_count != 1
        || !adjacent(input, q, miter)
    {
        return None;
    }
    let miter_value = miter.sole_number?;
    if !miter_value.is_finite() {
        return None;
    }

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
    if paint.operator != b"b"
        || paint.operand_count != 0
        || restore.operator != b"Q"
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
            miter_bits: miter_value.to_bits(),
            body_start: first_path.start,
            body_end: paint.start,
            body_already_closed: last_path.operator == b"h",
            bounds: path_bounds(path_events),
            fill_kind: fill_kind(path_events),
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

fn stroke_transaction(
    input: &[u8],
    events: &[Event],
    index: usize,
) -> Option<(StrokeTransaction, usize)> {
    let first = events.get(index)?;
    if !path_operator(&first.operator) {
        return None;
    }
    let mut cursor = index;
    while let Some(next) = events.get(cursor + 1) {
        if path_operator(&next.operator) && adjacent(input, &events[cursor], next) {
            cursor += 1;
        } else {
            break;
        }
    }
    let last_path = events.get(cursor)?;
    let paint = events.get(cursor + 1)?;
    if paint.operator != b"S" || paint.operand_count != 0 || !adjacent(input, last_path, paint) {
        return None;
    }
    Some((
        StrokeTransaction {
            start: first.start,
            end: paint.end,
            body_end: paint.start,
        },
        cursor + 2,
    ))
}

#[derive(Debug, Clone, Default)]
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

fn batch_closed_fills(input: &[u8]) -> Option<(Vec<u8>, ContentBatchStats)> {
    let events = events_for(input, "closed fill/stroke paint batching")?;
    if !page_paint_batching_is_safe(&events) {
        return Some((input.to_vec(), ContentBatchStats::default()));
    }
    let mut replacements = Vec::<(usize, usize, Vec<u8>)>::new();
    let mut stats = ContentBatchStats::default();
    let mut index = 0usize;

    while index < events.len() {
        let Some((first, mut next_index)) = closed_fill_transaction(input, &events, index) else {
            index += 1;
            continue;
        };
        let miter_bits = first.miter_bits;
        let mut group = vec![first];

        while let Some((next, after_next)) = closed_fill_transaction(input, &events, next_index) {
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
        return Some((input.to_vec(), stats));
    }
    let output = apply_replacements(input, replacements)?;
    Some((output, stats))
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

fn batch_strokes(input: &[u8]) -> Option<(Vec<u8>, ContentBatchStats)> {
    let events = events_for(input, "stroke paint batching")?;
    if !page_paint_batching_is_safe(&events) {
        return Some((input.to_vec(), ContentBatchStats::default()));
    }
    let mut replacements = Vec::<(usize, usize, Vec<u8>)>::new();
    let mut stats = ContentBatchStats::default();
    let mut index = 0usize;

    while index < events.len() {
        let Some((first, mut next_index)) = stroke_transaction(input, &events, index) else {
            index += 1;
            continue;
        };
        let mut run = vec![first];
        while next_index < events.len() {
            let Some((next, after_next)) = stroke_transaction(input, &events, next_index) else {
                break;
            };
            let previous = run.last()?;
            if !input.get(previous.end..next.start).is_some_and(trivia_only) {
                break;
            }
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
        index = next_index;
    }

    if replacements.is_empty() {
        return Some((input.to_vec(), stats));
    }
    let output = apply_replacements(input, replacements)?;
    Some((output, stats))
}

fn apply_replacements(
    input: &[u8],
    mut replacements: Vec<(usize, usize, Vec<u8>)>,
) -> Option<Vec<u8>> {
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

fn batch_content(input: &[u8]) -> Option<(Vec<u8>, ContentBatchStats)> {
    let (filled, fill_stats) = batch_closed_fills(input)?;
    let (stroked, stroke_stats) = batch_strokes(&filled)?;
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

#[derive(Debug, Clone, PartialEq, Eq)]
struct OptionalContentKey {
    tag: Vec<u8>,
    property: Vec<u8>,
}

fn optional_content_key(event: &Event) -> Option<OptionalContentKey> {
    if event.operator != b"BDC" || event.operand_count != 2 {
        return None;
    }
    let names = event.two_names.as_ref()?;
    if names[0].as_slice() != b"OC" {
        return None;
    }
    Some(OptionalContentKey {
        tag: names[0].clone(),
        property: names[1].clone(),
    })
}

fn coalesce_adjacent_ocg_content(input: &[u8]) -> Option<(Vec<u8>, usize)> {
    let events = events_for(input, "optional-content coalescing")?;
    let mut stack = Vec::<Option<OptionalContentKey>>::new();
    let mut replacements = Vec::<(usize, usize, Vec<u8>)>::new();
    let mut boundaries_coalesced = 0usize;
    let mut index = 0usize;

    while index < events.len() {
        let event = &events[index];
        match event.operator.as_slice() {
            b"BDC" => {
                stack.push(optional_content_key(event));
                index += 1;
            }
            b"BMC" => {
                stack.push(None);
                index += 1;
            }
            b"EMC" => {
                let current = stack.last().and_then(Clone::clone);
                if let (Some(current), Some(next)) = (current, events.get(index + 1))
                    && adjacent(input, event, next)
                    && optional_content_key(next).as_ref() == Some(&current)
                {
                    replacements.push((event.start, next.end, b"\n".to_vec()));
                    boundaries_coalesced = boundaries_coalesced.saturating_add(1);
                    index += 2;
                    continue;
                }
                stack.pop();
                index += 1;
            }
            _ => index += 1,
        }
    }

    if replacements.is_empty() {
        return Some((input.to_vec(), 0));
    }
    Some((
        apply_replacements(input, replacements)?,
        boundaries_coalesced,
    ))
}

pub fn coalesce_optional_content_hayro(
    document: &mut EditDocument,
    flate_level: i32,
) -> Result<MarkedContentCoalesceStats> {
    let mut stats = MarkedContentCoalesceStats::default();
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
        if decoded.len() < MIN_PAGE_CONTENT_BYTES {
            continue;
        }
        let Some((coalesced, boundaries)) = coalesce_adjacent_ocg_content(&decoded) else {
            continue;
        };
        if boundaries == 0 || coalesced == decoded {
            continue;
        }
        let before_flate = compressed_len(&decoded, flate_level)?;
        let after_flate = compressed_len(&coalesced, flate_level)?;
        if after_flate > before_flate {
            continue;
        }

        replace_page_content(document, page, coalesced.clone())?;
        stats.pages_rewritten = stats.pages_rewritten.saturating_add(1);
        stats.boundaries_coalesced = stats.boundaries_coalesced.saturating_add(boundaries);
        stats.decoded_bytes_removed = stats
            .decoded_bytes_removed
            .saturating_add(decoded.len().saturating_sub(coalesced.len()));
        stats.estimated_flate_bytes_saved = stats
            .estimated_flate_bytes_saved
            .saturating_add(before_flate.saturating_sub(after_flate));
    }
    Ok(stats)
}

const STRUCTURAL_GRID_SCALE: f64 = 100.0;
const MAX_OUTLINED_GLYPH_EXTENT_GRID: i64 = 16 * 100;
const MAX_TYPE3_GLYPHS_PER_FONT: usize = 255;
const MIN_OUTLINED_GLYPH_OCCURRENCES: usize = 2;
const MIN_OUTLINED_GLYPH_ESTIMATED_SAVINGS: usize = 16 * 1024;

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

fn line_point(event: &Event) -> Option<LinePoint> {
    if !event.all_operands_numeric || event.numbers.len() != 2 {
        return None;
    }
    Some(LinePoint {
        start: event.start,
        end: event.end,
        x: structural_grid_coordinate(event.numbers[0])?,
        y: structural_grid_coordinate(event.numbers[1])?,
    })
}

fn flush_collinear_run(
    run: &mut Vec<LinePoint>,
    replacements: &mut Vec<(usize, usize, Vec<u8>)>,
) -> usize {
    if run.len() < 3 {
        run.clear();
        return 0;
    }
    let mut stack = Vec::<LinePoint>::with_capacity(run.len());
    let mut removed = 0usize;
    for point in run.drain(..) {
        stack.push(point);
        while stack.len() >= 3 {
            let len = stack.len();
            let a = stack[len - 3];
            let b = stack[len - 2];
            let c = stack[len - 1];
            if !same_forward_line(a, b, c) {
                break;
            }
            replacements.push((b.start, b.end, Vec::new()));
            stack.remove(len - 2);
            removed = removed.saturating_add(1);
        }
    }
    removed
}

fn compact_collinear_line_points(input: &[u8]) -> Option<(Vec<u8>, usize)> {
    let events = events_for(input, "collinear path compaction")?;
    let mut replacements = Vec::<(usize, usize, Vec<u8>)>::new();
    let mut run = Vec::<LinePoint>::new();
    let mut vertices_removed = 0usize;
    let mut previous_event = None::<&Event>;

    for event in &events {
        let contiguous = previous_event.is_none_or(|previous| adjacent(input, previous, event));
        match event.operator.as_slice() {
            b"m" => {
                vertices_removed = vertices_removed
                    .saturating_add(flush_collinear_run(&mut run, &mut replacements));
                if contiguous && let Some(point) = line_point(event) {
                    run.push(point);
                }
            }
            b"l" if !run.is_empty() && contiguous => {
                if let Some(point) = line_point(event) {
                    run.push(point);
                } else {
                    vertices_removed = vertices_removed
                        .saturating_add(flush_collinear_run(&mut run, &mut replacements));
                }
            }
            _ => {
                vertices_removed = vertices_removed
                    .saturating_add(flush_collinear_run(&mut run, &mut replacements));
            }
        }
        previous_event = Some(event);
    }
    vertices_removed =
        vertices_removed.saturating_add(flush_collinear_run(&mut run, &mut replacements));

    if replacements.is_empty() {
        return Some((input.to_vec(), 0));
    }
    Some((apply_replacements(input, replacements)?, vertices_removed))
}

pub fn compact_collinear_paths_hayro(
    document: &mut EditDocument,
    flate_level: i32,
) -> Result<CollinearPathStats> {
    let mut stats = CollinearPathStats::default();
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
        if decoded.len() < MIN_PAGE_CONTENT_BYTES {
            continue;
        }
        let Some((compacted, vertices_removed)) = compact_collinear_line_points(&decoded) else {
            continue;
        };
        if vertices_removed == 0 || compacted == decoded {
            continue;
        }
        let before_flate = compressed_len(&decoded, flate_level)?;
        let after_flate = compressed_len(&compacted, flate_level)?;
        if after_flate > before_flate {
            continue;
        }

        replace_page_content(document, page, compacted.clone())?;
        stats.pages_rewritten = stats.pages_rewritten.saturating_add(1);
        stats.vertices_removed = stats.vertices_removed.saturating_add(vertices_removed);
        stats.decoded_bytes_removed = stats
            .decoded_bytes_removed
            .saturating_add(decoded.len().saturating_sub(compacted.len()));
        stats.estimated_flate_bytes_saved = stats
            .estimated_flate_bytes_saved
            .saturating_add(before_flate.saturating_sub(after_flate));
    }
    Ok(stats)
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
    #[allow(clippy::cast_possible_truncation)]
    let coordinate = rounded as i64;
    Some(coordinate)
}

fn outlined_glyph_transaction(
    input: &[u8],
    events: &[Event],
    index: usize,
) -> Option<(GlyphTransaction, usize)> {
    let save = events.get(index)?;
    let miter = events.get(index + 1)?;
    if save.operator != b"q"
        || save.operand_count != 0
        || miter.operator != b"M"
        || miter.operand_count != 1
        || !adjacent(input, save, miter)
    {
        return None;
    }
    let miter_value = miter.sole_number?;
    if !miter_value.is_finite() {
        return None;
    }

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
        match event.operator.as_slice() {
            b"m" | b"l" if event.all_operands_numeric && event.numbers.len() == 2 => {
                let x = structural_grid_coordinate(event.numbers[0])?;
                let y = structural_grid_coordinate(event.numbers[1])?;
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
            b"h" if event.operand_count == 0 => {
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
    if paint.operator != b"b"
        || paint.operand_count != 0
        || restore.operator != b"Q"
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
            miter_bits: miter_value.to_bits(),
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
    let events = events_for(input, "outlined glyph factoring")?;
    if events
        .iter()
        .any(|event| matches!(event.operator.as_slice(), b"BT" | b"ET"))
    {
        return Some(Vec::new());
    }
    let mut components = Vec::new();
    let mut index = 0usize;
    while index < events.len() {
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

fn outlined_glyph_pdf_number(value: i64) -> String {
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
    output.extend_from_slice(outlined_glyph_pdf_number(x).as_bytes());
    output.push(b' ');
    output.extend_from_slice(outlined_glyph_pdf_number(y).as_bytes());
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
    output.extend_from_slice(outlined_glyph_pdf_number(component.origin_x).as_bytes());
    output.push(b' ');
    output.extend_from_slice(outlined_glyph_pdf_number(component.origin_y).as_bytes());
    output.extend_from_slice(b" Tm <");
    output.extend_from_slice(format!("{code:02X}").as_bytes());
    output.extend_from_slice(b"> Tj ET\n");
    Some(output)
}

fn outlined_glyph_page_resources(
    document: &EditDocument,
    page: ObjectHandle,
) -> Result<OwnedDictionary> {
    Ok(match document.inherited_page_value(page, b"Resources")? {
        Some(value) => resolved_dictionary(document, Some(&value))?.unwrap_or_default(),
        None => OwnedDictionary::default(),
    })
}

fn install_page_font(
    document: &mut EditDocument,
    page: ObjectHandle,
    name: Vec<u8>,
    target: ObjectHandle,
) -> Result<()> {
    let mut resources = outlined_glyph_page_resources(document, page)?;
    let mut fonts =
        resolved_dictionary(document, resources.get(b"Font".as_slice()))?.unwrap_or_default();
    fonts.insert(name, OwnedObject::Reference(target));
    resources.insert(b"Font".to_vec(), OwnedObject::Dictionary(fonts));
    let object = match page {
        ObjectHandle::Existing(id) => document.edit_object(id)?,
        ObjectHandle::New(id) => document.edit_added_object(id)?,
    };
    if let Some(dictionary) = object.as_dictionary_mut() {
        dictionary.insert(b"Resources".to_vec(), OwnedObject::Dictionary(resources));
    }
    Ok(())
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

    let resources = outlined_glyph_page_resources(document, page)?;
    let mut occupied = resolved_dictionary(document, resources.get(b"Font".as_slice()))?
        .map(|fonts| fonts.keys().cloned().collect::<BTreeSet<_>>())
        .unwrap_or_default();

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

fn outlined_glyph_estimated_after_flate(
    rewritten: &[u8],
    plans: &[GlyphFontPlan],
    flate_level: i32,
) -> Result<usize> {
    let mut total = compressed_len(rewritten, flate_level)?;
    let glyphs = plans.iter().map(|plan| plan.shapes.len()).sum::<usize>();
    for plan in plans {
        for shape in &plan.shapes {
            total = total.saturating_add(compressed_len(&shape.charproc, flate_level)?);
        }
    }
    Ok(total
        .saturating_add(glyphs.saturating_mul(128))
        .saturating_add(plans.len().saturating_mul(512)))
}

fn outlined_glyph_font_dictionary(
    document: &mut EditDocument,
    plan: &GlyphFontPlan,
) -> ObjectHandle {
    let mut charprocs = OwnedDictionary::new();
    let mut differences = vec![OwnedObject::Integer(1)];
    let mut widths = Vec::with_capacity(plan.shapes.len());
    let mut max_width = 0i64;
    let mut max_height = 0i64;

    for (index, shape) in plan.shapes.iter().enumerate() {
        let code = index.saturating_add(1);
        let glyph_name = format!("G{code}").into_bytes();
        let stream = ObjectHandle::New(document.add_object(OwnedObject::Stream {
            dictionary: OwnedDictionary::new(),
            data: StreamData::Owned(shape.charproc.clone()),
        }));
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

pub fn factor_outlined_glyphs_hayro(
    document: &mut EditDocument,
    flate_level: i32,
) -> Result<OutlinedGlyphFactorStats> {
    let mut stats = OutlinedGlyphFactorStats::default();
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
        let Some(rewritten) = apply_replacements(&decoded, replacement_plan.replacements) else {
            continue;
        };
        let before_flate = compressed_len(&decoded, flate_level)?;
        let after_flate = outlined_glyph_estimated_after_flate(&rewritten, &plans, flate_level)?;
        if before_flate <= after_flate
            || before_flate.saturating_sub(after_flate) < MIN_OUTLINED_GLYPH_ESTIMATED_SAVINGS
        {
            continue;
        }

        for plan in &plans {
            let font = outlined_glyph_font_dictionary(document, plan);
            install_page_font(document, page, plan.name.clone(), font)?;
        }
        replace_page_content(document, page, rewritten.clone())?;

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
            .saturating_add(decoded.len().saturating_sub(rewritten.len()));
        stats.estimated_flate_bytes_saved = stats
            .estimated_flate_bytes_saved
            .saturating_add(before_flate.saturating_sub(after_flate));
    }
    Ok(stats)
}

fn compressed_len(bytes: &[u8], flate_level: i32) -> Result<usize> {
    let level = u32::try_from(flate_level.clamp(0, 9)).unwrap_or(9);
    let mut encoder = ZlibEncoder::new(Vec::new(), Compression::new(level));
    encoder.write_all(bytes)?;
    Ok(encoder.finish()?.len())
}

pub fn batch_page_paints_hayro(
    document: &mut EditDocument,
    flate_level: i32,
) -> Result<PaintBatchStats> {
    let mut stats = PaintBatchStats::default();
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
        if decoded.len() < MIN_PAGE_CONTENT_BYTES {
            continue;
        }
        let Some((batched, page_stats)) = batch_content(&decoded) else {
            continue;
        };
        if page_stats.paints_eliminated < MIN_PAINTS_ELIMINATED || batched == decoded {
            continue;
        }
        let before_flate = compressed_len(&decoded, flate_level)?;
        let after_flate = compressed_len(&batched, flate_level)?;
        if after_flate > before_flate {
            continue;
        }

        replace_page_content(document, page, batched.clone())?;
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
        stats.decoded_bytes_removed = stats
            .decoded_bytes_removed
            .saturating_add(decoded.len().saturating_sub(batched.len()));
        stats.estimated_flate_bytes_saved = stats
            .estimated_flate_bytes_saved
            .saturating_add(before_flate.saturating_sub(after_flate));
    }
    Ok(stats)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn operator_count(output: &[u8], operator: &[u8]) -> usize {
        events_for(output, "paint batching operator count")
            .expect("parse output")
            .iter()
            .filter(|event| event.operator == operator)
            .count()
    }

    fn assert_known_operators(output: &[u8]) {
        let events = events_for(output, "paint batching test output").expect("parse output");
        assert!(
            events.iter().all(|event| {
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
    fn batches_consecutive_strokes_without_reordering() {
        let input = b"0 0 m 1 0 l S 2 0 m 3 0 l S 4 0 m 5 0 l S";
        let (output, stats) = batch_content(input).expect("parse");
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
        let (output, stats) = batch_content(input).expect("parse");
        assert_eq!(stats.paints_eliminated, 0);
        assert_eq!(output, input);
    }

    #[test]
    fn ext_gstate_disables_page_paint_batching() {
        let input = b"/GS1 gs 0 0 m 1 0 l S 2 0 m 3 0 l S";
        let (output, stats) = batch_content(input).expect("parse");
        assert_eq!(stats.paints_eliminated, 0);
        assert_eq!(output, input);
    }

    #[test]
    fn batches_same_winding_closed_fill_stroke_transactions() {
        let input = b"q 1 M 0 0 m 1 0 l 1 1 l h b Q q 1 M 0.5 0 m 1.5 0 l 1.5 1 l h b Q";
        let (output, stats) = batch_content(input).expect("parse");
        assert_eq!(stats.source_paints_batched, 2);
        assert_eq!(stats.paints_eliminated, 1);
        assert_eq!(operator_count(&output, b"B"), 1);
        assert_known_operators(&output);
    }

    #[test]
    fn opposite_winding_overlap_is_not_batched() {
        let input = b"q 1 M 0 0 m 1 0 l 1 1 l h b Q q 1 M 0.5 0 m 0.5 1 l 1.5 1 l h b Q";
        let (output, stats) = batch_content(input).expect("parse");
        assert_eq!(stats.paints_eliminated, 0);
        assert_eq!(output, input);
    }

    #[test]
    fn opposite_winding_disjoint_paths_are_batched() {
        let input = b"q 1 M 0 0 m 1 0 l 1 1 l h b Q q 1 M 2 0 m 2 1 l 3 1 l h b Q";
        let (output, stats) = batch_content(input).expect("parse");
        assert_eq!(stats.paints_eliminated, 1);
        assert_known_operators(&output);
    }

    #[test]
    fn zero_area_fill_can_join_any_winding() {
        let input = b"q 1 M 0 0 m 1 0 l h b Q q 1 M 0 0 m 1 0 l 1 1 l h b Q";
        let (output, stats) = batch_content(input).expect("parse");
        assert_eq!(stats.paints_eliminated, 1);
        assert_known_operators(&output);
    }

    #[test]
    fn differing_miter_limit_splits_closed_fill_batches() {
        let input = b"q 1 M 0 0 m 1 0 l 1 1 l h b Q q 2 M 2 0 m 3 0 l 3 1 l h b Q";
        let (output, stats) = batch_content(input).expect("parse");
        assert_eq!(stats.paints_eliminated, 0);
        assert_eq!(output, input);
    }

    #[test]
    fn batching_is_capped_to_bounded_compound_paths() {
        let mut input = Vec::new();
        for index in 0..=MAX_SOURCE_PATHS_PER_BATCH {
            input.extend_from_slice(format!("{index} 0 m {index} 1 l S ").as_bytes());
        }
        let (output, stats) = batch_content(&input).expect("parse");
        assert_eq!(stats.source_paints_batched, MAX_SOURCE_PATHS_PER_BATCH + 1);
        assert_eq!(stats.paints_eliminated, MAX_SOURCE_PATHS_PER_BATCH - 1);
        assert_eq!(operator_count(&output, b"S"), 2);
        assert_known_operators(&output);
    }
    #[test]
    fn outlined_glyph_components_match_after_translation() {
        let input = b"q 1 M 0 0 m 1 0 l 1 1 l h b Q q 1 M 1 0 m 2 0 l 2 1 l h b Q q 1 M 10 0 m 11 0 l 11 1 l h b Q q 1 M 11 0 m 12 0 l 12 1 l h b Q";
        let components = outlined_glyph_components(input).expect("parse");
        assert_eq!(components.len(), 2);
        assert_eq!(components[0].source_paints, 2);
        assert_eq!(components[1].source_paints, 2);
        assert_eq!(components[0].signature, components[1].signature);
    }

    #[test]
    fn outlined_glyph_factor_rejects_noncontiguous_components() {
        let input = b"q 1 M 0 0 m 1 0 l 1 1 l h b Q q 1 M 10 0 m 11 0 l 11 1 l h b Q q 1 M 1 0 m 2 0 l 2 1 l h b Q";
        let components = outlined_glyph_components(input).expect("parse");
        assert_eq!(components.len(), 1);
        assert_eq!(components[0].source_paints, 1);
        assert_eq!(components[0].origin_x, 1000);
    }

    #[test]
    fn outlined_glyph_factor_leaves_existing_text_pages_alone() {
        let input = b"BT ET q 1 M 0 0 m 1 0 l 1 1 l h b Q q 1 M 1 0 m 2 0 l 2 1 l h b Q";
        let components = outlined_glyph_components(input).expect("parse");
        assert!(components.is_empty());
    }
    #[test]
    fn coalesces_adjacent_identical_optional_content() {
        let input = b"/OC /MC0 BDC 0 0 m 1 0 l S EMC /OC /MC0 BDC 2 0 m 3 0 l S EMC";
        let (output, count) = coalesce_adjacent_ocg_content(input).expect("parse");
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
    fn keeps_different_optional_content_boundaries() {
        let input = b"/OC /MC0 BDC 0 0 m 1 0 l S EMC /OC /MC1 BDC 2 0 m 3 0 l S EMC";
        let (output, count) = coalesce_adjacent_ocg_content(input).expect("parse");
        assert_eq!(count, 0);
        assert_eq!(output, input);
    }

    #[test]
    fn keeps_non_oc_marked_content_boundaries() {
        let input = b"/Span /P0 BDC 0 0 m 1 0 l S EMC /Span /P0 BDC 2 0 m 3 0 l S EMC";
        let (output, count) = coalesce_adjacent_ocg_content(input).expect("parse");
        assert_eq!(count, 0);
        assert_eq!(output, input);
    }
    #[test]
    fn removes_exact_forward_collinear_vertex() {
        let input = b"0 0 m 1 0 l 2 0 l S";
        let (output, removed) = compact_collinear_line_points(input).expect("parse");
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
            let (output, removed) = compact_collinear_line_points(input).expect("parse");
            assert_eq!(removed, 0);
            assert_eq!(output, input);
        }
    }

    #[test]
    fn keeps_off_grid_collinear_vertices() {
        let input = b"0 0 m 0.001 0 l 0.002 0 l S";
        let (output, removed) = compact_collinear_line_points(input).expect("parse");
        assert_eq!(removed, 0);
        assert_eq!(output, input);
    }
    #[test]
    fn paint_batching_is_idempotent() {
        let strokes = b"0 0 m 1 0 l S 2 0 m 3 0 l S 4 0 m 5 0 l S";
        let (first, first_stats) = batch_content(strokes).expect("parse");
        assert!(first_stats.paints_eliminated > 0);
        let (second, second_stats) = batch_content(&first).expect("parse generated output");
        assert_eq!(second_stats.paints_eliminated, 0);
        assert_eq!(second, first);

        let fills = b"q 1 M 0 0 m 1 0 l 1 1 l h b Q q 1 M 2 0 m 3 0 l 3 1 l h b Q";
        let (first, first_stats) = batch_content(fills).expect("parse");
        assert!(first_stats.paints_eliminated > 0);
        let (second, second_stats) = batch_content(&first).expect("parse generated output");
        assert_eq!(second_stats.paints_eliminated, 0);
        assert_eq!(second, first);
    }
}
