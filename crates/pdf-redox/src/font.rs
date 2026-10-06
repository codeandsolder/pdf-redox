use crate::stream_codec::{
    decode_stream, encode_flate, is_unfiltered_or_lone_flate, set_plain_flate,
};
use crate::{
    EditDocument, Error, ObjectHandle as CowObjectHandle, OwnedDictionary, OwnedObject, Result,
    StreamData, content::decoded_content_value, source::CurrentObject,
};
use hayro_syntax::object::{
    Dict as HayroDict, MaybeRef as HayroMaybeRef, Name as HayroName, Object as HayroObject,
};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet, HashMap, VecDeque};

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
struct FontProgramUsage {
    simple_truetype: bool,
    cidfont_type2: bool,
    cidfont_type0: bool,
}

impl FontProgramUsage {
    const fn cidfont_type2_only(self) -> bool {
        self.cidfont_type2 && !self.simple_truetype && !self.cidfont_type0
    }

    const fn cidfont_type0_only(self) -> bool {
        self.cidfont_type0 && !self.simple_truetype && !self.cidfont_type2
    }

    const fn simple_truetype_only(self) -> bool {
        self.simple_truetype && !self.cidfont_type2 && !self.cidfont_type0
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct FontOptimizationStats {
    pub programs_optimized: usize,
    pub programs_glyph_subset: usize,
    pub programs_dense_remapped: usize,
    pub cid_to_gid_maps_rewritten: usize,
    pub dense_glyph_slots_removed: usize,
    pub dense_decoded_bytes_removed: usize,
    pub original_encoded_bytes: usize,
    pub optimized_encoded_bytes: usize,
    pub decoded_table_bytes_removed: usize,
    pub glyph_subset_decoded_bytes_removed: usize,
}

fn single_font_ttc_to_sfnt(bytes: &[u8]) -> Option<Vec<u8>> {
    crate::font_subset::unwrap_single_face_collection(bytes)
}

fn sfnt_for_pdf_rendering(bytes: &[u8], usage: FontProgramUsage) -> Option<(Vec<u8>, usize)> {
    crate::font_subset::strip_pdf_unused_tables(bytes, usage.cidfont_type2_only())
}

fn sfnt_unicode_gid(bytes: &[u8], codepoint: u32) -> Option<u16> {
    use skrifa::MetadataProvider;

    let font = skrifa::FontRef::new(bytes).ok()?;
    let charmap = font.charmap();
    // PDF WinAnsi text is ordinary Unicode-addressed text. Preserve the old
    // conservative behavior for symbol/MacRoman-only fonts rather than using
    // their compatibility remappings.
    if charmap.is_symbol() {
        return None;
    }
    u16::try_from(charmap.map(codepoint)?.to_u32()).ok()
}

fn sfnt_winansi_ascii_glyph_ids(bytes: &[u8], codes: &BTreeSet<u8>) -> Option<BTreeSet<u16>> {
    let mut gids = BTreeSet::new();
    for code in codes {
        if !(0x20..=0x7e).contains(code) {
            return None;
        }
        let gid = sfnt_unicode_gid(bytes, u32::from(*code))?;
        if gid == 0 {
            return None;
        }
        gids.insert(gid);
    }
    Some(gids)
}

fn composite_components(glyph: &[u8]) -> Option<Vec<u16>> {
    use write_fonts::read::{FontData, FontRead, tables::glyf::Glyph};

    let glyph = Glyph::read(FontData::new(glyph)).ok()?;
    match glyph {
        Glyph::Simple(_) => Some(Vec::new()),
        Glyph::Composite(composite) => Some(
            composite
                .component_glyphs_and_flags()
                .map(|(gid, _)| gid.to_u16())
                .collect(),
        ),
    }
}

fn subset_base_font_name(name: &[u8]) -> Vec<u8> {
    if name.len() > 7 && name[6] == b'+' && name[..6].iter().all(u8::is_ascii_uppercase) {
        name[7..].to_vec()
    } else {
        name.to_vec()
    }
}

fn descriptor_base_font_name(
    document: &EditDocument,
    descriptor: CowObjectHandle,
) -> Result<Option<Vec<u8>>> {
    let Some(object) = document.current_owned_object(descriptor)? else {
        return Ok(None);
    };
    let Some(dictionary) = object.as_dictionary() else {
        return Ok(None);
    };
    Ok(
        owned_name_value(document, dictionary.get(b"FontName".as_slice()))?
            .map(|name| subset_base_font_name(&name)),
    )
}

fn sparse_cid_union_skeleton_hash(bytes: &[u8]) -> Option<[u8; 32]> {
    use write_fonts::read::{FontRef, TableProvider};

    let font = FontRef::new(bytes).ok()?;
    if font.table_directory().sfnt_version() != 0x0001_0000 {
        return None;
    }
    let mut tables = Vec::new();
    let mut required = [false; 6];
    for record in font.table_directory().table_records() {
        let tag = record.tag();
        let mut data = font.data_for_tag(tag)?.as_bytes().to_vec();
        match &tag.to_be_bytes() {
            b"glyf" => {
                required[0] = true;
                continue;
            }
            b"loca" => {
                required[1] = true;
                continue;
            }
            b"head" => {
                required[2] = true;
                if data.len() < 54 {
                    return None;
                }
                data[8..12].fill(0);
            }
            b"maxp" => required[3] = true,
            b"hhea" => required[4] = true,
            b"hmtx" => required[5] = true,
            _ => {}
        }
        tables.push((tag, data));
    }
    if required.iter().any(|present| !present) {
        return None;
    }
    tables.sort_unstable_by_key(|(tag, _)| tag.to_be_bytes());
    let mut hasher = Sha256::new();
    hasher.update(font.table_directory().sfnt_version().to_be_bytes());
    hasher.update((tables.len() as u64).to_le_bytes());
    for (tag, data) in tables {
        hasher.update(tag.to_be_bytes());
        hasher.update((data.len() as u64).to_le_bytes());
        hasher.update(data);
    }
    Some(hasher.finalize().into())
}

fn sfnt_glyph_offsets(bytes: &[u8]) -> Option<Vec<usize>> {
    use write_fonts::read::{FontRef, TableProvider};

    let font = FontRef::new(bytes).ok()?;
    let maxp = font.maxp().ok()?;
    let glyph_count = usize::from(maxp.num_glyphs());
    let loca = font.loca(None).ok()?;
    let glyf = font.glyf().ok()?;
    if loca.len() != glyph_count || !loca.all_offsets_are_ascending() {
        return None;
    }
    let glyf_len = glyf.offset_data().as_bytes().len();
    let mut offsets = Vec::with_capacity(glyph_count + 1);
    for index in 0..=glyph_count {
        offsets.push(usize::try_from(loca.get_raw(index)?).ok()?);
    }
    (offsets.last().copied()? <= glyf_len).then_some(offsets)
}

fn sfnt_union_sparse_glyphs(fonts: &[(&[u8], &BTreeSet<u16>)]) -> Option<Vec<u8>> {
    use write_fonts::read::{FontRef, TableProvider};

    if fonts.len() < 2 {
        return None;
    }
    let base = fonts.first()?.0;
    let base_hash = sparse_cid_union_skeleton_hash(base)?;
    let base_font = FontRef::new(base).ok()?;
    let glyph_count = usize::from(base_font.maxp().ok()?.num_glyphs());
    let loca_format = base_font.head().ok()?.index_to_loc_format();
    let alignment = match loca_format {
        0 => 2usize,
        1 => 4usize,
        _ => return None,
    };

    let mut union_glyphs = vec![None::<Vec<u8>>; glyph_count];
    for (font, _) in fonts {
        if sparse_cid_union_skeleton_hash(font)? != base_hash {
            return None;
        }
        let offsets = sfnt_glyph_offsets(font)?;
        if offsets.len() != glyph_count + 1 {
            return None;
        }
        let source_font = FontRef::new(font).ok()?;
        let glyf = source_font.glyf().ok()?.offset_data().as_bytes();
        for gid in 0..glyph_count {
            let glyph = glyf.get(offsets[gid]..offsets[gid + 1])?;
            if glyph.is_empty() {
                continue;
            }
            match &union_glyphs[gid] {
                Some(existing) if existing.as_slice() != glyph => return None,
                Some(_) => {}
                None => union_glyphs[gid] = Some(glyph.to_vec()),
            }
        }
    }

    // Stronger than merely proving non-conflicting sparse programs: every GID
    // that this document actually addresses through each source program, plus
    // every recursively referenced composite component, must resolve to exactly
    // the same outline before and after unioning. GID 0 is included because a
    // renderer may use .notdef for an invalid/missing character without that
    // GID appearing literally in the content stream.
    for (font, used_gids) in fonts {
        let offsets = sfnt_glyph_offsets(font)?;
        let source_font = FontRef::new(font).ok()?;
        let glyf = source_font.glyf().ok()?.offset_data().as_bytes();
        let mut pending = VecDeque::new();
        pending.push_back(0usize);
        pending.extend(used_gids.iter().map(|gid| usize::from(*gid)));
        let mut checked = BTreeSet::new();
        while let Some(gid) = pending.pop_front() {
            if gid >= glyph_count || !checked.insert(gid) {
                if gid >= glyph_count {
                    return None;
                }
                continue;
            }
            let source_glyph = glyf.get(offsets[gid]..offsets[gid + 1])?;
            let union_glyph = union_glyphs[gid].as_deref().unwrap_or_default();
            if source_glyph != union_glyph {
                return None;
            }
            for component in composite_components(source_glyph)? {
                pending.push_back(usize::from(component));
            }
        }
    }

    let mut rebuilt_glyf = Vec::new();
    let mut rebuilt_offsets = Vec::with_capacity(glyph_count + 1);
    for glyph in &union_glyphs {
        rebuilt_offsets.push(rebuilt_glyf.len());
        let Some(glyph) = glyph else {
            continue;
        };
        rebuilt_glyf.extend_from_slice(glyph);
        while rebuilt_glyf.len() % alignment != 0 {
            rebuilt_glyf.push(0);
        }
    }
    rebuilt_offsets.push(rebuilt_glyf.len());

    let mut rebuilt_loca = Vec::new();
    match loca_format {
        0 => {
            for offset in &rebuilt_offsets {
                if offset % 2 != 0 || offset / 2 > usize::from(u16::MAX) {
                    return None;
                }
                rebuilt_loca.extend_from_slice(&u16::try_from(offset / 2).ok()?.to_be_bytes());
            }
        }
        1 => {
            for offset in &rebuilt_offsets {
                rebuilt_loca.extend_from_slice(&u32::try_from(*offset).ok()?.to_be_bytes());
            }
        }
        _ => return None,
    }

    crate::font_subset::replace_glyf_and_loca(base, rebuilt_glyf, rebuilt_loca)
}

fn redirect_font_descriptor_program(
    document: &mut EditDocument,
    descriptor: CowObjectHandle,
    from: CowObjectHandle,
    to: CowObjectHandle,
) -> Result<bool> {
    let object = document.edit_handle(descriptor)?;
    let Some(dictionary) = object.as_dictionary_mut() else {
        return Ok(false);
    };
    let Some(OwnedObject::Reference(current)) = dictionary.get(b"FontFile2".as_slice()) else {
        return Ok(false);
    };
    if *current != from {
        return Ok(false);
    }
    dictionary.insert(b"FontFile2".to_vec(), OwnedObject::Reference(to));
    Ok(true)
}

struct IncomingReferenceOwners {
    owners: HashMap<CowObjectHandle, BTreeSet<CowObjectHandle>>,
    roots: BTreeSet<CowObjectHandle>,
}

fn incoming_reference_owners(document: &EditDocument) -> Result<IncomingReferenceOwners> {
    let roots = document.output_roots().into_iter().collect::<BTreeSet<_>>();
    let mut incoming = HashMap::<CowObjectHandle, BTreeSet<CowObjectHandle>>::new();
    document.walk_output_objects(|owner, object| {
        let references = match object {
            CurrentObject::Source(_) => {
                let CowObjectHandle::Existing(id) = owner else {
                    return Err(Error::Invalid(
                        "source object unexpectedly has a new-object handle".to_owned(),
                    ));
                };
                document
                    .source()
                    .references(id)?
                    .into_iter()
                    .map(CowObjectHandle::Existing)
                    .collect::<Vec<_>>()
            }
            CurrentObject::Owned(object) => object.references(),
        };
        for target in references {
            incoming.entry(target).or_default().insert(owner);
        }
        Ok(())
    })?;
    Ok(IncomingReferenceOwners {
        owners: incoming,
        roots,
    })
}

struct SparseCidUnionCandidate {
    program: CowObjectHandle,
    descriptors: Vec<CowObjectHandle>,
    base_name: Vec<u8>,
    font: Vec<u8>,
    raw_len: usize,
}

#[expect(
    clippy::too_many_lines,
    reason = "font discovery, compatibility checks, and document rewrites form one ordered optimization transaction"
)]
fn union_sparse_cid_font_programs(
    document: &mut EditDocument,
    flate_level: i32,
    program_usage: &HashMap<CowObjectHandle, FontProgramUsage>,
    program_descriptors: &HashMap<CowObjectHandle, Vec<CowObjectHandle>>,
    union_unsafe_programs: &BTreeSet<CowObjectHandle>,
) -> Result<FontOptimizationStats> {
    let mut candidates = Vec::<SparseCidUnionCandidate>::new();
    let mut ordered_programs = program_usage
        .iter()
        .map(|(&program, &usage)| (program, usage))
        .collect::<Vec<_>>();
    ordered_programs.sort_unstable_by_key(|(program, _)| *program);
    for (program, usage) in ordered_programs {
        if !usage.cidfont_type2_only() || union_unsafe_programs.contains(&program) {
            continue;
        }
        let Some(descriptors) = program_descriptors.get(&program) else {
            continue;
        };
        if descriptors.is_empty() {
            continue;
        }
        let mut base_name = None::<Vec<u8>>;
        let mut consistent_name = true;
        for descriptor in descriptors {
            let Some(name) = descriptor_base_font_name(document, *descriptor)? else {
                consistent_name = false;
                break;
            };
            match &base_name {
                Some(existing) if existing != &name => {
                    consistent_name = false;
                    break;
                }
                Some(_) => {}
                None => base_name = Some(name),
            }
        }
        if !consistent_name {
            continue;
        }
        let Some(base_name) = base_name else {
            continue;
        };

        let Some(object) = document.current_owned_object(program)? else {
            continue;
        };
        let OwnedObject::Stream { dictionary, data } = object else {
            continue;
        };
        if !dictionary.contains_key(b"Filter".as_slice())
            || !is_unfiltered_or_lone_flate(document, &dictionary)?
        {
            continue;
        }
        let raw = data.bytes(document.source())?;
        let Ok(decoded) = decode_stream(document, &dictionary, raw.as_ref()) else {
            continue;
        };
        if decoded.get(..4) != Some(&[0, 1, 0, 0]) {
            continue;
        }
        let font = sfnt_for_pdf_rendering(&decoded, usage)
            .map(|(trimmed, _)| trimmed)
            .unwrap_or(decoded);
        let Some(_) = sparse_cid_union_skeleton_hash(&font) else {
            continue;
        };
        candidates.push(SparseCidUnionCandidate {
            program,
            descriptors: descriptors.clone(),
            base_name,
            font,
            raw_len: raw.len(),
        });
    }

    let mut groups = BTreeMap::<(Vec<u8>, [u8; 32]), Vec<usize>>::new();
    for (index, candidate) in candidates.iter().enumerate() {
        let Some(skeleton) = sparse_cid_union_skeleton_hash(&candidate.font) else {
            continue;
        };
        groups
            .entry((candidate.base_name.clone(), skeleton))
            .or_default()
            .push(index);
    }

    if groups.values().all(|indices| indices.len() < 2) {
        return Ok(FontOptimizationStats::default());
    }

    // Only pay the page-content glyph-usage scan after proving that at least
    // one 2+ program rendering-skeleton group exists.
    let (identity_glyph_usage, _) = font_glyph_usage(document)?;
    let incoming_references = incoming_reference_owners(document)?;

    let mut stats = FontOptimizationStats::default();
    let mut eliminated_programs = BTreeSet::new();
    for indices in groups.into_values() {
        let indices = indices
            .into_iter()
            .filter(|index| identity_glyph_usage.contains_key(&candidates[*index].program))
            .collect::<Vec<_>>();
        if indices.len() < 2 {
            continue;
        }
        let fonts = indices
            .iter()
            .map(|index| {
                let candidate = &candidates[*index];
                (
                    candidate.font.as_slice(),
                    &identity_glyph_usage[&candidate.program],
                )
            })
            .collect::<Vec<_>>();
        let Some(mut union_font) = sfnt_union_sparse_glyphs(&fonts) else {
            continue;
        };
        let requested_gids = indices
            .iter()
            .flat_map(|index| {
                identity_glyph_usage[&candidates[*index].program]
                    .iter()
                    .copied()
            })
            .collect::<BTreeSet<_>>();
        if let Some(subset_union) =
            crate::font_subset::retain_glyph_ids(&union_font, &requested_gids)
        {
            union_font = subset_union.bytes;
        }

        let canonical_index = indices[0];
        let canonical = &candidates[canonical_index];

        // Prove that every program we are about to eliminate becomes
        // unreachable. The only permitted incoming owners are the exact
        // indirect FontDescriptor objects that this pass will rewrite.
        let mut closure_ok = true;
        for index in indices
            .iter()
            .copied()
            .filter(|index| *index != canonical_index)
        {
            let candidate = &candidates[index];
            if incoming_references.roots.contains(&candidate.program) {
                closure_ok = false;
                break;
            }
            let expected = candidate
                .descriptors
                .iter()
                .copied()
                .collect::<BTreeSet<_>>();
            let actual = incoming_references
                .owners
                .get(&candidate.program)
                .cloned()
                .unwrap_or_default();
            if actual != expected {
                closure_ok = false;
                break;
            }
        }
        if !closure_ok {
            continue;
        }
        let Some(object) = document.current_owned_object(canonical.program)? else {
            continue;
        };
        let OwnedObject::Stream { dictionary, .. } = object else {
            continue;
        };
        if !is_unfiltered_or_lone_flate(document, &dictionary)? {
            continue;
        }
        let Ok(encoded) = encode_flate(&union_font, flate_level) else {
            continue;
        };
        let original_encoded = indices
            .iter()
            .map(|index| candidates[*index].raw_len)
            .sum::<usize>();
        if encoded.len() >= original_encoded {
            continue;
        }

        replace_current_stream_data(document, canonical.program, encoded.clone(), true)?;
        // Keep Length1 truthful for the newly synthesized program. Existing
        // single-program table stripping intentionally preserves candidate-32
        // behavior; this only applies to the union stream.
        let object = document.edit_handle(canonical.program)?;
        if let Some(dictionary) = object.as_dictionary_mut() {
            dictionary.insert(
                b"Length1".to_vec(),
                OwnedObject::Integer(i64::try_from(union_font.len()).map_err(|_| {
                    Error::Invalid("union font Length1 exceeds PDF integer range".to_owned())
                })?),
            );
        }

        let expected_redirects = indices
            .iter()
            .filter(|index| candidates[**index].program != canonical.program)
            .map(|index| candidates[*index].descriptors.len())
            .sum::<usize>();
        let mut redirected = 0usize;
        for index in &indices {
            let candidate = &candidates[*index];
            if candidate.program == canonical.program {
                continue;
            }
            for descriptor in &candidate.descriptors {
                redirected += usize::from(redirect_font_descriptor_program(
                    document,
                    *descriptor,
                    candidate.program,
                    canonical.program,
                )?);
            }
        }
        if redirected != expected_redirects {
            return Err(Error::Invalid(format!(
                "sparse CID font union redirected {redirected} of {expected_redirects} proven descriptor reference(s)"
            )));
        }
        for index in &indices {
            let program = candidates[*index].program;
            if program != canonical.program {
                eliminated_programs.insert(program);
            }
        }

        stats.programs_optimized += indices.len();
        stats.original_encoded_bytes += original_encoded;
        stats.optimized_encoded_bytes += encoded.len();
    }
    if !eliminated_programs.is_empty() {
        let reachable = document
            .reachable_output_objects()?
            .into_iter()
            .collect::<BTreeSet<_>>();
        let residual = eliminated_programs
            .intersection(&reachable)
            .copied()
            .collect::<Vec<_>>();
        if !residual.is_empty() {
            return Err(Error::Invalid(format!(
                "sparse CID font union left {} eliminated program(s) reachable",
                residual.len()
            )));
        }
    }
    Ok(stats)
}

