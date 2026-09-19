use crate::{
    EditDocument, ObjectHandle as CowObjectHandle, OwnedObject, Result,
    content::{page_content, replace_page_content, resolved_dictionary},
};
use flpdf::content_stream::ContentScalar;
use flpdf::{Matrix, ObjectHandle, ObjectHandleParserCallbacks, ParseControl};
use smallvec::SmallVec;
use std::borrow::Cow;
use std::collections::{BTreeMap, BTreeSet};

const POSITION_TOLERANCE_PT: f64 = 12.0;
const MIN_SUPPORT_PAGES: usize = 3;

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct RepeatedPageObjectStats {
    pub groups_removed: usize,
    pub all_pages_groups_removed: usize,
    pub after_first_groups_removed: usize,
    pub objects_removed: usize,
    pub text_objects_removed: usize,
    pub xobject_paints_removed: usize,
    pub pages_rewritten: usize,
    pub rewritten_pages: BTreeSet<CowObjectHandle>,
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
enum CandidateKey {
    Text(Vec<u8>),
    XObject {
        handle: CowObjectHandle,
        linear: [i32; 4],
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CandidateKind {
    Text,
    XObject,
}

#[derive(Debug, Clone)]
struct Candidate {
    page_index: usize,
    start: usize,
    end: usize,
    x: f64,
    y: f64,
    key: CandidateKey,
    kind: CandidateKind,
}

#[derive(Debug, Clone)]
enum OperandValue {
    Scalar(ContentScalar),
    Handle(ObjectHandle),
}

impl OperandValue {
    fn number(&self) -> Option<f64> {
        match self {
            Self::Scalar(value) => value
                .as_integer()
                .map(|value| value as f64)
                .or_else(|| value.as_real()),
            Self::Handle(value) => value
                .as_integer()
                .map(|value| value as f64)
                .or_else(|| value.as_real()),
        }
    }

    fn as_name(&self) -> Option<Cow<'_, [u8]>> {
        match self {
            Self::Scalar(value) => value.as_name().map(Cow::Borrowed),
            Self::Handle(value) => value.as_name().map(Cow::Owned),
        }
    }

    fn as_string(&self) -> Option<Vec<u8>> {
        match self {
            Self::Scalar(value) => value.as_string().map(ToOwned::to_owned),
            Self::Handle(value) => value.as_string(),
        }
    }

    fn as_array(&self) -> Option<Vec<ObjectHandle>> {
        match self {
            Self::Scalar(_) => None,
            Self::Handle(value) => value.as_array(),
        }
    }
}

#[derive(Debug, Clone)]
struct OperandSpan {
    object: OperandValue,
    offset: usize,
}

#[derive(Debug, Clone)]
struct TextObjectState {
    start: usize,
    payload: Vec<u8>,
    anchor: Option<(f64, f64)>,
    linear: Option<[i32; 4]>,
    saw_text: bool,
}

struct PageScanner<'a> {
    page_index: usize,
    xobjects: &'a BTreeMap<Vec<u8>, CowObjectHandle>,
    graphics_ctm: Matrix,
    graphics_stack: Vec<Matrix>,
    text_matrix: Matrix,
    line_matrix: Matrix,
    font_name: Vec<u8>,
    font_size: f64,
    horizontal_scale: f64,
    operands: Vec<OperandSpan>,
    text_object: Option<TextObjectState>,
    candidates: Vec<Candidate>,
}

impl<'a> PageScanner<'a> {
    fn new(page_index: usize, xobjects: &'a BTreeMap<Vec<u8>, CowObjectHandle>) -> Self {
        Self {
            page_index,
            xobjects,
            graphics_ctm: Matrix::default(),
            graphics_stack: Vec::new(),
            text_matrix: Matrix::default(),
            line_matrix: Matrix::default(),
            font_name: Vec::new(),
            font_size: 0.0,
            horizontal_scale: 100.0,
            operands: Vec::new(),
            text_object: None,
            candidates: Vec::new(),
        }
    }

    fn numbers(&self) -> Option<SmallVec<[f64; 6]>> {
        let mut values = SmallVec::new();
        for operand in &self.operands {
            values.push(operand.object.number()?);
        }
        Some(values)
    }

    fn quantized(value: f64, scale: f64) -> i32 {
        if !value.is_finite() {
            return 0;
        }
        (value * scale)
            .round()
            .clamp(f64::from(i32::MIN), f64::from(i32::MAX)) as i32
    }

    fn linear(matrix: Matrix) -> [i32; 4] {
        [
            Self::quantized(matrix.a, 1000.0),
            Self::quantized(matrix.b, 1000.0),
            Self::quantized(matrix.c, 1000.0),
            Self::quantized(matrix.d, 1000.0),
        ]
    }

    fn append_string_payload(target: &mut Vec<u8>, bytes: &[u8]) {
        target.push(b'S');
        let len = u32::try_from(bytes.len()).unwrap_or(u32::MAX);
        target.extend_from_slice(&len.to_be_bytes());
        target.extend_from_slice(bytes);
    }

    fn prepare_text_show(&mut self) {
        let mut combined = self.graphics_ctm;
        combined.concat(self.text_matrix);
        if let Some(object) = self.text_object.as_mut() {
            if object.anchor.is_none() {
                object.anchor = Some((combined.e, combined.f));
                object.linear = Some(Self::linear(combined));
            }
            object.payload.push(b'F');
            let len = u16::try_from(self.font_name.len()).unwrap_or(u16::MAX);
            object.payload.extend_from_slice(&len.to_be_bytes());
            object.payload.extend_from_slice(&self.font_name);
            object
                .payload
                .extend_from_slice(&Self::quantized(self.font_size, 100.0).to_be_bytes());
            object
                .payload
                .extend_from_slice(&Self::quantized(self.horizontal_scale, 100.0).to_be_bytes());
        }
    }

    fn append_text_show(&mut self, operator: &[u8]) {
        let mut strings = Vec::new();
        match operator {
            b"Tj" | b"'" => {
                if let Some(bytes) = self
                    .operands
                    .first()
                    .and_then(|operand| operand.object.as_string())
                    && !bytes.is_empty()
                {
                    strings.push(bytes);
                }
            }
            b"\"" => {
                if let Some(bytes) = self
                    .operands
                    .get(2)
                    .and_then(|operand| operand.object.as_string())
                    && !bytes.is_empty()
                {
                    strings.push(bytes);
                }
            }
            b"TJ" => {
                if let Some(array) = self
                    .operands
                    .first()
                    .and_then(|operand| operand.object.as_array())
                {
                    for item in array {
                        if let Some(bytes) = item.as_string()
                            && !bytes.is_empty()
                        {
                            strings.push(bytes);
                        }
                    }
                }
            }
            _ => {}
        }
        if strings.is_empty() {
            return;
        }
        self.prepare_text_show();
        if let Some(object) = self.text_object.as_mut() {
            object.saw_text = true;
            for bytes in strings {
                Self::append_string_payload(&mut object.payload, &bytes);
            }
        }
    }

    fn finish_text_object(&mut self, end: usize) {
        let Some(object) = self.text_object.take() else {
            return;
        };
        let (Some((x, y)), Some(linear)) = (object.anchor, object.linear) else {
            return;
        };
        if !object.saw_text || object.payload.is_empty() || !x.is_finite() || !y.is_finite() {
            return;
        }
        let mut payload = object.payload;
        payload.push(b'M');
        for component in linear {
            payload.extend_from_slice(&component.to_be_bytes());
        }
        self.candidates.push(Candidate {
            page_index: self.page_index,
            start: object.start,
            end,
            x,
            y,
            key: CandidateKey::Text(payload),
            kind: CandidateKind::Text,
        });
    }

    fn apply_operator(&mut self, operator: &[u8], span_start: usize, span_end: usize) {
        match operator {
            b"q" => self.graphics_stack.push(self.graphics_ctm),
            b"Q" => {
                if let Some(matrix) = self.graphics_stack.pop() {
                    self.graphics_ctm = matrix;
                }
            }
            b"cm" => {
                if let Some(values) = self.numbers().filter(|values| values.len() >= 6) {
                    self.graphics_ctm.concat(Matrix::new(
                        values[0], values[1], values[2], values[3], values[4], values[5],
                    ));
                }
            }
            b"BT" => {
                self.text_matrix = Matrix::default();
                self.line_matrix = Matrix::default();
                self.text_object = Some(TextObjectState {
                    start: span_start,
                    payload: Vec::new(),
                    anchor: None,
                    linear: None,
                    saw_text: false,
                });
            }
            b"ET" => self.finish_text_object(span_end),
            b"Tf" => {
                if self.operands.len() >= 2 {
                    if let Some(name) = self.operands[0].object.as_name() {
                        self.font_name = name.into_owned();
                    }
                    if let Some(size) = self.operands[1].object.number() {
                        self.font_size = size;
                    }
                }
            }
            b"Tz" => {
                if let Some(value) = self.numbers().and_then(|values| values.first().copied()) {
                    self.horizontal_scale = value;
                }
            }
            b"Tm" => {
                if let Some(values) = self.numbers().filter(|values| values.len() >= 6) {
                    let matrix = Matrix::new(
                        values[0], values[1], values[2], values[3], values[4], values[5],
                    );
                    self.text_matrix = matrix;
                    self.line_matrix = matrix;
                }
            }
            b"Td" | b"TD" => {
                if let Some(values) = self.numbers().filter(|values| values.len() >= 2) {
                    self.line_matrix.translate(values[0], values[1]);
                    self.text_matrix = self.line_matrix;
                }
            }
            b"T*" => self.text_matrix = self.line_matrix,
            b"Tj" | b"TJ" | b"'" | b"\"" => self.append_text_show(operator),
            b"Do" => {
                let Some(name) = self
                    .operands
                    .first()
                    .and_then(|operand| operand.object.as_name())
                else {
                    return;
                };
                let Some(handle) = self.xobjects.get(name.as_ref()).copied() else {
                    return;
                };
                self.candidates.push(Candidate {
                    page_index: self.page_index,
                    start: span_start,
                    end: span_end,
                    x: self.graphics_ctm.e,
                    y: self.graphics_ctm.f,
                    key: CandidateKey::XObject {
                        handle,
                        linear: Self::linear(self.graphics_ctm),
                    },
                    kind: CandidateKind::XObject,
                });
            }
            _ => {}
        }
    }
}

impl ObjectHandleParserCallbacks for PageScanner<'_> {
    const HANDLES_CONTENT_SCALARS: bool = true;

    fn handle_scalar(
        &mut self,
        scalar: ContentScalar,
        offset: usize,
        length: usize,
    ) -> flpdf::Result<ParseControl> {
        if let Some(operator) = scalar.as_operator() {
            let span_start = self
                .operands
                .first()
                .map_or(offset, |operand| operand.offset);
            self.apply_operator(operator, span_start, offset.saturating_add(length));
            self.operands.clear();
        } else {
            self.operands.push(OperandSpan {
                object: OperandValue::Scalar(scalar),
                offset,
            });
        }
        Ok(ParseControl::Continue)
    }

    fn handle_object(
        &mut self,
        object: ObjectHandle,
        offset: usize,
        length: usize,
    ) -> flpdf::Result<ParseControl> {
        if let Some(operator) = object.as_operator() {
            let span_start = self
                .operands
                .first()
                .map_or(offset, |operand| operand.offset);
            self.apply_operator(&operator, span_start, offset.saturating_add(length));
            self.operands.clear();
        } else if object.as_inline_image().is_none() {
            self.operands.push(OperandSpan {
                object: OperandValue::Handle(object),
                offset,
            });
        }
        Ok(ParseControl::Continue)
    }

    fn handle_eof(&mut self) -> flpdf::Result<()> {
        Ok(())
    }
}