const HAYRO_FONT_FILE_KEYS: [&[u8]; 2] = [b"FontFile2", b"FontFile3"];

fn owned_name_value(
    document: &EditDocument,
    value: Option<&OwnedObject>,
) -> Result<Option<Vec<u8>>> {
    let Some(value) = value else {
        return Ok(None);
    };
    Ok(match document.resolve_owned_value(value)? {
        Some(OwnedObject::Name(name)) => Some(name),
        _ => None,
    })
}

const fn direct_owned_reference(value: Option<&OwnedObject>) -> Option<CowObjectHandle> {
    match value {
        Some(OwnedObject::Reference(handle)) => Some(*handle),
        _ => None,
    }
}

fn resolved_owned_dictionary(
    document: &EditDocument,
    value: Option<&OwnedObject>,
) -> Result<Option<OwnedDictionary>> {
    let Some(value) = value else {
        return Ok(None);
    };
    Ok(document
        .resolve_owned_value(value)?
        .and_then(|value| value.as_dictionary().cloned()))
}

fn descriptor_program_ref_for(
    document: &EditDocument,
    descriptor: Option<&OwnedObject>,
    key: &[u8],
) -> Result<Option<CowObjectHandle>> {
    let Some(descriptor) = resolved_owned_dictionary(document, descriptor)? else {
        return Ok(None);
    };
    Ok(direct_owned_reference(descriptor.get(key)))
}

fn descriptor_program_ref(
    document: &EditDocument,
    descriptor: Option<&OwnedObject>,
) -> Result<Option<CowObjectHandle>> {
    descriptor_program_ref_for(document, descriptor, b"FontFile2")
}

fn is_cidfont_type0c_program(document: &EditDocument, program: CowObjectHandle) -> Result<bool> {
    let Some(OwnedObject::Stream { dictionary, .. }) = document.current_owned_object(program)?
    else {
        return Ok(false);
    };
    Ok(
        owned_name_value(document, dictionary.get(b"Subtype".as_slice()))?.as_deref()
            == Some(b"CIDFontType0C"),
    )
}

fn union_fontfile2_program(
    document: &EditDocument,
    descriptor: CowObjectHandle,
) -> Result<Option<CowObjectHandle>> {
    let Some(object) = document.current_owned_object(descriptor)? else {
        return Ok(None);
    };
    let Some(dictionary) = object.as_dictionary() else {
        return Ok(None);
    };
    let font_file2 = direct_owned_reference(dictionary.get(b"FontFile2".as_slice()));
    if font_file2.is_none()
        || direct_owned_reference(dictionary.get(b"FontFile".as_slice())).is_some()
        || direct_owned_reference(dictionary.get(b"FontFile3".as_slice())).is_some()
    {
        return Ok(None);
    }
    Ok(font_file2)
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum CidToGidMapping {
    Identity,
    Explicit(Vec<u16>),
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct CidFontGlyphSpec {
    program: CowObjectHandle,
    mapping: CidToGidMapping,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum PageCidFontProgram {
    Eligible(CidFontGlyphSpec),
    Unsafe(CowObjectHandle),
}

fn cid_to_gid_mapping(
    document: &EditDocument,
    value: Option<&OwnedObject>,
) -> Result<Option<CidToGidMapping>> {
    let Some(value) = value else {
        return Ok(Some(CidToGidMapping::Identity));
    };
    if owned_name_value(document, Some(value))?.as_deref() == Some(b"Identity") {
        return Ok(Some(CidToGidMapping::Identity));
    }

    let decoded = match value {
        OwnedObject::Reference(handle) => document.decoded_stream_data(*handle),
        OwnedObject::Stream { .. } => document.decoded_owned_stream_data(value),
        _ => return Ok(None),
    };
    let Ok(decoded) = decoded else {
        return Ok(None);
    };
    if decoded.len() % 2 != 0 {
        return Ok(None);
    }
    Ok(Some(CidToGidMapping::Explicit(
        decoded
            .as_chunks::<2>()
            .0
            .iter()
            .map(|pair| u16::from_be_bytes(*pair))
            .collect(),
    )))
}

/// Return the embedded TrueType program and the CID-to-GID mapping required
/// for retain-GID subsetting. Unsupported mappings remain explicitly unsafe so
/// a program shared through another eligible font resource cannot be subset.
fn page_font_program(
    document: &EditDocument,
    value: &OwnedObject,
) -> Result<Option<PageCidFontProgram>> {
    let Some(font) = resolved_owned_dictionary(document, Some(value))? else {
        return Ok(None);
    };
    let subtype = owned_name_value(document, font.get(b"Subtype".as_slice()))?;
    if subtype.as_deref() == Some(b"TrueType") {
        return Ok(
            descriptor_program_ref(document, font.get(b"FontDescriptor".as_slice()))?
                .map(PageCidFontProgram::Unsafe),
        );
    }
    if subtype.as_deref() != Some(b"Type0") {
        return Ok(None);
    }

    let encoding = owned_name_value(document, font.get(b"Encoding".as_slice()))?;
    let identity_encoding = matches!(encoding.as_deref(), Some(b"Identity-H" | b"Identity-V"));
    let Some(descendants) = font.get(b"DescendantFonts".as_slice()) else {
        return Ok(None);
    };
    let Some(OwnedObject::Array(descendants)) = document.resolve_owned_value(descendants)? else {
        return Ok(None);
    };
    if descendants.len() != 1 {
        return Ok(None);
    }
    let Some(cid_font) = resolved_owned_dictionary(document, descendants.first())? else {
        return Ok(None);
    };
    let cid_subtype = owned_name_value(document, cid_font.get(b"Subtype".as_slice()))?;

    match cid_subtype.as_deref() {
        Some(b"CIDFontType2") => {
            let Some(program) =
                descriptor_program_ref(document, cid_font.get(b"FontDescriptor".as_slice()))?
            else {
                return Ok(None);
            };
            if !identity_encoding {
                return Ok(Some(PageCidFontProgram::Unsafe(program)));
            }
            let Some(mapping) =
                cid_to_gid_mapping(document, cid_font.get(b"CIDToGIDMap".as_slice()))?
            else {
                return Ok(Some(PageCidFontProgram::Unsafe(program)));
            };
            Ok(Some(PageCidFontProgram::Eligible(CidFontGlyphSpec {
                program,
                mapping,
            })))
        }
        Some(b"CIDFontType0") => {
            let Some(program) = descriptor_program_ref_for(
                document,
                cid_font.get(b"FontDescriptor".as_slice()),
                b"FontFile3",
            )?
            else {
                return Ok(None);
            };
            if !identity_encoding || !is_cidfont_type0c_program(document, program)? {
                return Ok(Some(PageCidFontProgram::Unsafe(program)));
            }
            Ok(Some(PageCidFontProgram::Eligible(CidFontGlyphSpec {
                program,
                mapping: CidToGidMapping::Identity,
            })))
        }
        _ => Ok(None),
    }
}

struct IdentityCidGlyphScanner<'a> {
    fonts: &'a HashMap<Vec<u8>, CidFontGlyphSpec>,
    current_font: Option<&'a CidFontGlyphSpec>,
    used: HashMap<CowObjectHandle, BTreeSet<u16>>,
    unsafe_programs: BTreeSet<CowObjectHandle>,
}

impl IdentityCidGlyphScanner<'_> {
    fn record_string(&mut self, bytes: &[u8]) {
        let Some(font) = self.current_font else {
            return;
        };
        let (pairs, remainder) = bytes.as_chunks::<2>();
        if !remainder.is_empty() {
            self.unsafe_programs.insert(font.program);
            return;
        }
        for pair in pairs {
            let cid = u16::from_be_bytes(*pair);
            let gid = match &font.mapping {
                CidToGidMapping::Identity => cid,
                CidToGidMapping::Explicit(mapping) => {
                    let Some(&gid) = mapping.get(usize::from(cid)) else {
                        self.unsafe_programs.insert(font.program);
                        return;
                    };
                    gid
                }
            };
            self.used.entry(font.program).or_default().insert(gid);
        }
    }

    fn instruction(&mut self, instruction: &hayro_syntax::content::Instruction<'_, '_>) {
        let mut operands = instruction.operands();
        match &instruction.operator[..] {
            b"Tf" => {
                self.current_font = operands
                    .next()
                    .and_then(crate::content_stream::operand_name)
                    .and_then(|name| self.fonts.get(name));
            }
            b"Tj" | b"'" => {
                if let Some(bytes) = operands
                    .next()
                    .and_then(crate::content_stream::operand_string)
                {
                    self.record_string(bytes);
                }
            }
            b"\"" => {
                if let Some(bytes) = operands
                    .nth(2)
                    .and_then(crate::content_stream::operand_string)
                {
                    self.record_string(bytes);
                }
            }
            b"TJ" => {
                if let Some(HayroObject::Array(array)) = operands.next() {
                    for item in array.raw_iter() {
                        if let HayroMaybeRef::NotRef(HayroObject::String(value)) = item {
                            self.record_string(value.as_bytes());
                        }
                    }
                }
            }
            _ => {}
        }
    }
}

fn page_simple_truetype_program(
    document: &EditDocument,
    value: &OwnedObject,
) -> Result<Option<(CowObjectHandle, bool)>> {
    let Some(font) = resolved_owned_dictionary(document, Some(value))? else {
        return Ok(None);
    };
    if owned_name_value(document, font.get(b"Subtype".as_slice()))?.as_deref() != Some(b"TrueType")
    {
        return Ok(None);
    }
    let Some(program) = descriptor_program_ref(document, font.get(b"FontDescriptor".as_slice()))?
    else {
        return Ok(None);
    };
    let winansi = owned_name_value(document, font.get(b"Encoding".as_slice()))?.as_deref()
        == Some(b"WinAnsiEncoding");
    Ok(Some((program, winansi)))
}

struct WinAnsiCodeScanner<'a> {
    fonts: &'a HashMap<Vec<u8>, CowObjectHandle>,
    current_program: Option<CowObjectHandle>,
    used: HashMap<CowObjectHandle, BTreeSet<u8>>,
    unsafe_programs: BTreeSet<CowObjectHandle>,
}

impl WinAnsiCodeScanner<'_> {
    fn record_string(&mut self, bytes: &[u8]) {
        let Some(program) = self.current_program else {
            return;
        };
        if bytes.iter().any(|byte| !(0x20..=0x7e).contains(byte)) {
            self.unsafe_programs.insert(program);
            return;
        }
        self.used
            .entry(program)
            .or_default()
            .extend(bytes.iter().copied());
    }

    fn instruction(&mut self, instruction: &hayro_syntax::content::Instruction<'_, '_>) {
        let mut operands = instruction.operands();
        match &instruction.operator[..] {
            b"Tf" => {
                self.current_program = operands
                    .next()
                    .and_then(crate::content_stream::operand_name)
                    .and_then(|name| self.fonts.get(name).copied());
            }
            b"Tj" | b"'" => {
                if let Some(bytes) = operands
                    .next()
                    .and_then(crate::content_stream::operand_string)
                {
                    self.record_string(bytes);
                }
            }
            b"\"" => {
                if let Some(bytes) = operands
                    .nth(2)
                    .and_then(crate::content_stream::operand_string)
                {
                    self.record_string(bytes);
                }
            }
            b"TJ" => {
                if let Some(HayroObject::Array(array)) = operands.next() {
                    for item in array.raw_iter() {
                        if let HayroMaybeRef::NotRef(HayroObject::String(value)) = item {
                            self.record_string(value.as_bytes());
                        }
                    }
                }
            }
            _ => {}
        }
    }
}

fn eligible_winansi_fonts(
    document: &EditDocument,
    resources: &OwnedDictionary,
    unsafe_programs: &mut BTreeSet<CowObjectHandle>,
) -> Result<HashMap<Vec<u8>, CowObjectHandle>> {
    let Some(font_resources) =
        resolved_owned_dictionary(document, resources.get(b"Font".as_slice()))?
    else {
        return Ok(HashMap::new());
    };
    let mut eligible = HashMap::new();
    for (name, value) in &font_resources {
        let Some((program, winansi)) = page_simple_truetype_program(document, value)? else {
            continue;
        };
        if winansi {
            eligible.insert(name.clone(), program);
        } else {
            unsafe_programs.insert(program);
        }
    }
    Ok(eligible)
}

fn eligible_identity_fonts(
    document: &EditDocument,
    resources: &OwnedDictionary,
    unsafe_programs: &mut BTreeSet<CowObjectHandle>,
) -> Result<HashMap<Vec<u8>, CidFontGlyphSpec>> {
    let Some(font_resources) =
        resolved_owned_dictionary(document, resources.get(b"Font".as_slice()))?
    else {
        return Ok(HashMap::new());
    };
    let mut eligible = HashMap::new();
    for (name, value) in &font_resources {
        let Some(program) = page_font_program(document, value)? else {
            continue;
        };
        match program {
            PageCidFontProgram::Eligible(spec) => {
                eligible.insert(name.clone(), spec);
            }
            PageCidFontProgram::Unsafe(program) => {
                // One embedded program can be referenced through multiple font
                // dictionaries. Any unsupported use makes retain-GID subsetting
                // unsafe for the shared program.
                unsafe_programs.insert(program);
            }
        }
    }
    Ok(eligible)
}

fn scan_font_usage_scope(
    content: &[u8],
    identity_eligible: &HashMap<Vec<u8>, CidFontGlyphSpec>,
    winansi_eligible: &HashMap<Vec<u8>, CowObjectHandle>,
    identity_used: &mut HashMap<CowObjectHandle, BTreeSet<u16>>,
    identity_unsafe: &mut BTreeSet<CowObjectHandle>,
    winansi_used: &mut HashMap<CowObjectHandle, BTreeSet<u8>>,
    winansi_unsafe: &mut BTreeSet<CowObjectHandle>,
) {
    if identity_eligible.is_empty() && winansi_eligible.is_empty() {
        return;
    }
    let mut identity = IdentityCidGlyphScanner {
        fonts: identity_eligible,
        current_font: None,
        used: HashMap::new(),
        unsafe_programs: BTreeSet::new(),
    };
    let mut winansi = WinAnsiCodeScanner {
        fonts: winansi_eligible,
        current_program: None,
        used: HashMap::new(),
        unsafe_programs: BTreeSet::new(),
    };
    let parsed = crate::content_stream::visit_instructions(content, |instruction| {
        identity.instruction(instruction);
        winansi.instruction(instruction);
        Ok(())
    });
    if !matches!(parsed, Ok(false)) {
        identity_unsafe.extend(identity_eligible.values().map(|spec| spec.program));
        winansi_unsafe.extend(winansi_eligible.values().copied());
        return;
    }
    identity_unsafe.extend(identity.unsafe_programs);
    for (program, gids) in identity.used {
        identity_used.entry(program).or_default().extend(gids);
    }
    winansi_unsafe.extend(winansi.unsafe_programs);
    for (program, codes) in winansi.used {
        winansi_used.entry(program).or_default().extend(codes);
    }
}

fn form_xobject_handles(document: &EditDocument) -> Result<Vec<CowObjectHandle>> {
    let mut forms = Vec::new();
    document.walk_output_objects(|handle, object| {
        let subtype = match object {
            CurrentObject::Source(HayroObject::Stream(stream)) => stream
                .dict()
                .get::<HayroName<'_>>(b"Subtype")
                .map(|name| name.as_ref().to_vec()),
            CurrentObject::Owned(OwnedObject::Stream { dictionary, .. }) => {
                owned_name_value(document, dictionary.get(b"Subtype".as_slice()))?
            }
            _ => None,
        };
        if subtype.as_deref() == Some(b"Form") {
            forms.push(handle);
        }
        Ok(())
    })?;
    Ok(forms)
}

type IdentityGlyphUsage = HashMap<CowObjectHandle, BTreeSet<u16>>;
type WinAnsiCodeUsage = HashMap<CowObjectHandle, BTreeSet<u8>>;
type FontGlyphUsage = (IdentityGlyphUsage, WinAnsiCodeUsage);