fn page_xobjects(
    document: &EditDocument,
    page: CowObjectHandle,
) -> Result<BTreeMap<Vec<u8>, CowObjectHandle>> {
    let Some(resources_value) = document.inherited_page_value(page, b"Resources")? else {
        return Ok(BTreeMap::new());
    };
    let Some(OwnedObject::Dictionary(resources)) =
        document.resolve_owned_value(&resources_value)?
    else {
        return Ok(BTreeMap::new());
    };
    let Some(xobjects) = resolved_dictionary(document, resources.get(b"XObject".as_slice()))?
    else {
        return Ok(BTreeMap::new());
    };
    Ok(xobjects
        .into_iter()
        .filter_map(|(name, value)| match value {
            OwnedObject::Reference(handle) => Some((name, handle)),
            _ => None,
        })
        .collect())
}

fn close_position(a: &Candidate, b: &Candidate) -> bool {
    (a.x - b.x).abs() <= POSITION_TOLERANCE_PT && (a.y - b.y).abs() <= POSITION_TOLERANCE_PT
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CoverageMode {
    AllPages,
    AfterFirst,
}

fn select_mode_clusters(
    candidates: &[Candidate],
    indices: &[usize],
    page_count: usize,
    start_page: usize,
    unavailable: &BTreeSet<usize>,
) -> Vec<Vec<usize>> {
    let support = page_count.saturating_sub(start_page);
    if support < MIN_SUPPORT_PAGES {
        return Vec::new();
    }
    let mut by_page = vec![Vec::new(); page_count];
    for &index in indices {
        if !unavailable.contains(&index) {
            by_page[candidates[index].page_index].push(index);
        }
    }
    if (start_page..page_count).any(|page| by_page[page].is_empty()) {
        return Vec::new();
    }
    let mut used = BTreeSet::new();
    let mut clusters = Vec::new();
    for &seed_index in &by_page[start_page] {
        if used.contains(&seed_index) {
            continue;
        }
        let seed = &candidates[seed_index];
        let mut cluster = Vec::with_capacity(support);
        for page_candidates in by_page.iter().take(page_count).skip(start_page) {
            let choice = page_candidates
                .iter()
                .copied()
                .filter(|index| !used.contains(index) && close_position(seed, &candidates[*index]))
                .min_by(|left, right| {
                    let l =
                        (candidates[*left].x - seed.x).abs() + (candidates[*left].y - seed.y).abs();
                    let r = (candidates[*right].x - seed.x).abs()
                        + (candidates[*right].y - seed.y).abs();
                    l.total_cmp(&r)
                });
            let Some(choice) = choice else {
                cluster.clear();
                break;
            };
            cluster.push(choice);
        }
        if cluster.len() == support {
            used.extend(cluster.iter().copied());
            clusters.push(cluster);
        }
    }
    clusters
}

fn select_persistent_candidates(
    candidates: &[Candidate],
    page_count: usize,
) -> (BTreeSet<usize>, Vec<CoverageMode>) {
    let mut groups: BTreeMap<&CandidateKey, Vec<usize>> = BTreeMap::new();
    for (index, candidate) in candidates.iter().enumerate() {
        groups.entry(&candidate.key).or_default().push(index);
    }
    let mut selected = BTreeSet::new();
    let mut modes = Vec::new();
    for indices in groups.values() {
        for cluster in select_mode_clusters(candidates, indices, page_count, 0, &selected) {
            selected.extend(cluster);
            modes.push(CoverageMode::AllPages);
        }
        if page_count > 1 {
            for cluster in select_mode_clusters(candidates, indices, page_count, 1, &selected) {
                selected.extend(cluster);
                modes.push(CoverageMode::AfterFirst);
            }
        }
    }
    (selected, modes)
}

fn remove_ranges(input: &[u8], ranges: &[(usize, usize)]) -> Vec<u8> {
    let mut output = Vec::with_capacity(input.len());
    let mut cursor = 0usize;
    for &(start, end) in ranges {
        if start < cursor || start > end || end > input.len() {
            continue;
        }
        output.extend_from_slice(&input[cursor..start]);
        cursor = end;
    }
    output.extend_from_slice(&input[cursor..]);
    output
}

pub(crate) fn repeated_page_objects_prefix_possible_hayro(document: &EditDocument) -> Result<bool> {
    let pages = document.page_handles()?;
    if pages.len() < MIN_SUPPORT_PAGES {
        return Ok(false);
    }
    let seed_pages = pages.len().min(MIN_SUPPORT_PAGES.saturating_add(1));
    let mut candidates = Vec::new();
    for (page_index, &page) in pages.iter().take(seed_pages).enumerate() {
        let content = page_content(document, page)?;
        let xobjects = page_xobjects(document, page)?;
        let mut scanner = PageScanner::new(page_index, &xobjects);
        if flpdf::parse_detached_content_stream(
            &content,
            "repeated page object cache preflight",
            &mut scanner,
        )
        .is_ok()
        {
            candidates.extend(scanner.candidates);
        }
    }
    Ok(!select_persistent_candidates(&candidates, seed_pages)
        .0
        .is_empty())
}

pub(crate) fn remove_repeated_page_objects_hayro(
    document: &mut EditDocument,
) -> Result<RepeatedPageObjectStats> {
    let pages = document.page_handles()?;
    if pages.len() < MIN_SUPPORT_PAGES {
        return Ok(RepeatedPageObjectStats::default());
    }
    let mut contents = Vec::with_capacity(pages.len());
    let mut candidates = Vec::new();
    let seed_pages = pages.len().min(MIN_SUPPORT_PAGES.saturating_add(1));
    for (page_index, &page) in pages.iter().take(seed_pages).enumerate() {
        let content = page_content(document, page)?;
        let xobjects = page_xobjects(document, page)?;
        let mut scanner = PageScanner::new(page_index, &xobjects);
        if flpdf::parse_detached_content_stream(
            &content,
            "repeated page object removal seed",
            &mut scanner,
        )
        .is_ok()
        {
            candidates.extend(scanner.candidates);
        }
        contents.push(content);
    }
    if seed_pages < pages.len()
        && select_persistent_candidates(&candidates, seed_pages)
            .0
            .is_empty()
    {
        return Ok(RepeatedPageObjectStats::default());
    }
    for (page_index, &page) in pages.iter().enumerate().skip(seed_pages) {
        let content = page_content(document, page)?;
        let xobjects = page_xobjects(document, page)?;
        let mut scanner = PageScanner::new(page_index, &xobjects);
        if flpdf::parse_detached_content_stream(
            &content,
            "repeated page object removal",
            &mut scanner,
        )
        .is_ok()
        {
            candidates.extend(scanner.candidates);
        }
        contents.push(content);
    }

    let (selected, modes) = select_persistent_candidates(&candidates, pages.len());
    if selected.is_empty() {
        return Ok(RepeatedPageObjectStats::default());
    }
    let mut by_page: BTreeMap<usize, Vec<(usize, usize)>> = BTreeMap::new();
    let mut stats = RepeatedPageObjectStats {
        groups_removed: modes.len(),
        all_pages_groups_removed: modes
            .iter()
            .filter(|mode| matches!(mode, CoverageMode::AllPages))
            .count(),
        after_first_groups_removed: modes
            .iter()
            .filter(|mode| matches!(mode, CoverageMode::AfterFirst))
            .count(),
        ..RepeatedPageObjectStats::default()
    };
    for index in selected {
        let candidate = &candidates[index];
        by_page
            .entry(candidate.page_index)
            .or_default()
            .push((candidate.start, candidate.end));
        stats.objects_removed = stats.objects_removed.saturating_add(1);
        match candidate.kind {
            CandidateKind::Text => {
                stats.text_objects_removed = stats.text_objects_removed.saturating_add(1);
            }
            CandidateKind::XObject => {
                stats.xobject_paints_removed = stats.xobject_paints_removed.saturating_add(1);
            }
        }
    }
    for (page_index, mut ranges) in by_page {
        ranges.sort_unstable();
        replace_page_content(
            document,
            pages[page_index],
            remove_ranges(&contents[page_index], &ranges),
        )?;
        stats.pages_rewritten = stats.pages_rewritten.saturating_add(1);
        stats.rewritten_pages.insert(pages[page_index]);
    }
    Ok(stats)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn text_candidate(page: usize, x: f64, y: f64, key: &[u8]) -> Candidate {
        Candidate {
            page_index: page,
            start: page * 10,
            end: page * 10 + 5,
            x,
            y,
            key: CandidateKey::Text(key.to_vec()),
            kind: CandidateKind::Text,
        }
    }

    #[test]
    fn selects_same_object_on_every_page_with_fuzzy_position() {
        let candidates = vec![
            text_candidate(0, 100.0, 700.0, b"notice"),
            text_candidate(1, 104.0, 697.0, b"notice"),
            text_candidate(2, 96.0, 705.0, b"notice"),
        ];
        let (selected, modes) = select_persistent_candidates(&candidates, 3);
        assert_eq!(selected.len(), 3);
        assert_eq!(modes, vec![CoverageMode::AllPages]);
    }

    #[test]
    fn selects_object_on_every_page_after_first() {
        let candidates = vec![
            text_candidate(1, 100.0, 700.0, b"acquired"),
            text_candidate(2, 100.0, 700.0, b"acquired"),
            text_candidate(3, 100.0, 700.0, b"acquired"),
        ];
        let (selected, modes) = select_persistent_candidates(&candidates, 4);
        assert_eq!(selected.len(), 3);
        assert_eq!(modes, vec![CoverageMode::AfterFirst]);
    }

    #[test]
    fn four_page_seed_rejects_group_that_cannot_cover_required_prefix() {
        let candidates = vec![
            text_candidate(0, 100.0, 700.0, b"notice"),
            text_candidate(1, 100.0, 700.0, b"different"),
            text_candidate(2, 100.0, 700.0, b"notice"),
            text_candidate(3, 100.0, 700.0, b"notice"),
        ];
        assert!(select_persistent_candidates(&candidates, 4).0.is_empty());
    }

    #[test]
    fn scanner_records_xobject_identity_and_fuzzy_position_anchor() {
        let handle = CowObjectHandle::Existing(crate::ObjectId::new(42, 0));
        let xobjects = BTreeMap::from([(b"Im0".to_vec(), handle)]);
        let mut scanner = PageScanner::new(2, &xobjects);
        let content = b"q 2 0 0 3 101 699 cm /Im0 Do Q";
        assert!(
            flpdf::parse_detached_content_stream(content, "repeated xobject test", &mut scanner)
                .is_ok()
        );
        assert_eq!(scanner.candidates.len(), 1);
        let candidate = &scanner.candidates[0];
        assert_eq!(candidate.page_index, 2);
        assert_eq!(candidate.x, 101.0);
        assert_eq!(candidate.y, 699.0);
        assert_eq!(
            candidate.key,
            CandidateKey::XObject {
                handle,
                linear: [2000, 0, 0, 3000],
            }
        );
    }

    #[test]
    fn does_not_select_changing_or_distant_objects() {
        let candidates = vec![
            text_candidate(0, 100.0, 700.0, b"same"),
            text_candidate(1, 100.0, 700.0, b"different"),
            text_candidate(2, 140.0, 700.0, b"same"),
        ];
        let (selected, modes) = select_persistent_candidates(&candidates, 3);
        assert!(selected.is_empty());
        assert!(modes.is_empty());
    }
}