fn font_glyph_usage(document: &EditDocument) -> Result<FontGlyphUsage> {
    let mut identity_used = HashMap::<CowObjectHandle, BTreeSet<u16>>::new();
    let mut identity_unsafe = BTreeSet::new();
    let mut winansi_used = HashMap::<CowObjectHandle, BTreeSet<u8>>::new();
    let mut winansi_unsafe = BTreeSet::new();

    for page in document.page_handles()? {
        let Some(resources_value) = document.inherited_page_value(page, b"Resources")? else {
            continue;
        };
        let Some(resources) = resolved_owned_dictionary(document, Some(&resources_value))? else {
            continue;
        };
        let identity_eligible =
            eligible_identity_fonts(document, &resources, &mut identity_unsafe)?;
        let winansi_eligible = eligible_winansi_fonts(document, &resources, &mut winansi_unsafe)?;
        if identity_eligible.is_empty() && winansi_eligible.is_empty() {
            continue;
        }
        let Some(page_object) = document.current_owned_object(page)? else {
            continue;
        };
        let Some(page_dictionary) = page_object.as_dictionary() else {
            continue;
        };
        let Some(contents) = page_dictionary.get(b"Contents".as_slice()) else {
            continue;
        };
        let mut content = Vec::new();
        decoded_content_value(document, contents, &mut content)?;
        scan_font_usage_scope(
            &content,
            &identity_eligible,
            &winansi_eligible,
            &mut identity_used,
            &mut identity_unsafe,
            &mut winansi_used,
            &mut winansi_unsafe,
        );
    }

    for form in form_xobject_handles(document)? {
        let Some(form_object) = document.current_owned_object(form)? else {
            continue;
        };
        let OwnedObject::Stream { dictionary, .. } = &form_object else {
            continue;
        };
        let Some(resources) =
            resolved_owned_dictionary(document, dictionary.get(b"Resources".as_slice()))?
        else {
            // A resource-less Form borrows its caller's scope. Until usage scanning
            // is call-graph-aware, it poisons both retain-GID proofs.
            return Ok((HashMap::new(), HashMap::new()));
        };
        let identity_eligible =
            eligible_identity_fonts(document, &resources, &mut identity_unsafe)?;
        let winansi_eligible = eligible_winansi_fonts(document, &resources, &mut winansi_unsafe)?;
        if identity_eligible.is_empty() && winansi_eligible.is_empty() {
            continue;
        }
        let Ok(content) = document.decoded_content_stream_data(form) else {
            identity_unsafe.extend(identity_eligible.values().map(|spec| spec.program));
            winansi_unsafe.extend(winansi_eligible.values().copied());
            continue;
        };
        scan_font_usage_scope(
            &content,
            &identity_eligible,
            &winansi_eligible,
            &mut identity_used,
            &mut identity_unsafe,
            &mut winansi_used,
            &mut winansi_unsafe,
        );
    }

    for program in &identity_unsafe {
        identity_used.remove(program);
    }
    for program in &winansi_unsafe {
        winansi_used.remove(program);
    }
    Ok((identity_used, winansi_used))
}

fn inspect_hayro_font_dictionary(
    holder: CowObjectHandle,
    dictionary: &HayroDict<'_>,
    descriptor_usage_edges: &mut Vec<(CowObjectHandle, FontProgramUsage)>,
    descriptor_program_edges: &mut Vec<(CowObjectHandle, CowObjectHandle)>,
) {
    if let Some(descriptor) = dictionary.get_ref(b"FontDescriptor") {
        let subtype = dictionary.get::<HayroName<'_>>(b"Subtype");
        let usage = match subtype.as_ref().map(AsRef::<[u8]>::as_ref) {
            Some(b"TrueType") => FontProgramUsage {
                simple_truetype: true,
                cidfont_type2: false,
                cidfont_type0: false,
            },
            Some(b"CIDFontType2") => FontProgramUsage {
                simple_truetype: false,
                cidfont_type2: true,
                cidfont_type0: false,
            },
            Some(b"CIDFontType0") => FontProgramUsage {
                simple_truetype: false,
                cidfont_type2: false,
                cidfont_type0: true,
            },
            _ => FontProgramUsage::default(),
        };
        if usage != FontProgramUsage::default() {
            descriptor_usage_edges.push((CowObjectHandle::Existing(descriptor.into()), usage));
        }
    }

    for key in HAYRO_FONT_FILE_KEYS {
        if let Some(program) = dictionary.get_ref(key) {
            descriptor_program_edges.push((holder, CowObjectHandle::Existing(program.into())));
        }
    }
}

fn inspect_owned_font_dictionary(
    document: &EditDocument,
    holder: CowObjectHandle,
    dictionary: &OwnedDictionary,
    descriptor_usage_edges: &mut Vec<(CowObjectHandle, FontProgramUsage)>,
    descriptor_program_edges: &mut Vec<(CowObjectHandle, CowObjectHandle)>,
) -> Result<()> {
    if let Some(descriptor) = direct_owned_reference(dictionary.get(b"FontDescriptor".as_slice())) {
        let usage =
            match owned_name_value(document, dictionary.get(b"Subtype".as_slice()))?.as_deref() {
                Some(b"TrueType") => FontProgramUsage {
                    simple_truetype: true,
                    cidfont_type2: false,
                    cidfont_type0: false,
                },
                Some(b"CIDFontType2") => FontProgramUsage {
                    simple_truetype: false,
                    cidfont_type2: true,
                    cidfont_type0: false,
                },
                Some(b"CIDFontType0") => FontProgramUsage {
                    simple_truetype: false,
                    cidfont_type2: false,
                    cidfont_type0: true,
                },
                _ => FontProgramUsage::default(),
            };
        if usage != FontProgramUsage::default() {
            descriptor_usage_edges.push((descriptor, usage));
        }
    }

    for key in HAYRO_FONT_FILE_KEYS {
        if let Some(program) = direct_owned_reference(dictionary.get(key)) {
            descriptor_program_edges.push((holder, program));
        }
    }
    Ok(())
}

fn merge_program_usage(
    program_usage: &mut HashMap<CowObjectHandle, FontProgramUsage>,
    program: CowObjectHandle,
    usage: FontProgramUsage,
) {
    let merged = program_usage.entry(program).or_default();
    merged.simple_truetype |= usage.simple_truetype;
    merged.cidfont_type2 |= usage.cidfont_type2;
    merged.cidfont_type0 |= usage.cidfont_type0;
}

fn hayro_dictionary_font_usage(dictionary: &HayroDict<'_>) -> FontProgramUsage {
    match dictionary
        .get::<HayroName<'_>>(b"Subtype")
        .as_ref()
        .map(AsRef::<[u8]>::as_ref)
    {
        Some(b"TrueType") => FontProgramUsage {
            simple_truetype: true,
            cidfont_type2: false,
            cidfont_type0: false,
        },
        Some(b"CIDFontType2") => FontProgramUsage {
            simple_truetype: false,
            cidfont_type2: true,
            cidfont_type0: false,
        },
        Some(b"CIDFontType0") => FontProgramUsage {
            simple_truetype: false,
            cidfont_type2: false,
            cidfont_type0: true,
        },
        _ => FontProgramUsage::default(),
    }
}

fn inspect_hayro_direct_font_dictionary(
    dictionary: &HayroDict<'_>,
    program_usage: &mut HashMap<CowObjectHandle, FontProgramUsage>,
    descriptor_usage_edges: &mut Vec<(CowObjectHandle, FontProgramUsage)>,
) {
    let usage = hayro_dictionary_font_usage(dictionary);
    if usage != FontProgramUsage::default() {
        for (name, value) in dictionary.entries() {
            if name.as_ref() != b"FontDescriptor" {
                continue;
            }
            match value {
                HayroMaybeRef::Ref(descriptor) => {
                    descriptor_usage_edges
                        .push((CowObjectHandle::Existing(descriptor.into()), usage));
                }
                HayroMaybeRef::NotRef(HayroObject::Dict(descriptor)) => {
                    for key in HAYRO_FONT_FILE_KEYS {
                        if let Some(program) = descriptor.get_ref(key) {
                            merge_program_usage(
                                program_usage,
                                CowObjectHandle::Existing(program.into()),
                                usage,
                            );
                        }
                    }
                }
                HayroMaybeRef::NotRef(_) => {}
            }
            break;
        }
    }

    for (_, value) in dictionary.entries() {
        let HayroMaybeRef::NotRef(value) = value else {
            continue;
        };
        inspect_hayro_direct_font_object(&value, program_usage, descriptor_usage_edges);
    }
}

fn inspect_hayro_direct_font_object(
    object: &HayroObject<'_>,
    program_usage: &mut HashMap<CowObjectHandle, FontProgramUsage>,
    descriptor_usage_edges: &mut Vec<(CowObjectHandle, FontProgramUsage)>,
) {
    match object {
        HayroObject::Dict(dictionary) => {
            inspect_hayro_direct_font_dictionary(dictionary, program_usage, descriptor_usage_edges);
        }
        HayroObject::Stream(stream) => {
            inspect_hayro_direct_font_dictionary(
                stream.dict(),
                program_usage,
                descriptor_usage_edges,
            );
        }
        HayroObject::Array(array) => {
            for value in array.raw_iter() {
                let HayroMaybeRef::NotRef(value) = value else {
                    continue;
                };
                inspect_hayro_direct_font_object(&value, program_usage, descriptor_usage_edges);
            }
        }
        HayroObject::Null(_)
        | HayroObject::Boolean(_)
        | HayroObject::Number(_)
        | HayroObject::String(_)
        | HayroObject::Name(_) => {}
    }
}

fn owned_dictionary_font_usage(
    document: &EditDocument,
    dictionary: &OwnedDictionary,
) -> Result<FontProgramUsage> {
    Ok(
        match owned_name_value(document, dictionary.get(b"Subtype".as_slice()))?.as_deref() {
            Some(b"TrueType") => FontProgramUsage {
                simple_truetype: true,
                cidfont_type2: false,
                cidfont_type0: false,
            },
            Some(b"CIDFontType2") => FontProgramUsage {
                simple_truetype: false,
                cidfont_type2: true,
                cidfont_type0: false,
            },
            Some(b"CIDFontType0") => FontProgramUsage {
                simple_truetype: false,
                cidfont_type2: false,
                cidfont_type0: true,
            },
            _ => FontProgramUsage::default(),
        },
    )
}

fn inspect_owned_direct_font_dictionary(
    document: &EditDocument,
    dictionary: &OwnedDictionary,
    program_usage: &mut HashMap<CowObjectHandle, FontProgramUsage>,
    descriptor_usage_edges: &mut Vec<(CowObjectHandle, FontProgramUsage)>,
) -> Result<()> {
    let usage = owned_dictionary_font_usage(document, dictionary)?;
    if usage != FontProgramUsage::default()
        && let Some(descriptor) = dictionary.get(b"FontDescriptor".as_slice())
    {
        match descriptor {
            OwnedObject::Reference(descriptor) => {
                descriptor_usage_edges.push((*descriptor, usage));
            }
            OwnedObject::Dictionary(descriptor) => {
                for key in HAYRO_FONT_FILE_KEYS {
                    if let Some(program) = direct_owned_reference(descriptor.get(key)) {
                        merge_program_usage(program_usage, program, usage);
                    }
                }
            }
            _ => {}
        }
    }

    for value in dictionary.values() {
        if matches!(value, OwnedObject::Reference(_)) {
            continue;
        }
        inspect_owned_direct_font_object(document, value, program_usage, descriptor_usage_edges)?;
    }
    Ok(())
}

fn inspect_owned_direct_font_object(
    document: &EditDocument,
    object: &OwnedObject,
    program_usage: &mut HashMap<CowObjectHandle, FontProgramUsage>,
    descriptor_usage_edges: &mut Vec<(CowObjectHandle, FontProgramUsage)>,
) -> Result<()> {
    match object {
        OwnedObject::Dictionary(dictionary) | OwnedObject::Stream { dictionary, .. } => {
            inspect_owned_direct_font_dictionary(
                document,
                dictionary,
                program_usage,
                descriptor_usage_edges,
            )?;
        }
        OwnedObject::Array(values) => {
            for value in values {
                if matches!(value, OwnedObject::Reference(_)) {
                    continue;
                }
                inspect_owned_direct_font_object(
                    document,
                    value,
                    program_usage,
                    descriptor_usage_edges,
                )?;
            }
        }
        OwnedObject::Null
        | OwnedObject::Boolean(_)
        | OwnedObject::Integer(_)
        | OwnedObject::Real(_)
        | OwnedObject::Name(_)
        | OwnedObject::String(_)
        | OwnedObject::Reference(_) => {}
    }
    Ok(())
}

fn replace_current_stream_data(
    document: &mut EditDocument,
    handle: CowObjectHandle,
    encoded: Vec<u8>,
    plain_flate: bool,
) -> Result<()> {
    let object = document.edit_handle(handle)?;
    match object {
        OwnedObject::Stream { dictionary, data } => {
            if plain_flate {
                set_plain_flate(dictionary);
            }
            *data = StreamData::Owned(encoded);
            Ok(())
        }
        _ => Err(Error::Invalid(
            "font program reference does not resolve to a stream".to_owned(),
        )),
    }
}

#[derive(Debug, Clone, Default)]
struct DenseCidProgramUsers {
    maps: BTreeSet<CowObjectHandle>,
    unsafe_mapping: bool,
}

fn collect_dense_cidfont_users_from_object(
    document: &EditDocument,
    object: &OwnedObject,
    users: &mut HashMap<CowObjectHandle, DenseCidProgramUsers>,
) -> Result<()> {
    match object {
        OwnedObject::Dictionary(dictionary) | OwnedObject::Stream { dictionary, .. } => {
            if owned_name_value(document, dictionary.get(b"Subtype".as_slice()))?.as_deref()
                == Some(b"CIDFontType2")
                && let Some(program) =
                    descriptor_program_ref(document, dictionary.get(b"FontDescriptor".as_slice()))?
            {
                let entry = users.entry(program).or_default();
                match dictionary.get(b"CIDToGIDMap".as_slice()) {
                    Some(OwnedObject::Reference(map))
                        if document
                            .current_object(*map)?
                            .is_some_and(|object| object.is_stream()) =>
                    {
                        entry.maps.insert(*map);
                    }
                    _ => entry.unsafe_mapping = true,
                }
            }
            for value in dictionary.values() {
                if !matches!(value, OwnedObject::Reference(_)) {
                    collect_dense_cidfont_users_from_object(document, value, users)?;
                }
            }
        }
        OwnedObject::Array(values) => {
            for value in values {
                if !matches!(value, OwnedObject::Reference(_)) {
                    collect_dense_cidfont_users_from_object(document, value, users)?;
                }
            }
        }
        OwnedObject::Null
        | OwnedObject::Boolean(_)
        | OwnedObject::Integer(_)
        | OwnedObject::Real(_)
        | OwnedObject::Name(_)
        | OwnedObject::String(_)
        | OwnedObject::Reference(_) => {}
    }
    Ok(())
}

fn dense_cid_program_users(
    document: &EditDocument,
) -> Result<HashMap<CowObjectHandle, DenseCidProgramUsers>> {
    let mut users = HashMap::new();
    for handle in document.reachable_output_objects()? {
        let Some(object) = document.current_owned_object(handle)? else {
            continue;
        };
        collect_dense_cidfont_users_from_object(document, &object, &mut users)?;
    }
    Ok(users)
}

fn current_font_program_usage(
    document: &EditDocument,
) -> Result<HashMap<CowObjectHandle, FontProgramUsage>> {
    let mut descriptor_usage_edges = Vec::new();
    let mut descriptor_program_edges = Vec::new();
    let mut direct_program_usage = HashMap::<CowObjectHandle, FontProgramUsage>::new();
    document.walk_output_objects(|handle, object| match object {
        CurrentObject::Source(object) => {
            match &object {
                HayroObject::Dict(dictionary) => inspect_hayro_font_dictionary(
                    handle,
                    dictionary,
                    &mut descriptor_usage_edges,
                    &mut descriptor_program_edges,
                ),
                HayroObject::Stream(stream) => inspect_hayro_font_dictionary(
                    handle,
                    stream.dict(),
                    &mut descriptor_usage_edges,
                    &mut descriptor_program_edges,
                ),
                _ => {}
            }
            inspect_hayro_direct_font_object(
                &object,
                &mut direct_program_usage,
                &mut descriptor_usage_edges,
            );
            Ok(())
        }
        CurrentObject::Owned(object) => {
            if let Some(dictionary) = object.as_dictionary() {
                inspect_owned_font_dictionary(
                    document,
                    handle,
                    dictionary,
                    &mut descriptor_usage_edges,
                    &mut descriptor_program_edges,
                )?;
            }
            inspect_owned_direct_font_object(
                document,
                object,
                &mut direct_program_usage,
                &mut descriptor_usage_edges,
            )?;
            Ok(())
        }
    })?;

    let mut descriptor_usage = HashMap::<CowObjectHandle, FontProgramUsage>::new();
    for (descriptor, usage) in descriptor_usage_edges {
        let merged = descriptor_usage.entry(descriptor).or_default();
        merged.simple_truetype |= usage.simple_truetype;
        merged.cidfont_type2 |= usage.cidfont_type2;
        merged.cidfont_type0 |= usage.cidfont_type0;
    }

    let mut program_usage = HashMap::<CowObjectHandle, FontProgramUsage>::new();
    for (descriptor, program) in descriptor_program_edges {
        let Some(usage) = descriptor_usage.get(&descriptor).copied() else {
            continue;
        };
        merge_program_usage(&mut program_usage, program, usage);
    }
    for (program, usage) in direct_program_usage {
        merge_program_usage(&mut program_usage, program, usage);
    }
    Ok(program_usage)
}

fn decoded_u16_mapping(
    document: &EditDocument,
    handle: CowObjectHandle,
) -> Result<Option<(OwnedDictionary, Vec<u16>, usize)>> {
    let Some(OwnedObject::Stream { dictionary, data }) = document.current_owned_object(handle)?
    else {
        return Ok(None);
    };
    let raw = data.bytes(document.source())?;
    let decoded = if dictionary.contains_key(b"Filter".as_slice()) {
        if !is_unfiltered_or_lone_flate(document, &dictionary)? {
            return Ok(None);
        }
        let Ok(decoded) = decode_stream(document, &dictionary, raw.as_ref()) else {
            return Ok(None);
        };
        decoded
    } else {
        raw.as_ref().to_vec()
    };
    if decoded.len() % 2 != 0 {
        return Ok(None);
    }
    let values = decoded
        .as_chunks::<2>()
        .0
        .iter()
        .map(|pair| u16::from_be_bytes(*pair))
        .collect::<Vec<_>>();
    Ok(Some((dictionary, values, raw.len())))
}

fn encode_like_stream(
    document: &EditDocument,
    dictionary: &OwnedDictionary,
    decoded: &[u8],
    flate_level: i32,
) -> Result<Option<(Vec<u8>, bool)>> {
    if !dictionary.contains_key(b"Filter".as_slice()) {
        return Ok(Some((decoded.to_vec(), false)));
    }
    if !is_unfiltered_or_lone_flate(document, dictionary)? {
        return Ok(None);
    }
    Ok(encode_flate(decoded, flate_level)
        .ok()
        .map(|encoded| (encoded, true)))
}

fn set_font_program_length1(
    document: &mut EditDocument,
    handle: CowObjectHandle,
    decoded_len: usize,
) -> Result<()> {
    let decoded_len = i64::try_from(decoded_len)
        .map_err(|_| Error::Invalid("dense font program length exceeds i64".to_owned()))?;
    let object = document.edit_handle(handle)?;
    let OwnedObject::Stream { dictionary, .. } = object else {
        return Err(Error::Invalid(
            "dense font program reference does not resolve to a stream".to_owned(),
        ));
    };
    dictionary.insert(b"Length1".to_vec(), OwnedObject::Integer(decoded_len));
    Ok(())
}

/// Densely renumber TrueType GIDs for `CIDFontType2` programs whose every PDF
/// user has an explicit rewritable `CIDToGIDMap`. Every GID addressable by those
/// maps is retained (plus composite dependencies), so page text codes and CID
/// widths stay unchanged; only the embedded SFNT and CID-to-GID map values move.
#[expect(
    clippy::too_many_lines,
    reason = "dense CID remapping keeps whole-program eligibility, map preparation, SFNT rewrite, and atomic size gating together for auditability"
)]
pub fn dense_compact_cidfont_type2_programs(
    document: &mut EditDocument,
    flate_level: i32,
) -> Result<FontOptimizationStats> {
    let program_usage = current_font_program_usage(document)?;
    let users = dense_cid_program_users(document)?;
    if users.is_empty() {
        return Ok(FontOptimizationStats::default());
    }

    let mut map_programs = HashMap::<CowObjectHandle, BTreeSet<CowObjectHandle>>::new();
    for (program, program_users) in &users {
        for map in &program_users.maps {
            map_programs.entry(*map).or_default().insert(*program);
        }
    }

    let mut stats = FontOptimizationStats::default();
    for (program, program_users) in users {
        if program_users.unsafe_mapping || program_users.maps.is_empty() {
            continue;
        }
        if !program_usage
            .get(&program)
            .copied()
            .is_some_and(FontProgramUsage::cidfont_type2_only)
        {
            continue;
        }
        if program_users.maps.iter().any(|map| {
            map_programs
                .get(map)
                .is_some_and(|programs| programs.len() != 1)
        }) {
            continue;
        }

        let mut requested_gids = BTreeSet::from([0_u16]);
        let mut map_data = Vec::<(CowObjectHandle, OwnedDictionary, Vec<u16>, usize)>::new();
        let mut maps_ok = true;
        for map in &program_users.maps {
            let Some((dictionary, values, raw_len)) = decoded_u16_mapping(document, *map)? else {
                maps_ok = false;
                break;
            };
            requested_gids.extend(values.iter().copied());
            map_data.push((*map, dictionary, values, raw_len));
        }
        if !maps_ok {
            continue;
        }

        let Some(OwnedObject::Stream {
            dictionary: program_dictionary,
            data: program_data,
        }) = document.current_owned_object(program)?
        else {
            continue;
        };
        let program_raw = program_data.bytes(document.source())?;
        let program_decoded = if program_dictionary.contains_key(b"Filter".as_slice()) {
            if !is_unfiltered_or_lone_flate(document, &program_dictionary)? {
                continue;
            }
            let Ok(decoded) = decode_stream(document, &program_dictionary, program_raw.as_ref())
            else {
                continue;
            };
            decoded
        } else {
            program_raw.as_ref().to_vec()
        };
        let Some(dense) = crate::font_subset::compact_glyph_ids(&program_decoded, &requested_gids)
        else {
            continue;
        };
        let Some((encoded_program, program_plain_flate)) =
            encode_like_stream(document, &program_dictionary, &dense.bytes, flate_level)?
        else {
            continue;
        };

        let mut encoded_maps = Vec::<(CowObjectHandle, Vec<u8>, usize, bool)>::new();
        let mut remap_ok = true;
        for (map, dictionary, values, raw_len) in map_data {
            let mut decoded = Vec::with_capacity(values.len().saturating_mul(2));
            for old_gid in values {
                let Some(new_gid) = dense.old_to_new.get(&old_gid).copied() else {
                    remap_ok = false;
                    break;
                };
                decoded.extend_from_slice(&new_gid.to_be_bytes());
            }
            if !remap_ok {
                break;
            }
            let Some((encoded, plain_flate)) =
                encode_like_stream(document, &dictionary, &decoded, flate_level)?
            else {
                remap_ok = false;
                break;
            };
            encoded_maps.push((map, encoded, raw_len, plain_flate));
        }
        if !remap_ok {
            continue;
        }

        let before_encoded = program_raw.len().saturating_add(
            encoded_maps
                .iter()
                .map(|(_, _, raw_len, _)| *raw_len)
                .sum::<usize>(),
        );
        let after_encoded = encoded_program.len().saturating_add(
            encoded_maps
                .iter()
                .map(|(_, encoded, _, _)| encoded.len())
                .sum::<usize>(),
        );
        if after_encoded >= before_encoded {
            continue;
        }

        replace_current_stream_data(document, program, encoded_program, program_plain_flate)?;
        set_font_program_length1(document, program, dense.bytes.len())?;
        for (map, encoded, _, plain_flate) in encoded_maps {
            replace_current_stream_data(document, map, encoded, plain_flate)?;
        }

        stats.programs_optimized = stats.programs_optimized.saturating_add(1);
        stats.programs_glyph_subset = stats.programs_glyph_subset.saturating_add(1);
        stats.programs_dense_remapped = stats.programs_dense_remapped.saturating_add(1);
        stats.cid_to_gid_maps_rewritten = stats
            .cid_to_gid_maps_rewritten
            .saturating_add(program_users.maps.len());
        stats.dense_glyph_slots_removed = stats.dense_glyph_slots_removed.saturating_add(
            dense
                .original_glyph_count
                .saturating_sub(dense.old_to_new.len()),
        );
        stats.dense_decoded_bytes_removed = stats
            .dense_decoded_bytes_removed
            .saturating_add(dense.removed_decoded_bytes);
        stats.original_encoded_bytes = stats.original_encoded_bytes.saturating_add(before_encoded);
        stats.optimized_encoded_bytes = stats.optimized_encoded_bytes.saturating_add(after_encoded);
    }
    Ok(stats)
}

/// Union compatible sparse `CIDFontType2` programs after exact font-program
/// deduplication has already canonicalized byte-identical stripped subsets.
pub fn union_sparse_cid_font_programs_after_dedup(
    document: &mut EditDocument,
    flate_level: i32,
) -> Result<FontOptimizationStats> {
    let mut descriptor_usage_edges = Vec::new();
    let mut descriptor_program_edges = Vec::new();
    let mut direct_program_usage = HashMap::<CowObjectHandle, FontProgramUsage>::new();
    document.walk_output_objects(|handle, object| match object {
        CurrentObject::Source(object) => {
            match &object {
                HayroObject::Dict(dictionary) => inspect_hayro_font_dictionary(
                    handle,
                    dictionary,
                    &mut descriptor_usage_edges,
                    &mut descriptor_program_edges,
                ),
                HayroObject::Stream(stream) => inspect_hayro_font_dictionary(
                    handle,
                    stream.dict(),
                    &mut descriptor_usage_edges,
                    &mut descriptor_program_edges,
                ),
                _ => {}
            }
            inspect_hayro_direct_font_object(
                &object,
                &mut direct_program_usage,
                &mut descriptor_usage_edges,
            );
            Ok(())
        }
        CurrentObject::Owned(object) => {
            if let Some(dictionary) = object.as_dictionary() {
                inspect_owned_font_dictionary(
                    document,
                    handle,
                    dictionary,
                    &mut descriptor_usage_edges,
                    &mut descriptor_program_edges,
                )?;
            }
            inspect_owned_direct_font_object(
                document,
                object,
                &mut direct_program_usage,
                &mut descriptor_usage_edges,
            )?;
            Ok(())
        }
    })?;

    let mut descriptor_usage = HashMap::<CowObjectHandle, FontProgramUsage>::new();
    for (descriptor, usage) in descriptor_usage_edges {
        let merged = descriptor_usage.entry(descriptor).or_default();
        merged.simple_truetype |= usage.simple_truetype;
        merged.cidfont_type2 |= usage.cidfont_type2;
        merged.cidfont_type0 |= usage.cidfont_type0;
    }

    let mut program_usage = HashMap::<CowObjectHandle, FontProgramUsage>::new();
    let mut program_descriptors = HashMap::<CowObjectHandle, Vec<CowObjectHandle>>::new();
    let mut union_unsafe_programs = BTreeSet::<CowObjectHandle>::new();
    for (descriptor, program) in descriptor_program_edges {
        if union_fontfile2_program(document, descriptor)? == Some(program) {
            program_descriptors
                .entry(program)
                .or_default()
                .push(descriptor);
        } else {
            union_unsafe_programs.insert(program);
        }
        let Some(usage) = descriptor_usage.get(&descriptor).copied() else {
            continue;
        };
        merge_program_usage(&mut program_usage, program, usage);
    }

    union_unsafe_programs.extend(direct_program_usage.keys().copied());
    for (program, usage) in direct_program_usage {
        merge_program_usage(&mut program_usage, program, usage);
    }

    if program_usage.is_empty() {
        return Ok(FontOptimizationStats::default());
    }

    union_sparse_cid_font_programs(
        document,
        flate_level,
        &program_usage,
        &program_descriptors,
        &union_unsafe_programs,
    )
}

/// Hayro/COW port of [`strip_font_editing_tables`].
///
/// The graph walk, mutation, and stream filter handling are Hayro-native.
#[expect(
    clippy::too_many_lines,
    reason = "font usage analysis, table filtering, and encoded-cost gating form one ordered optimization transaction"
)]
pub fn strip_font_editing_tables(
    document: &mut EditDocument,
    flate_level: i32,
) -> Result<FontOptimizationStats> {
    let mut descriptor_usage_edges = Vec::new();
    let mut descriptor_program_edges = Vec::new();
    let mut direct_program_usage = HashMap::<CowObjectHandle, FontProgramUsage>::new();
    document.walk_output_objects(|handle, object| match object {
        CurrentObject::Source(object) => {
            match &object {
                HayroObject::Dict(dictionary) => inspect_hayro_font_dictionary(
                    handle,
                    dictionary,
                    &mut descriptor_usage_edges,
                    &mut descriptor_program_edges,
                ),
                HayroObject::Stream(stream) => inspect_hayro_font_dictionary(
                    handle,
                    stream.dict(),
                    &mut descriptor_usage_edges,
                    &mut descriptor_program_edges,
                ),
                _ => {}
            }
            inspect_hayro_direct_font_object(
                &object,
                &mut direct_program_usage,
                &mut descriptor_usage_edges,
            );
            Ok(())
        }
        CurrentObject::Owned(object) => {
            if let Some(dictionary) = object.as_dictionary() {
                inspect_owned_font_dictionary(
                    document,
                    handle,
                    dictionary,
                    &mut descriptor_usage_edges,
                    &mut descriptor_program_edges,
                )?;
            }
            inspect_owned_direct_font_object(
                document,
                object,
                &mut direct_program_usage,
                &mut descriptor_usage_edges,
            )?;
            Ok(())
        }
    })?;

    let mut descriptor_usage = HashMap::<CowObjectHandle, FontProgramUsage>::new();
    for (descriptor, usage) in descriptor_usage_edges {
        let merged = descriptor_usage.entry(descriptor).or_default();
        merged.simple_truetype |= usage.simple_truetype;
        merged.cidfont_type2 |= usage.cidfont_type2;
        merged.cidfont_type0 |= usage.cidfont_type0;
    }

    let mut program_usage = HashMap::<CowObjectHandle, FontProgramUsage>::new();
    for (descriptor, program) in descriptor_program_edges {
        let Some(usage) = descriptor_usage.get(&descriptor).copied() else {
            continue;
        };
        merge_program_usage(&mut program_usage, program, usage);
    }
    let legacy_subset_programs = program_usage.keys().copied().collect::<BTreeSet<_>>();
    for (program, usage) in direct_program_usage {
        merge_program_usage(&mut program_usage, program, usage);
    }

    if program_usage.is_empty() {
        return Ok(FontOptimizationStats::default());
    }

    let outline_subset_programs = legacy_subset_programs;
    let (identity_glyph_usage, winansi_code_usage) = if outline_subset_programs.is_empty() {
        (HashMap::new(), HashMap::new())
    } else {
        font_glyph_usage(document)?
    };
    let mut stats = FontOptimizationStats::default();
    for (&program, &usage) in &program_usage {
        let Some(object) = document.current_owned_object(program)? else {
            continue;
        };
        let OwnedObject::Stream { dictionary, data } = object else {
            continue;
        };
        if !dictionary.contains_key(b"Filter".as_slice())
            || !is_unfiltered_or_lone_flate(document, &dictionary)?
        {
            continue;
        }
        let raw = data.bytes(document.source())?;
        let Ok(decoded) = decode_stream(document, &dictionary, raw.as_ref()) else {
            continue;
        };
        let allow_outline_subset = outline_subset_programs.contains(&program);

        let mut candidate = decoded;
        let mut removed_decoded_bytes = 0usize;
        let mut glyph_subset_removed_bytes = 0usize;
        let mut changed = single_font_ttc_to_sfnt(&candidate).is_some_and(|unwrapped| {
            candidate = unwrapped;
            true
        });
        if allow_outline_subset
            && usage.cidfont_type0_only()
            && let Some(cids) = identity_glyph_usage.get(&program)
            && !cids.is_empty()
            && let Some((subset, removed)) = crate::cff_cid::subset_cid_font(&candidate, cids)
        {
            candidate = subset;
            glyph_subset_removed_bytes = removed;
            changed = true;
        } else if allow_outline_subset
            && usage.cidfont_type2_only()
            && let Some(gids) = identity_glyph_usage.get(&program)
            && !gids.is_empty()
            && let Some(subset) = crate::font_subset::retain_glyph_ids(&candidate, gids)
        {
            glyph_subset_removed_bytes = subset.removed_decoded_bytes;
            candidate = subset.bytes;
            changed = true;
        } else if allow_outline_subset
            && usage.simple_truetype_only()
            && let Some(codes) = winansi_code_usage.get(&program)
            && !codes.is_empty()
            && let Some(gids) = sfnt_winansi_ascii_glyph_ids(&candidate, codes)
            && let Some(subset) = crate::font_subset::retain_glyph_ids(&candidate, &gids)
        {
            glyph_subset_removed_bytes = subset.removed_decoded_bytes;
            candidate = subset.bytes;
            changed = true;
        }
        if let Some((trimmed, removed)) = sfnt_for_pdf_rendering(&candidate, usage) {
            candidate = trimmed;
            removed_decoded_bytes = removed;
            changed = true;
        }
        if !changed {
            continue;
        }
        let Ok(encoded) = encode_flate(&candidate, flate_level) else {
            continue;
        };
        if encoded.len() >= raw.len() {
            continue;
        }

        stats.programs_optimized += 1;
        stats.programs_glyph_subset += usize::from(glyph_subset_removed_bytes > 0);
        stats.original_encoded_bytes += raw.len();
        stats.optimized_encoded_bytes += encoded.len();
        stats.decoded_table_bytes_removed += removed_decoded_bytes;
        stats.glyph_subset_decoded_bytes_removed += glyph_subset_removed_bytes;
        replace_current_stream_data(document, program, encoded, true)?;
    }

    Ok(stats)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ObjectId;
    use flate2::{Compression, write::ZlibEncoder};
    use std::io::Write;

    fn append_pdf_object(pdf: &mut Vec<u8>, offsets: &mut Vec<usize>, body: &[u8]) {
        offsets.push(pdf.len());
        pdf.extend_from_slice(body);
    }

    fn hayro_font_fixture_with_filter_array(array_filter: bool) -> Result<Vec<u8>> {
        let head = [0_u8; 54];
        let gsub = vec![0x55_u8; 8192];
        let source_font = sfnt(&[
            (*b"head", &head),
            (*b"glyf", b"glyph-data"),
            (*b"hmtx", b"metric-data"),
            (*b"cmap", b"mapping-data"),
            (*b"post", b"post-data"),
            (*b"GSUB", gsub.as_slice()),
        ]);
        let mut compressor = ZlibEncoder::new(Vec::new(), Compression::best());
        compressor.write_all(&source_font)?;
        let encoded = compressor.finish()?;

        let mut pdf = b"%PDF-1.4\n".to_vec();
        let mut offsets = Vec::new();
        append_pdf_object(
            &mut pdf,
            &mut offsets,
            b"1 0 obj\n<< /Type /Catalog /Pages 2 0 R >>\nendobj\n",
        );
        append_pdf_object(
            &mut pdf,
            &mut offsets,
            b"2 0 obj\n<< /Type /Pages /Kids [3 0 R] /Count 1 >>\nendobj\n",
        );
        append_pdf_object(
            &mut pdf,
            &mut offsets,
            b"3 0 obj\n<< /Type /Page /Parent 2 0 R /MediaBox [0 0 10 10] /Resources << /Font << /F1 5 0 R >> >> /Contents 4 0 R >>\nendobj\n",
        );
        append_pdf_object(
            &mut pdf,
            &mut offsets,
            b"4 0 obj\n<< /Length 3 >>\nstream\nq Q\nendstream\nendobj\n",
        );
        append_pdf_object(
            &mut pdf,
            &mut offsets,
            b"5 0 obj\n<< /Type /Font /Subtype /TrueType /BaseFont /TestFont /FontDescriptor 6 0 R >>\nendobj\n",
        );
        append_pdf_object(
            &mut pdf,
            &mut offsets,
            b"6 0 obj\n<< /Type /FontDescriptor /FontName /TestFont /FontFile2 7 0 R >>\nendobj\n",
        );
        let header = if array_filter {
            format!(
                "7 0 obj\n<< /Length {} /Filter [ /FlateDecode ] /DecodeParms [ << /Predictor 1 >> ] >>\nstream\n",
                encoded.len()
            )
        } else {
            format!(
                "7 0 obj\n<< /Length {} /Filter /FlateDecode /DecodeParms << /Predictor 1 >> >>\nstream\n",
                encoded.len()
            )
        };
        let mut stream_object = header.into_bytes();
        stream_object.extend_from_slice(&encoded);
        stream_object.extend_from_slice(b"\nendstream\nendobj\n");
        append_pdf_object(&mut pdf, &mut offsets, &stream_object);

        let xref_offset = pdf.len();
        pdf.extend_from_slice(format!("xref\n0 {}\n", offsets.len() + 1).as_bytes());
        pdf.extend_from_slice(b"0000000000 65535 f \n");
        for offset in offsets {
            pdf.extend_from_slice(format!("{offset:010} 00000 n \n").as_bytes());
        }
        pdf.extend_from_slice(
            format!("trailer\n<< /Size 8 /Root 1 0 R >>\nstartxref\n{xref_offset}\n%%EOF\n")
                .as_bytes(),
        );
        Ok(pdf)
    }

    fn hayro_direct_cid_font_fixture() -> Result<Vec<u8>> {
        let head = [0_u8; 54];
        let gsub = vec![0x55_u8; 8192];
        let source_font = sfnt(&[
            (*b"head", &head),
            (*b"glyf", b"glyph-data"),
            (*b"hmtx", b"metric-data"),
            (*b"cmap", b"mapping-data"),
            (*b"post", b"post-data"),
            (*b"GSUB", gsub.as_slice()),
        ]);
        let mut compressor = ZlibEncoder::new(Vec::new(), Compression::best());
        compressor.write_all(&source_font)?;
        let encoded = compressor.finish()?;

        let mut pdf = b"%PDF-1.4\n".to_vec();
        let mut offsets = Vec::new();
        append_pdf_object(
            &mut pdf,
            &mut offsets,
            b"1 0 obj\n<< /Type /Catalog /Pages 2 0 R >>\nendobj\n",
        );
        append_pdf_object(
            &mut pdf,
            &mut offsets,
            b"2 0 obj\n<< /Type /Pages /Kids [3 0 R] /Count 1 >>\nendobj\n",
        );
        append_pdf_object(
            &mut pdf,
            &mut offsets,
            b"3 0 obj\n<< /Type /Page /Parent 2 0 R /MediaBox [0 0 10 10] /Resources << /Font << /F1 << /Type /Font /Subtype /Type0 /BaseFont /TestFont /Encoding /Identity-H /DescendantFonts [ << /Type /Font /Subtype /CIDFontType2 /BaseFont /TestFont /FontDescriptor << /Type /FontDescriptor /FontName /TestFont /FontFile2 5 0 R >> >> ] >> >> >> /Contents 4 0 R >>\nendobj\n",
        );
        append_pdf_object(
            &mut pdf,
            &mut offsets,
            b"4 0 obj\n<< /Length 3 >>\nstream\nq Q\nendstream\nendobj\n",
        );
        let header = format!(
            "5 0 obj\n<< /Length {} /Filter /FlateDecode /DecodeParms << /Predictor 1 >> >>\nstream\n",
            encoded.len()
        );
        let mut stream_object = header.into_bytes();
        stream_object.extend_from_slice(&encoded);
        stream_object.extend_from_slice(b"\nendstream\nendobj\n");
        append_pdf_object(&mut pdf, &mut offsets, &stream_object);

        let xref_offset = pdf.len();
        pdf.extend_from_slice(format!("xref\n0 {}\n", offsets.len() + 1).as_bytes());
        pdf.extend_from_slice(b"0000000000 65535 f \n");
        for offset in offsets {
            pdf.extend_from_slice(format!("{offset:010} 00000 n \n").as_bytes());
        }
        pdf.extend_from_slice(
            format!("trailer\n<< /Size 6 /Root 1 0 R >>\nstartxref\n{xref_offset}\n%%EOF\n")
                .as_bytes(),
        );
        Ok(pdf)
    }

    fn dense_cidfont_fixture() -> Result<Vec<u8>> {
        let font = sparse_union_test_font(
            &[
                Some(simple_test_glyph(0x10)),
                Some(vec![0x44_u8; 4096]),
                Some(vec![0x55_u8; 4096]),
                Some(simple_test_glyph(0x13)),
            ],
            [0, 0, 100, 100],
        )?;
        let map = [0_u8, 0, 0, 0, 0, 0, 0, 3];

        let content = b"BT /F1 10 Tf <0003> Tj ET";
        let mut pdf = b"%PDF-1.4\n".to_vec();
        let mut offsets = Vec::new();
        append_pdf_object(
            &mut pdf,
            &mut offsets,
            b"1 0 obj\n<< /Type /Catalog /Pages 2 0 R >>\nendobj\n",
        );
        append_pdf_object(
            &mut pdf,
            &mut offsets,
            b"2 0 obj\n<< /Type /Pages /Kids [3 0 R] /Count 1 >>\nendobj\n",
        );
        append_pdf_object(
            &mut pdf,
            &mut offsets,
            b"3 0 obj\n<< /Type /Page /Parent 2 0 R /MediaBox [0 0 100 100] /Resources << /Font << /F1 6 0 R >> >> /Contents 4 0 R >>\nendobj\n",
        );
        append_pdf_object(
            &mut pdf,
            &mut offsets,
            format!(
                "4 0 obj\n<< /Length {} >>\nstream\n{}\nendstream\nendobj\n",
                content.len(),
                String::from_utf8_lossy(content)
            )
            .as_bytes(),
        );
        let mut font_stream = format!(
            "5 0 obj\n<< /Length {} /Length1 {} >>\nstream\n",
            font.len(),
            font.len()
        )
        .into_bytes();
        font_stream.extend_from_slice(&font);
        font_stream.extend_from_slice(b"\nendstream\nendobj\n");
        append_pdf_object(&mut pdf, &mut offsets, &font_stream);
        append_pdf_object(
            &mut pdf,
            &mut offsets,
            b"6 0 obj\n<< /Type /Font /Subtype /Type0 /BaseFont /DenseTest /Encoding /Identity-H /DescendantFonts [7 0 R] >>\nendobj\n",
        );
        append_pdf_object(
            &mut pdf,
            &mut offsets,
            b"7 0 obj\n<< /Type /Font /Subtype /CIDFontType2 /BaseFont /DenseTest /CIDSystemInfo << /Registry (Adobe) /Ordering (Identity) /Supplement 0 >> /FontDescriptor 8 0 R /CIDToGIDMap 9 0 R /DW 1000 /W [3 [500]] >>\nendobj\n",
        );
        append_pdf_object(
            &mut pdf,
            &mut offsets,
            b"8 0 obj\n<< /Type /FontDescriptor /FontName /DenseTest /Flags 4 /FontBBox [0 0 100 100] /ItalicAngle 0 /Ascent 100 /Descent 0 /CapHeight 100 /StemV 80 /FontFile2 5 0 R >>\nendobj\n",
        );
        let mut map_stream = format!("9 0 obj\n<< /Length {} >>\nstream\n", map.len()).into_bytes();
        map_stream.extend_from_slice(&map);
        map_stream.extend_from_slice(b"\nendstream\nendobj\n");
        append_pdf_object(&mut pdf, &mut offsets, &map_stream);

        let xref_offset = pdf.len();
        pdf.extend_from_slice(format!("xref\n0 {}\n", offsets.len() + 1).as_bytes());
        pdf.extend_from_slice(b"0000000000 65535 f \n");
        for offset in offsets {
            pdf.extend_from_slice(format!("{offset:010} 00000 n \n").as_bytes());
        }
        pdf.extend_from_slice(
            format!("trailer\n<< /Size 10 /Root 1 0 R >>\nstartxref\n{xref_offset}\n%%EOF\n")
                .as_bytes(),
        );
        Ok(pdf)
    }

    #[test]
    fn dense_cidfont_rewrites_explicit_map_and_is_idempotent() -> Result<()> {
        let mut document = EditDocument::from_bytes(dense_cidfont_fixture()?)?;
        let first = dense_compact_cidfont_type2_programs(&mut document, 9)?;
        assert_eq!(first.programs_dense_remapped, 1);
        assert_eq!(first.cid_to_gid_maps_rewritten, 1);
        assert_eq!(first.dense_glyph_slots_removed, 2);

        let cid_map_stream = CowObjectHandle::Existing(ObjectId::new(9, 0));
        let decoded_map = document.decoded_stream_data(cid_map_stream)?;
        assert_eq!(decoded_map.as_slice(), &[0, 0, 0, 0, 0, 0, 0, 1]);

        let program = CowObjectHandle::Existing(ObjectId::new(5, 0));
        let decoded_program = document.decoded_stream_data(program)?;
        {
            use write_fonts::read::{FontRef, TableProvider};
            let font = FontRef::new(decoded_program.as_ref())
                .map_err(|_| Error::Invalid("dense fixture is not a valid font".to_owned()))?;
            let maxp = font
                .maxp()
                .map_err(|_| Error::Invalid("dense fixture lost maxp".to_owned()))?;
            assert_eq!(maxp.num_glyphs(), 2);
        }

        let output = document.write_compact()?;
        let mut reparsed = EditDocument::from_bytes(output)?;
        let second = dense_compact_cidfont_type2_programs(&mut reparsed, 9)?;
        assert_eq!(second.programs_dense_remapped, 0);
        assert_eq!(second.cid_to_gid_maps_rewritten, 0);
        Ok(())
    }

    #[test]
    fn hayro_direct_cid_descriptor_font_is_optimized() -> Result<()> {
        let input = hayro_direct_cid_font_fixture()?;
        let mut document = EditDocument::from_bytes(input)?;
        let first = strip_font_editing_tables(&mut document, 9)?;
        assert_eq!(first.programs_optimized, 1);
        assert_eq!(first.programs_glyph_subset, 0);
        assert!(first.optimized_encoded_bytes < first.original_encoded_bytes);
        assert!(first.decoded_table_bytes_removed > 8192);

        let output = document.write_compact()?;
        let mut reparsed = EditDocument::from_bytes(output)?;
        let second = strip_font_editing_tables(&mut reparsed, 9)?;
        assert_eq!(second.programs_optimized, 0);
        Ok(())
    }

    #[test]
    fn hayro_font_table_strip_accepts_single_flate_filter_array() -> Result<()> {
        let input = hayro_font_fixture_with_filter_array(true)?;
        let mut document = EditDocument::from_bytes(input)?;
        let first = strip_font_editing_tables(&mut document, 9)?;
        assert_eq!(first.programs_optimized, 1);
        assert!(first.optimized_encoded_bytes < first.original_encoded_bytes);
        assert_eq!(first.decoded_table_bytes_removed, 8192);

        let output = document.write_compact()?;
        let mut reparsed = EditDocument::from_bytes(output)?;
        let second = strip_font_editing_tables(&mut reparsed, 9)?;
        assert_eq!(second.programs_optimized, 0);
        Ok(())
    }

    fn sfnt(tables: &[([u8; 4], &[u8])]) -> Vec<u8> {
        use write_fonts::{FontBuilder, types::Tag};

        let mut builder = FontBuilder::new();
        for (tag, data) in tables {
            builder.add_raw(Tag::new(tag), data.to_vec());
        }
        builder.build()
    }

    fn sparse_union_test_font(glyphs: &[Option<Vec<u8>>], bbox: [i16; 4]) -> Result<Vec<u8>> {
        let mut head = [0_u8; 54];
        head[36..38].copy_from_slice(&bbox[0].to_be_bytes());
        head[38..40].copy_from_slice(&bbox[1].to_be_bytes());
        head[40..42].copy_from_slice(&bbox[2].to_be_bytes());
        head[42..44].copy_from_slice(&bbox[3].to_be_bytes());
        head[50..52].copy_from_slice(&1_i16.to_be_bytes()); // long loca
        let glyph_count_u8 = u8::try_from(glyphs.len()).map_err(|_| {
            Error::Invalid("sparse-union test fixture has too many glyphs".to_owned())
        })?;
        let glyph_count_u16 = u16::try_from(glyphs.len()).map_err(|_| {
            Error::Invalid("sparse-union test fixture has too many glyphs".to_owned())
        })?;
        let mut maxp = vec![0, 1, 0, 0, 0, glyph_count_u8];
        maxp.resize(32, 0);
        let mut hhea = vec![0_u8; 36];
        hhea[34..36].copy_from_slice(&glyph_count_u16.to_be_bytes());
        let hmtx = vec![0_u8; glyphs.len() * 4];

        let mut glyf = Vec::new();
        let mut offsets = Vec::new();
        for glyph in glyphs {
            offsets.push(
                u32::try_from(glyf.len())
                    .map_err(|_| Error::Invalid("test glyph data exceeds u32".to_owned()))?,
            );
            if let Some(glyph) = glyph {
                glyf.extend_from_slice(glyph);
                while glyf.len() % 4 != 0 {
                    glyf.push(0);
                }
            }
        }
        offsets.push(
            u32::try_from(glyf.len())
                .map_err(|_| Error::Invalid("test glyph data exceeds u32".to_owned()))?,
        );
        let loca = offsets
            .iter()
            .flat_map(|offset| offset.to_be_bytes())
            .collect::<Vec<_>>();
        Ok(sfnt(&[
            (*b"head", &head),
            (*b"maxp", &maxp),
            (*b"hhea", &hhea),
            (*b"hmtx", &hmtx),
            (*b"loca", &loca),
            (*b"glyf", &glyf),
            (*b"cvt ", b"cvt"),
            (*b"fpgm", b"fpgm"),
            (*b"prep", b"prep"),
        ]))
    }

    fn simple_test_glyph(marker: u8) -> Vec<u8> {
        let mut glyph = vec![0_u8; 12];
        glyph[0..2].copy_from_slice(&1_i16.to_be_bytes());
        glyph[10] = marker;
        glyph[11] = marker ^ 0x5a;
        glyph
    }

    #[test]
    fn sparse_cid_union_combines_disjoint_gids_and_preserves_head_bbox() -> Result<()> {
        use write_fonts::read::{FontRef, TableProvider};

        let bbox = [-123_i16, -456, 789, 1024];
        let glyph0 = simple_test_glyph(0x10);
        let glyph1 = simple_test_glyph(0x11);
        let glyph2 = simple_test_glyph(0x12);
        let left =
            sparse_union_test_font(&[Some(glyph0.clone()), Some(glyph1.clone()), None], bbox)?;
        let right = sparse_union_test_font(&[Some(glyph0), None, Some(glyph2.clone())], bbox)?;
        let left_used = BTreeSet::from([1_u16]);
        let right_used = BTreeSet::from([2_u16]);
        let Some(union) = sfnt_union_sparse_glyphs(&[
            (left.as_slice(), &left_used),
            (right.as_slice(), &right_used),
        ]) else {
            return Err(Error::Invalid(
                "compatible sparse fonts should union".to_owned(),
            ));
        };
        let font = FontRef::new(&union)
            .map_err(|_| Error::Invalid("union should be a valid font".to_owned()))?;
        let head = font
            .head()
            .map_err(|_| Error::Invalid("union should retain head".to_owned()))?;
        assert_eq!(
            [head.x_min(), head.y_min(), head.x_max(), head.y_max()],
            bbox
        );

        let offsets = sfnt_glyph_offsets(&union)
            .ok_or_else(|| Error::Invalid("union should have valid loca".to_owned()))?;
        let glyf = font
            .glyf()
            .map_err(|_| Error::Invalid("union should retain glyf".to_owned()))?;
        let glyf = glyf.offset_data().as_bytes();
        assert_eq!(&glyf[offsets[1]..offsets[2]], glyph1.as_slice());
        assert_eq!(&glyf[offsets[2]..offsets[3]], glyph2.as_slice());
        Ok(())
    }

    #[test]
    fn sparse_cid_union_rejects_same_gid_outline_conflict() -> Result<()> {
        let bbox = [0_i16, 0, 100, 100];
        let a = sparse_union_test_font(
            &[Some(simple_test_glyph(0)), Some(simple_test_glyph(1))],
            bbox,
        )?;
        let b = sparse_union_test_font(
            &[Some(simple_test_glyph(0)), Some(simple_test_glyph(2))],
            bbox,
        )?;
        let used = BTreeSet::from([1_u16]);
        assert!(
            sfnt_union_sparse_glyphs(&[(a.as_slice(), &used), (b.as_slice(), &used)]).is_none()
        );
        Ok(())
    }

    #[test]
    fn sparse_cid_union_rejects_different_head_bbox() -> Result<()> {
        let glyphs = [Some(simple_test_glyph(0)), Some(simple_test_glyph(1))];
        let a = sparse_union_test_font(&glyphs, [0, 0, 100, 100])?;
        let b = sparse_union_test_font(&glyphs, [0, 0, 101, 100])?;
        let used = BTreeSet::from([1_u16]);
        assert!(
            sfnt_union_sparse_glyphs(&[(a.as_slice(), &used), (b.as_slice(), &used)]).is_none()
        );
        Ok(())
    }

    fn tags(bytes: &[u8]) -> Vec<[u8; 4]> {
        let Ok(font) = write_fonts::read::FontRef::new(bytes) else {
            return Vec::new();
        };
        font.table_directory()
            .table_records()
            .iter()
            .map(|record| record.tag().to_be_bytes())
            .collect()
    }

    fn cmap_table(platform: u16, encoding: u16, subtable: &[u8]) -> Vec<u8> {
        let mut cmap = Vec::new();
        cmap.extend_from_slice(&0_u16.to_be_bytes());
        cmap.extend_from_slice(&1_u16.to_be_bytes());
        cmap.extend_from_slice(&platform.to_be_bytes());
        cmap.extend_from_slice(&encoding.to_be_bytes());
        cmap.extend_from_slice(&12_u32.to_be_bytes());
        cmap.extend_from_slice(subtable);
        cmap
    }

    #[test]
    fn winansi_ascii_uses_unicode_cmap_format4_without_renumbering_gids() {
        // Two segments: A-C -> gids 5-7, then the required 0xffff sentinel.
        let mut format4 = Vec::new();
        format4.extend_from_slice(&4_u16.to_be_bytes());
        format4.extend_from_slice(&32_u16.to_be_bytes());
        format4.extend_from_slice(&0_u16.to_be_bytes());
        format4.extend_from_slice(&4_u16.to_be_bytes()); // segCountX2
        format4.extend_from_slice(&4_u16.to_be_bytes()); // searchRange
        format4.extend_from_slice(&1_u16.to_be_bytes()); // entrySelector
        format4.extend_from_slice(&0_u16.to_be_bytes()); // rangeShift
        format4.extend_from_slice(&67_u16.to_be_bytes());
        format4.extend_from_slice(&u16::MAX.to_be_bytes());
        format4.extend_from_slice(&0_u16.to_be_bytes()); // reservedPad
        format4.extend_from_slice(&65_u16.to_be_bytes());
        format4.extend_from_slice(&u16::MAX.to_be_bytes());
        format4.extend_from_slice(&(-60_i16).to_be_bytes()); // 65 + (-60) = gid 5
        format4.extend_from_slice(&1_i16.to_be_bytes());
        format4.extend_from_slice(&0_u16.to_be_bytes());
        format4.extend_from_slice(&0_u16.to_be_bytes());
        let cmap = cmap_table(3, 1, &format4);
        let font = sfnt(&[(*b"cmap", &cmap)]);
        assert_eq!(sfnt_unicode_gid(&font, 65), Some(5));
        assert_eq!(sfnt_unicode_gid(&font, 67), Some(7));
        assert_eq!(
            sfnt_winansi_ascii_glyph_ids(&font, &BTreeSet::from(*b"AC")),
            Some(BTreeSet::from([5_u16, 7_u16]))
        );
        assert!(sfnt_winansi_ascii_glyph_ids(&font, &BTreeSet::from([0x80])).is_none());
    }

    #[test]
    fn unicode_cmap_format12_maps_supplementary_codepoint() {
        let mut format12 = Vec::new();
        format12.extend_from_slice(&12_u16.to_be_bytes());
        format12.extend_from_slice(&0_u16.to_be_bytes());
        format12.extend_from_slice(&28_u32.to_be_bytes());
        format12.extend_from_slice(&0_u32.to_be_bytes());
        format12.extend_from_slice(&1_u32.to_be_bytes());
        format12.extend_from_slice(&0x1f600_u32.to_be_bytes());
        format12.extend_from_slice(&0x1f602_u32.to_be_bytes());
        format12.extend_from_slice(&42_u32.to_be_bytes());
        let cmap = cmap_table(3, 10, &format12);
        let font = sfnt(&[(*b"cmap", &cmap)]);
        assert_eq!(sfnt_unicode_gid(&font, 0x1f600), Some(42));
        assert_eq!(sfnt_unicode_gid(&font, 0x1f602), Some(44));
    }

    #[test]
    fn rendering_sfnt_drops_layout_and_vertical_metric_tables() -> Result<()> {
        let head = [0_u8; 54];
        let source = sfnt(&[
            (*b"head", &head),
            (*b"glyf", b"glyphs"),
            (*b"hmtx", b"metrics"),
            (*b"GSUB", b"substitution"),
            (*b"GPOS", b"positioning"),
            (*b"vhea", b"vertical header"),
            (*b"vmtx", b"vertical metrics"),
            (*b"hdmx", b"device metrics"),
            (*b"LTSH", b"linear threshold"),
            (*b"VDMX", b"vertical device metrics"),
        ]);
        let Some((trimmed, removed)) = sfnt_for_pdf_rendering(&source, FontProgramUsage::default())
        else {
            return Err(Error::Invalid(
                "expected removable rendering-unused tables".to_owned(),
            ));
        };
        let remaining = tags(&trimmed);
        assert!(remaining.contains(b"head"));
        assert!(remaining.contains(b"glyf"));
        assert!(remaining.contains(b"hmtx"));
        assert!(!remaining.contains(b"GSUB"));
        assert!(!remaining.contains(b"GPOS"));
        assert!(!remaining.contains(b"vhea"));
        assert!(!remaining.contains(b"vmtx"));
        assert!(!remaining.contains(b"hdmx"));
        assert!(!remaining.contains(b"LTSH"));
        assert!(!remaining.contains(b"VDMX"));
        assert_eq!(removed, 12 + 11 + 15 + 16 + 14 + 16 + 23);
        Ok(())
    }

    #[test]
    fn cidfont_type2_drops_cmap_and_post() -> Result<()> {
        let head = [0_u8; 54];
        let source = sfnt(&[
            (*b"head", &head),
            (*b"glyf", b"glyphs"),
            (*b"cmap", b"mapping"),
            (*b"post", b"names"),
            (*b"name", b"editing metadata"),
        ]);
        let cid_usage = FontProgramUsage {
            simple_truetype: false,
            cidfont_type2: true,
            cidfont_type0: false,
        };
        let Some((trimmed, _)) = sfnt_for_pdf_rendering(&source, cid_usage) else {
            return Err(Error::Invalid("expected CID-only table removal".to_owned()));
        };
        let remaining = tags(&trimmed);
        assert!(!remaining.contains(b"cmap"));
        assert!(!remaining.contains(b"post"));

        let simple_usage = FontProgramUsage {
            simple_truetype: true,
            cidfont_type2: false,
            cidfont_type0: false,
        };
        let Some((simple, _)) = sfnt_for_pdf_rendering(&source, simple_usage) else {
            return Err(Error::Invalid("expected metadata table removal".to_owned()));
        };
        let simple_remaining = tags(&simple);
        assert!(simple_remaining.contains(b"cmap"));
        assert!(simple_remaining.contains(b"post"));
        Ok(())
    }

    #[test]
    fn rendering_sfnt_is_noop_without_removable_tables() {
        let head = [0_u8; 54];
        let source = sfnt(&[(*b"head", &head), (*b"glyf", b"glyphs")]);
        assert!(sfnt_for_pdf_rendering(&source, FontProgramUsage::default()).is_none());
    }
}
