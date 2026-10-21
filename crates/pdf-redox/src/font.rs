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
    const fn from_subtype(subtype: Option<&[u8]>) -> Self {
        match subtype {
            Some(b"TrueType") => Self {
                simple_truetype: true,
                cidfont_type2: false,
                cidfont_type0: false,
            },
            Some(b"CIDFontType2") => Self {
                simple_truetype: false,
                cidfont_type2: true,
                cidfont_type0: false,
            },
            Some(b"CIDFontType0") => Self {
                simple_truetype: false,
                cidfont_type2: false,
                cidfont_type0: true,
            },
            _ => Self {
                simple_truetype: false,
                cidfont_type2: false,
                cidfont_type0: false,
            },
        }
    }

    const fn merge(&mut self, other: Self) {
        self.simple_truetype |= other.simple_truetype;
        self.cidfont_type2 |= other.cidfont_type2;
        self.cidfont_type0 |= other.cidfont_type0;
    }

    const fn cidfont_type2_only(self) -> bool {
        self.cidfont_type2 && !self.simple_truetype && !self.cidfont_type0
    }

    const fn cidfont_type0_only(self) -> bool {
        self.cidfont_type0 && !self.simple_truetype && !self.cidfont_type2
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct FontOptimizationStats {
    pub programs_optimized: usize,
    pub programs_glyph_subset: usize,
    pub programs_dense_remapped: usize,
    pub cid_to_gid_maps_rewritten: usize,
    pub cid_cmaps_removed: usize,
    pub cid_cmap_decoded_bytes_removed: usize,
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

fn sfnt_for_pdf_rendering(bytes: &[u8]) -> Option<(Vec<u8>, usize)> {
    crate::font_subset::strip_pdf_unused_tables(bytes)
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

fn sfnt_blank_unused_glyph_outlines(
    bytes: &[u8],
    used_gids: &BTreeSet<u16>,
) -> Option<(Vec<u8>, usize)> {
    use write_fonts::read::{FontRef, TableProvider};

    let font = FontRef::new(bytes).ok()?;
    if font.table_directory().sfnt_version() != 0x0001_0000 {
        return None;
    }
    let glyph_count = usize::from(font.maxp().ok()?.num_glyphs());
    if glyph_count == 0 || used_gids.iter().any(|gid| usize::from(*gid) >= glyph_count) {
        return None;
    }
    let offsets = sfnt_glyph_offsets(bytes)?;
    let glyf = font.glyf().ok()?.offset_data().as_bytes();

    // Preserve the complete original GID address space. Only outline bodies are
    // discarded; cmap, metrics, hint programs, maxp.numGlyphs and every other
    // SFNT table retain their source semantics.
    let mut retained = BTreeSet::new();
    let mut pending = VecDeque::from([0_usize]);
    pending.extend(used_gids.iter().map(|gid| usize::from(*gid)));
    while let Some(gid) = pending.pop_front() {
        if gid >= glyph_count || !retained.insert(gid) {
            continue;
        }
        let glyph = glyf.get(offsets[gid]..offsets[gid + 1])?;
        // A zero-length TrueType glyph is a valid empty outline and has no
        // composite dependencies.
        if glyph.is_empty() {
            continue;
        }
        for component in composite_components(glyph)? {
            let component = usize::from(component);
            if component >= glyph_count {
                return None;
            }
            if !retained.contains(&component) {
                pending.push_back(component);
            }
        }
    }

    let loca_format = font.head().ok()?.index_to_loc_format();
    let alignment = match loca_format {
        0 => 2usize,
        1 => 4usize,
        _ => return None,
    };
    let mut rebuilt_glyf = Vec::new();
    let mut rebuilt_offsets = Vec::with_capacity(glyph_count + 1);
    for gid in 0..glyph_count {
        rebuilt_offsets.push(rebuilt_glyf.len());
        if retained.contains(&gid) {
            rebuilt_glyf.extend_from_slice(glyf.get(offsets[gid]..offsets[gid + 1])?);
            while rebuilt_glyf.len() % alignment != 0 {
                rebuilt_glyf.push(0);
            }
        }
    }
    rebuilt_offsets.push(rebuilt_glyf.len());

    let mut rebuilt_loca = Vec::new();
    match loca_format {
        0 => {
            for offset in rebuilt_offsets {
                if offset % 2 != 0 || offset / 2 > usize::from(u16::MAX) {
                    return None;
                }
                rebuilt_loca.extend_from_slice(&u16::try_from(offset / 2).ok()?.to_be_bytes());
            }
        }
        1 => {
            for offset in rebuilt_offsets {
                rebuilt_loca.extend_from_slice(&u32::try_from(offset).ok()?.to_be_bytes());
            }
        }
        _ => return None,
    }
    let rebuilt = crate::font_subset::replace_glyf_and_loca(bytes, rebuilt_glyf, rebuilt_loca)?;
    let removed = bytes.len().saturating_sub(rebuilt.len());
    (removed > 0).then_some((rebuilt, removed))
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
    flate_level: crate::FlateLevel,
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
        let font = sfnt_for_pdf_rendering(&decoded)
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
        let Some(union_font) = sfnt_union_sparse_glyphs(&fonts) else {
            continue;
        };
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

const FONT_FILE_KEYS: [&[u8]; 2] = [b"FontFile2", b"FontFile3"];

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

fn font_neutral_ext_gstates(
    document: &EditDocument,
    resources: &OwnedDictionary,
) -> Result<BTreeSet<Vec<u8>>> {
    let Some(ext_gstates) =
        resolved_owned_dictionary(document, resources.get(b"ExtGState".as_slice()))?
    else {
        return Ok(BTreeSet::new());
    };
    let mut neutral = BTreeSet::new();
    for (name, value) in &ext_gstates {
        let Some(dictionary) = resolved_owned_dictionary(document, Some(value))? else {
            continue;
        };
        if !dictionary.contains_key(b"Font".as_slice()) {
            neutral.insert(name.clone());
        }
    }
    Ok(neutral)
}

struct FontSelectionState<T: Copy> {
    current: Option<T>,
    known: bool,
    stack: Vec<(Option<T>, bool)>,
}

impl<T: Copy> Default for FontSelectionState<T> {
    fn default() -> Self {
        Self {
            current: None,
            known: false,
            stack: Vec::new(),
        }
    }
}

impl<T: Copy> FontSelectionState<T> {
    const fn select(&mut self, current: Option<T>) {
        self.current = current;
        self.known = true;
    }

    const fn invalidate(&mut self) {
        self.current = None;
        self.known = false;
    }

    fn save(&mut self) {
        self.stack.push((self.current, self.known));
    }

    fn restore(&mut self) {
        if let Some((current, known)) = self.stack.pop() {
            self.current = current;
            self.known = known;
        } else {
            self.invalidate();
        }
    }
}

const fn raw_operand_contains_unescaped_literal_cr(raw: &[u8]) -> bool {
    let mut index = 0_usize;
    let mut literal_depth = 0_usize;
    let mut in_comment = false;
    while index < raw.len() {
        let byte = raw[index];
        if literal_depth == 0 {
            if in_comment {
                if matches!(byte, b'\r' | b'\n') {
                    in_comment = false;
                }
                index += 1;
                continue;
            }
            match byte {
                b'%' => in_comment = true,
                b'(' => literal_depth = 1,
                _ => {}
            }
            index += 1;
            continue;
        }

        match byte {
            b'\\' => {
                index += 1;
                if index >= raw.len() {
                    break;
                }
                if raw[index] == b'\r' {
                    index += 1;
                    if index < raw.len() && raw[index] == b'\n' {
                        index += 1;
                    }
                } else {
                    index += 1;
                }
            }
            b'(' => {
                literal_depth += 1;
                index += 1;
            }
            b')' => {
                literal_depth -= 1;
                index += 1;
            }
            b'\r' => return true,
            _ => index += 1,
        }
    }
    false
}

fn decode_raw_literal_string_preserving_cr(raw: &[u8], start: usize) -> Option<(Vec<u8>, usize)> {
    if raw.get(start) != Some(&b'(') {
        return None;
    }
    let mut output = Vec::new();
    let mut depth = 1_usize;
    let mut index = start + 1;
    while index < raw.len() {
        match raw[index] {
            b'\\' => {
                index += 1;
                let escaped = *raw.get(index)?;
                match escaped {
                    b'n' => output.push(b'\n'),
                    b'r' => output.push(b'\r'),
                    b't' => output.push(b'\t'),
                    b'b' => output.push(0x08),
                    b'f' => output.push(0x0c),
                    b'\r' => {
                        index += 1;
                        if raw.get(index) == Some(&b'\n') {
                            index += 1;
                        }
                        continue;
                    }
                    b'\n' => {
                        index += 1;
                        continue;
                    }
                    b'0'..=b'7' => {
                        let mut value = u16::from(escaped - b'0');
                        let mut digits = 1_u8;
                        while digits < 3 {
                            let Some(next) = raw.get(index + 1).copied() else {
                                break;
                            };
                            if !(b'0'..=b'7').contains(&next) {
                                break;
                            }
                            index += 1;
                            value = value * 8 + u16::from(next - b'0');
                            digits += 1;
                        }
                        output.push(u8::try_from(value).ok()?);
                    }
                    other => output.push(other),
                }
                index += 1;
            }
            b'(' => {
                depth += 1;
                output.push(b'(');
                index += 1;
            }
            b')' => {
                depth -= 1;
                index += 1;
                if depth == 0 {
                    return Some((output, index));
                }
                output.push(b')');
            }
            b'\r' => {
                // A raw CR is the producer ambiguity we are preserving. CRLF is
                // a different-width ambiguity; fail closed instead of guessing
                // how a renderer pairs the following byte in an Identity code.
                if raw.get(index + 1) == Some(&b'\n') {
                    return None;
                }
                output.push(b'\r');
                index += 1;
            }
            other => {
                output.push(other);
                index += 1;
            }
        }
    }
    None
}

fn decode_raw_literal_strings_preserving_cr(raw: &[u8]) -> Option<Vec<Vec<u8>>> {
    let mut strings = Vec::new();
    let mut index = 0_usize;
    let mut in_comment = false;
    while index < raw.len() {
        let byte = raw[index];
        if in_comment {
            if matches!(byte, b'\r' | b'\n') {
                in_comment = false;
            }
            index += 1;
            continue;
        }
        match byte {
            b'%' => {
                in_comment = true;
                index += 1;
            }
            b'(' => {
                let (decoded, next) = decode_raw_literal_string_preserving_cr(raw, index)?;
                strings.push(decoded);
                index = next;
            }
            _ => index += 1,
        }
    }
    Some(strings)
}

struct IdentityCidGlyphScanner<'a> {
    fonts: &'a HashMap<Vec<u8>, CidFontGlyphSpec>,
    font_neutral_ext_gstates: &'a BTreeSet<Vec<u8>>,
    selection: FontSelectionState<&'a CidFontGlyphSpec>,
    used: HashMap<CowObjectHandle, BTreeSet<u16>>,
    unsafe_programs: BTreeSet<CowObjectHandle>,
    unknown_font_text: bool,
}

impl IdentityCidGlyphScanner<'_> {
    fn mark_current_program_unsafe(&mut self) {
        if !self.selection.known {
            self.unknown_font_text = true;
        } else if let Some(font) = self.selection.current {
            self.unsafe_programs.insert(font.program);
        }
    }

    fn record_raw_cr_variants(
        &mut self,
        content: &[u8],
        instruction: &hayro_syntax::content::Instruction<'_, '_>,
        operand_index: usize,
    ) -> bool {
        let Some(raw) = instruction
            .operand_spans()
            .nth(operand_index)
            .and_then(|span| content.get(span))
        else {
            self.mark_current_program_unsafe();
            return false;
        };
        if !raw_operand_contains_unescaped_literal_cr(raw) {
            return true;
        }
        let Some(strings) = decode_raw_literal_strings_preserving_cr(raw) else {
            self.mark_current_program_unsafe();
            return false;
        };
        if strings.is_empty() {
            self.mark_current_program_unsafe();
            return false;
        }
        for bytes in strings {
            self.record_string(&bytes);
        }
        true
    }

    fn record_string(&mut self, bytes: &[u8]) {
        if !self.selection.known {
            self.unknown_font_text = true;
            return;
        }
        let Some(font) = self.selection.current else {
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

    fn instruction(
        &mut self,
        content: &[u8],
        instruction: &hayro_syntax::content::Instruction<'_, '_>,
    ) {
        let mut operands = instruction.operands();
        match &instruction.operator[..] {
            b"q" => self.selection.save(),
            b"Q" => self.selection.restore(),
            b"gs" => {
                let neutral = operands
                    .next()
                    .and_then(crate::content_stream::operand_name)
                    .is_some_and(|name| self.font_neutral_ext_gstates.contains(name));
                if !neutral {
                    self.selection.invalidate();
                }
            }
            b"Tf" => {
                if let Some(name) = operands
                    .next()
                    .and_then(crate::content_stream::operand_name)
                {
                    self.selection.select(self.fonts.get(name));
                } else {
                    self.selection.invalidate();
                }
            }
            b"Tj" | b"'" => {
                if !self.record_raw_cr_variants(content, instruction, 0) {
                    return;
                }
                if let Some(bytes) = operands
                    .next()
                    .and_then(crate::content_stream::operand_string)
                {
                    self.record_string(bytes);
                }
            }
            b"\"" => {
                if !self.record_raw_cr_variants(content, instruction, 2) {
                    return;
                }
                if let Some(bytes) = operands
                    .nth(2)
                    .and_then(crate::content_stream::operand_string)
                {
                    self.record_string(bytes);
                }
            }
            b"TJ" => {
                if !self.record_raw_cr_variants(content, instruction, 0) {
                    return;
                }
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
    font_neutral_ext_gstates: &'a BTreeSet<Vec<u8>>,
    selection: FontSelectionState<CowObjectHandle>,
    used: HashMap<CowObjectHandle, BTreeSet<u8>>,
    unsafe_programs: BTreeSet<CowObjectHandle>,
    unknown_font_text: bool,
}

impl WinAnsiCodeScanner<'_> {
    fn record_string(&mut self, bytes: &[u8]) {
        if !self.selection.known {
            self.unknown_font_text = true;
            return;
        }
        let Some(program) = self.selection.current else {
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
            b"q" => self.selection.save(),
            b"Q" => self.selection.restore(),
            b"gs" => {
                let neutral = operands
                    .next()
                    .and_then(crate::content_stream::operand_name)
                    .is_some_and(|name| self.font_neutral_ext_gstates.contains(name));
                if !neutral {
                    self.selection.invalidate();
                }
            }
            b"Tf" => {
                if let Some(name) = operands
                    .next()
                    .and_then(crate::content_stream::operand_name)
                {
                    self.selection.select(self.fonts.get(name).copied());
                } else {
                    self.selection.invalidate();
                }
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

#[derive(Default)]
struct FontUsageAccumulator {
    identity_used: HashMap<CowObjectHandle, BTreeSet<u16>>,
    identity_unsafe: BTreeSet<CowObjectHandle>,
    winansi_used: HashMap<CowObjectHandle, BTreeSet<u8>>,
    winansi_unsafe: BTreeSet<CowObjectHandle>,
}

impl FontUsageAccumulator {
    fn scan_scope(
        &mut self,
        content: &[u8],
        identity_eligible: &HashMap<Vec<u8>, CidFontGlyphSpec>,
        winansi_eligible: &HashMap<Vec<u8>, CowObjectHandle>,
        font_neutral_ext_gstates: &BTreeSet<Vec<u8>>,
    ) -> bool {
        if identity_eligible.is_empty() && winansi_eligible.is_empty() {
            return false;
        }

        let mut identity = IdentityCidGlyphScanner {
            fonts: identity_eligible,
            font_neutral_ext_gstates,
            selection: FontSelectionState::default(),
            used: HashMap::new(),
            unsafe_programs: BTreeSet::new(),
            unknown_font_text: false,
        };
        let mut winansi = WinAnsiCodeScanner {
            fonts: winansi_eligible,
            font_neutral_ext_gstates,
            selection: FontSelectionState::default(),
            used: HashMap::new(),
            unsafe_programs: BTreeSet::new(),
            unknown_font_text: false,
        };

        let parsed = crate::content_stream::visit_instructions(content, |instruction| {
            if !identity_eligible.is_empty() {
                identity.instruction(content, instruction);
            }
            if !winansi_eligible.is_empty() {
                winansi.instruction(instruction);
            }
            Ok(())
        });
        if !matches!(parsed, Ok(false)) {
            self.identity_unsafe
                .extend(identity_eligible.values().map(|spec| spec.program));
            self.winansi_unsafe
                .extend(winansi_eligible.values().copied());
            return true;
        }

        let unknown_font_text = identity.unknown_font_text || winansi.unknown_font_text;
        self.identity_unsafe.extend(identity.unsafe_programs);
        for (program, gids) in identity.used {
            self.identity_used.entry(program).or_default().extend(gids);
        }
        self.winansi_unsafe.extend(winansi.unsafe_programs);
        for (program, codes) in winansi.used {
            self.winansi_used.entry(program).or_default().extend(codes);
        }
        unknown_font_text
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

fn tiling_pattern_handles(document: &EditDocument) -> Result<Vec<CowObjectHandle>> {
    let mut patterns = Vec::new();
    for handle in document.reachable_streams()? {
        let Some(OwnedObject::Stream { dictionary, .. }) = document.current_owned_object(handle)?
        else {
            continue;
        };
        let Some(pattern_type) = dictionary.get(b"PatternType".as_slice()) else {
            continue;
        };
        if matches!(
            document.resolve_owned_value(pattern_type)?,
            Some(OwnedObject::Integer(1))
        ) {
            patterns.push(handle);
        }
    }
    Ok(patterns)
}

fn collect_type3_font_dictionaries(
    document: &EditDocument,
    object: &OwnedObject,
    fonts: &mut Vec<OwnedDictionary>,
) -> Result<()> {
    match object {
        OwnedObject::Dictionary(dictionary) | OwnedObject::Stream { dictionary, .. } => {
            if owned_name_value(document, dictionary.get(b"Subtype".as_slice()))?.as_deref()
                == Some(b"Type3")
            {
                fonts.push(dictionary.clone());
            }
            for value in dictionary.values() {
                if !matches!(value, OwnedObject::Reference(_)) {
                    collect_type3_font_dictionaries(document, value, fonts)?;
                }
            }
        }
        OwnedObject::Array(values) => {
            for value in values {
                if !matches!(value, OwnedObject::Reference(_)) {
                    collect_type3_font_dictionaries(document, value, fonts)?;
                }
            }
        }
        OwnedObject::Reference(_)
        | OwnedObject::Null
        | OwnedObject::Boolean(_)
        | OwnedObject::Integer(_)
        | OwnedObject::Real(_)
        | OwnedObject::Name(_)
        | OwnedObject::String(_) => {}
    }
    Ok(())
}

fn type3_font_dictionaries(document: &EditDocument) -> Result<Vec<OwnedDictionary>> {
    let mut fonts = Vec::new();
    for handle in document.reachable_output_objects()? {
        let Some(object) = document.current_owned_object(handle)? else {
            continue;
        };
        collect_type3_font_dictionaries(document, &object, &mut fonts)?;
    }
    Ok(fonts)
}

fn poison_font_usage_scope(
    identity_eligible: &HashMap<Vec<u8>, CidFontGlyphSpec>,
    winansi_eligible: &HashMap<Vec<u8>, CowObjectHandle>,
    usage: &mut FontUsageAccumulator,
) {
    usage
        .identity_unsafe
        .extend(identity_eligible.values().map(|spec| spec.program));
    usage
        .winansi_unsafe
        .extend(winansi_eligible.values().copied());
}

fn scan_tiling_pattern_font_usage(
    document: &EditDocument,
    usage: &mut FontUsageAccumulator,
) -> Result<bool> {
    for pattern in tiling_pattern_handles(document)? {
        let Some(OwnedObject::Stream { dictionary, .. }) =
            document.current_owned_object(pattern)?
        else {
            continue;
        };
        let Some(resources_value) = dictionary.get(b"Resources".as_slice()) else {
            return Ok(true);
        };
        let Some(resources) = resolved_owned_dictionary(document, Some(resources_value))? else {
            return Ok(true);
        };
        let identity_eligible =
            eligible_identity_fonts(document, &resources, &mut usage.identity_unsafe)?;
        let winansi_eligible =
            eligible_winansi_fonts(document, &resources, &mut usage.winansi_unsafe)?;
        if identity_eligible.is_empty() && winansi_eligible.is_empty() {
            continue;
        }
        let font_neutral_ext_gstates = font_neutral_ext_gstates(document, &resources)?;
        let Ok(content) = document.decoded_content_stream_data(pattern) else {
            poison_font_usage_scope(&identity_eligible, &winansi_eligible, usage);
            continue;
        };
        if usage.scan_scope(
            &content,
            &identity_eligible,
            &winansi_eligible,
            &font_neutral_ext_gstates,
        ) {
            return Ok(true);
        }
    }
    Ok(false)
}

fn scan_type3_charproc_font_usage(
    document: &EditDocument,
    usage: &mut FontUsageAccumulator,
) -> Result<bool> {
    for font in type3_font_dictionaries(document)? {
        let Some(resources_value) = font.get(b"Resources".as_slice()) else {
            // PDF 1.x permits legacy Type 3 fonts without /Resources to use
            // the resource dictionary of the page on which the font is used.
            // Without a call graph, we cannot attribute that inherited scope.
            return Ok(true);
        };
        let Some(resources) = resolved_owned_dictionary(document, Some(resources_value))? else {
            return Ok(true);
        };
        let identity_eligible =
            eligible_identity_fonts(document, &resources, &mut usage.identity_unsafe)?;
        let winansi_eligible =
            eligible_winansi_fonts(document, &resources, &mut usage.winansi_unsafe)?;
        if identity_eligible.is_empty() && winansi_eligible.is_empty() {
            continue;
        }
        let font_neutral_ext_gstates = font_neutral_ext_gstates(document, &resources)?;
        let Some(char_procs_value) = font.get(b"CharProcs".as_slice()) else {
            poison_font_usage_scope(&identity_eligible, &winansi_eligible, usage);
            continue;
        };
        let Some(char_procs) = resolved_owned_dictionary(document, Some(char_procs_value))? else {
            poison_font_usage_scope(&identity_eligible, &winansi_eligible, usage);
            continue;
        };
        for value in char_procs.values() {
            let Some(stream) = document.resolve_owned_value(value)? else {
                poison_font_usage_scope(&identity_eligible, &winansi_eligible, usage);
                break;
            };
            let Ok(content) = document.decoded_content_stream_value(&stream) else {
                poison_font_usage_scope(&identity_eligible, &winansi_eligible, usage);
                break;
            };
            if usage.scan_scope(
                &content,
                &identity_eligible,
                &winansi_eligible,
                &font_neutral_ext_gstates,
            ) {
                return Ok(true);
            }
        }
    }
    Ok(false)
}

type IdentityGlyphUsage = HashMap<CowObjectHandle, BTreeSet<u16>>;
type WinAnsiCodeUsage = HashMap<CowObjectHandle, BTreeSet<u8>>;
type FontGlyphUsage = (IdentityGlyphUsage, WinAnsiCodeUsage);

fn font_glyph_usage(document: &EditDocument) -> Result<FontGlyphUsage> {
    let mut usage = FontUsageAccumulator::default();

    for page in document.page_handles()? {
        let Some(resources_value) = document.inherited_page_value(page, b"Resources")? else {
            continue;
        };
        let Some(resources) = resolved_owned_dictionary(document, Some(&resources_value))? else {
            continue;
        };
        let identity_eligible =
            eligible_identity_fonts(document, &resources, &mut usage.identity_unsafe)?;
        let winansi_eligible =
            eligible_winansi_fonts(document, &resources, &mut usage.winansi_unsafe)?;
        let font_neutral_ext_gstates = font_neutral_ext_gstates(document, &resources)?;
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
        if usage.scan_scope(
            &content,
            &identity_eligible,
            &winansi_eligible,
            &font_neutral_ext_gstates,
        ) {
            return Ok((HashMap::new(), HashMap::new()));
        }
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
            eligible_identity_fonts(document, &resources, &mut usage.identity_unsafe)?;
        let winansi_eligible =
            eligible_winansi_fonts(document, &resources, &mut usage.winansi_unsafe)?;
        let font_neutral_ext_gstates = font_neutral_ext_gstates(document, &resources)?;
        if identity_eligible.is_empty() && winansi_eligible.is_empty() {
            continue;
        }
        let Ok(content) = document.decoded_content_stream_data(form) else {
            usage
                .identity_unsafe
                .extend(identity_eligible.values().map(|spec| spec.program));
            usage
                .winansi_unsafe
                .extend(winansi_eligible.values().copied());
            continue;
        };
        if usage.scan_scope(
            &content,
            &identity_eligible,
            &winansi_eligible,
            &font_neutral_ext_gstates,
        ) {
            return Ok((HashMap::new(), HashMap::new()));
        }
    }

    if scan_tiling_pattern_font_usage(document, &mut usage)?
        || scan_type3_charproc_font_usage(document, &mut usage)?
    {
        return Ok((HashMap::new(), HashMap::new()));
    }

    for program in &usage.identity_unsafe {
        usage.identity_used.remove(program);
    }
    for program in &usage.winansi_unsafe {
        usage.winansi_used.remove(program);
    }
    Ok((usage.identity_used, usage.winansi_used))
}

fn inspect_source_font_dictionary(
    holder: CowObjectHandle,
    dictionary: &HayroDict<'_>,
    descriptor_usage_edges: &mut Vec<(CowObjectHandle, FontProgramUsage)>,
    descriptor_program_edges: &mut Vec<(CowObjectHandle, CowObjectHandle)>,
) {
    if let Some(descriptor) = dictionary.get_ref(b"FontDescriptor") {
        let subtype = dictionary.get::<HayroName<'_>>(b"Subtype");
        let usage = FontProgramUsage::from_subtype(subtype.as_ref().map(AsRef::<[u8]>::as_ref));
        if usage != FontProgramUsage::default() {
            descriptor_usage_edges.push((CowObjectHandle::Existing(descriptor.into()), usage));
        }
    }

    for key in FONT_FILE_KEYS {
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

    for key in FONT_FILE_KEYS {
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
    program_usage.entry(program).or_default().merge(usage);
}

fn source_dictionary_font_usage(dictionary: &HayroDict<'_>) -> FontProgramUsage {
    FontProgramUsage::from_subtype(
        dictionary
            .get::<HayroName<'_>>(b"Subtype")
            .as_ref()
            .map(AsRef::<[u8]>::as_ref),
    )
}

fn inspect_source_direct_font_dictionary(
    dictionary: &HayroDict<'_>,
    program_usage: &mut HashMap<CowObjectHandle, FontProgramUsage>,
    descriptor_usage_edges: &mut Vec<(CowObjectHandle, FontProgramUsage)>,
) {
    let usage = source_dictionary_font_usage(dictionary);
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
                    for key in FONT_FILE_KEYS {
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
        inspect_source_direct_font_object(&value, program_usage, descriptor_usage_edges);
    }
}

fn inspect_source_direct_font_object(
    object: &HayroObject<'_>,
    program_usage: &mut HashMap<CowObjectHandle, FontProgramUsage>,
    descriptor_usage_edges: &mut Vec<(CowObjectHandle, FontProgramUsage)>,
) {
    match object {
        HayroObject::Dict(dictionary) => {
            inspect_source_direct_font_dictionary(
                dictionary,
                program_usage,
                descriptor_usage_edges,
            );
        }
        HayroObject::Stream(stream) => {
            inspect_source_direct_font_dictionary(
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
                inspect_source_direct_font_object(&value, program_usage, descriptor_usage_edges);
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
    let subtype = owned_name_value(document, dictionary.get(b"Subtype".as_slice()))?;
    Ok(FontProgramUsage::from_subtype(subtype.as_deref()))
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
                for key in FONT_FILE_KEYS {
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

#[derive(Default)]
pub struct FontUsageGraph {
    descriptor_usage: HashMap<CowObjectHandle, FontProgramUsage>,
    descriptor_program_edges: Vec<(CowObjectHandle, CowObjectHandle)>,
    direct_program_usage: HashMap<CowObjectHandle, FontProgramUsage>,
}

impl FontUsageGraph {
    fn indirect_program_usage(&self) -> HashMap<CowObjectHandle, FontProgramUsage> {
        let mut program_usage = HashMap::new();
        for &(descriptor, program) in &self.descriptor_program_edges {
            let Some(usage) = self.descriptor_usage.get(&descriptor).copied() else {
                continue;
            };
            merge_program_usage(&mut program_usage, program, usage);
        }
        program_usage
    }

    fn program_usage(&self) -> HashMap<CowObjectHandle, FontProgramUsage> {
        let mut program_usage = self.indirect_program_usage();
        for (&program, &usage) in &self.direct_program_usage {
            merge_program_usage(&mut program_usage, program, usage);
        }
        program_usage
    }
}

pub fn collect_font_usage_graph(document: &EditDocument) -> Result<FontUsageGraph> {
    let mut descriptor_usage_edges = Vec::new();
    let mut graph = FontUsageGraph::default();
    document.walk_output_objects(|handle, object| match object {
        CurrentObject::Source(object) => {
            match &object {
                HayroObject::Dict(dictionary) => inspect_source_font_dictionary(
                    handle,
                    dictionary,
                    &mut descriptor_usage_edges,
                    &mut graph.descriptor_program_edges,
                ),
                HayroObject::Stream(stream) => inspect_source_font_dictionary(
                    handle,
                    stream.dict(),
                    &mut descriptor_usage_edges,
                    &mut graph.descriptor_program_edges,
                ),
                _ => {}
            }
            inspect_source_direct_font_object(
                &object,
                &mut graph.direct_program_usage,
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
                    &mut graph.descriptor_program_edges,
                )?;
            }
            inspect_owned_direct_font_object(
                document,
                object,
                &mut graph.direct_program_usage,
                &mut descriptor_usage_edges,
            )
        }
    })?;
    for (descriptor, usage) in descriptor_usage_edges {
        graph
            .descriptor_usage
            .entry(descriptor)
            .or_default()
            .merge(usage);
    }
    Ok(graph)
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
    Ok(collect_font_usage_graph(document)?.program_usage())
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
    flate_level: crate::FlateLevel,
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
    flate_level: crate::FlateLevel,
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
    flate_level: crate::FlateLevel,
    graph: &FontUsageGraph,
) -> Result<FontOptimizationStats> {
    let mut program_usage = HashMap::<CowObjectHandle, FontProgramUsage>::new();
    let mut program_descriptors = HashMap::<CowObjectHandle, Vec<CowObjectHandle>>::new();
    let mut union_unsafe_programs = BTreeSet::<CowObjectHandle>::new();
    for &(descriptor, program) in &graph.descriptor_program_edges {
        if union_fontfile2_program(document, descriptor)? == Some(program) {
            program_descriptors
                .entry(program)
                .or_default()
                .push(descriptor);
        } else {
            union_unsafe_programs.insert(program);
        }
        let Some(usage) = graph.descriptor_usage.get(&descriptor).copied() else {
            continue;
        };
        merge_program_usage(&mut program_usage, program, usage);
    }

    union_unsafe_programs.extend(graph.direct_program_usage.keys().copied());
    for (&program, &usage) in &graph.direct_program_usage {
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
/// This pass is intentionally usage-independent so byte-identical programs
/// remain byte-identical for the exact deduplication stage that follows.
pub fn strip_font_editing_tables(
    document: &mut EditDocument,
    flate_level: crate::FlateLevel,
) -> Result<FontOptimizationStats> {
    let graph = collect_font_usage_graph(document)?;
    let program_usage = graph.program_usage();
    if program_usage.is_empty() {
        return Ok(FontOptimizationStats::default());
    }

    let mut ordered_programs = program_usage.keys().copied().collect::<Vec<_>>();
    ordered_programs.sort_unstable();
    let mut stats = FontOptimizationStats::default();
    for program in ordered_programs {
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

        let mut candidate = decoded;
        let unwrapped = single_font_ttc_to_sfnt(&candidate).is_some_and(|unwrapped| {
            candidate = unwrapped;
            true
        });
        let (trimmed, removed_decoded_bytes) =
            if let Some((trimmed, removed)) = sfnt_for_pdf_rendering(&candidate) {
                candidate = trimmed;
                (true, removed)
            } else {
                (false, 0)
            };
        let changed = unwrapped || trimmed;
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
        stats.original_encoded_bytes += raw.len();
        stats.optimized_encoded_bytes += encoded.len();
        stats.decoded_table_bytes_removed += removed_decoded_bytes;
        replace_current_stream_data(document, program, encoded, true)?;
    }

    Ok(stats)
}

const MIN_CID_CMAP_DECODED_BYTES_PER_PROGRAM: usize = 1024;
const MIN_CID_CMAP_DECODED_BYTES_PER_DOCUMENT: usize = 4 * 1024;

fn cid_cmap_program_has_proven_owners(
    graph: &FontUsageGraph,
    incoming_references: &IncomingReferenceOwners,
    program_descriptors: &HashMap<CowObjectHandle, BTreeSet<CowObjectHandle>>,
    program: CowObjectHandle,
) -> bool {
    if graph.direct_program_usage.contains_key(&program)
        || incoming_references.roots.contains(&program)
    {
        return false;
    }
    let Some(descriptors) = program_descriptors.get(&program) else {
        return false;
    };
    descriptors.iter().all(|descriptor| {
        graph
            .descriptor_usage
            .get(descriptor)
            .copied()
            .is_some_and(FontProgramUsage::cidfont_type2_only)
    }) && incoming_references.owners.get(&program) == Some(descriptors)
}

fn cid_cmap_program_descriptors(
    graph: &FontUsageGraph,
) -> HashMap<CowObjectHandle, BTreeSet<CowObjectHandle>> {
    let mut descriptors = HashMap::<CowObjectHandle, BTreeSet<CowObjectHandle>>::new();
    for &(descriptor, program) in &graph.descriptor_program_edges {
        descriptors.entry(program).or_default().insert(descriptor);
    }
    descriptors
}

struct CidCmapStripCandidate {
    program: CowObjectHandle,
    dictionary: OwnedDictionary,
    raw_len: usize,
    cmap_len: usize,
    had_length1: bool,
}

/// Remove the sfnt `cmap` only from programs whose complete current PDF usage
/// is `CIDFontType2`. This is deliberately post-dedup and usage-dependent.
pub fn strip_cidfont_type2_cmaps_after_dedup(
    document: &mut EditDocument,
    flate_level: crate::FlateLevel,
) -> Result<FontOptimizationStats> {
    let graph = collect_font_usage_graph(document)?;
    let program_usage = graph.indirect_program_usage();
    let mut ordered_programs = program_usage.into_iter().collect::<Vec<_>>();
    ordered_programs.sort_unstable_by_key(|(program, _)| *program);
    if !ordered_programs.iter().any(|(program, usage)| {
        usage.cidfont_type2_only() && !graph.direct_program_usage.contains_key(program)
    }) {
        return Ok(FontOptimizationStats::default());
    }

    let incoming_references = incoming_reference_owners(document)?;
    let program_descriptors = cid_cmap_program_descriptors(&graph);
    let mut candidates = Vec::<CidCmapStripCandidate>::new();
    let mut total_cmap_len = 0_usize;
    for (program, usage) in ordered_programs {
        if !usage.cidfont_type2_only()
            || !cid_cmap_program_has_proven_owners(
                &graph,
                &incoming_references,
                &program_descriptors,
                program,
            )
        {
            continue;
        }
        let Some(object) = document.current_owned_object(program)? else {
            continue;
        };
        let OwnedObject::Stream { dictionary, data } = object else {
            continue;
        };
        if !is_unfiltered_or_lone_flate(document, &dictionary)? {
            continue;
        }
        let had_length1 = dictionary.contains_key(b"Length1".as_slice());
        let raw = data.bytes(document.source())?;
        let Ok(decoded) = decode_stream(document, &dictionary, raw.as_ref()) else {
            continue;
        };
        let Some(cmap_len) = crate::font_subset::cidfont_type2_cmap_size(&decoded) else {
            continue;
        };
        if cmap_len < MIN_CID_CMAP_DECODED_BYTES_PER_PROGRAM {
            continue;
        }
        total_cmap_len = total_cmap_len.saturating_add(cmap_len);
        candidates.push(CidCmapStripCandidate {
            program,
            dictionary,
            raw_len: raw.len(),
            cmap_len,
            had_length1,
        });
    }
    if total_cmap_len < MIN_CID_CMAP_DECODED_BYTES_PER_DOCUMENT {
        return Ok(FontOptimizationStats::default());
    }

    let mut stats = FontOptimizationStats::default();
    for info in candidates {
        let Some(OwnedObject::Stream { data, .. }) = document.current_owned_object(info.program)?
        else {
            continue;
        };
        let raw = data.bytes(document.source())?;
        if raw.len() != info.raw_len {
            return Err(Error::Invalid(
                "CIDFontType2 program changed between cmap measurement and rewrite".to_owned(),
            ));
        }
        let Ok(decoded) = decode_stream(document, &info.dictionary, raw.as_ref()) else {
            continue;
        };
        let Some((candidate, removed_decoded_bytes)) =
            crate::font_subset::strip_cidfont_type2_cmap(&decoded)
        else {
            continue;
        };
        if removed_decoded_bytes != info.cmap_len {
            return Err(Error::Invalid(
                "CIDFontType2 cmap measurement changed during rewrite".to_owned(),
            ));
        }
        let Some((encoded, plain_flate)) =
            encode_like_stream(document, &info.dictionary, &candidate, flate_level)?
        else {
            continue;
        };
        if encoded.len() >= info.raw_len {
            continue;
        }
        stats.programs_optimized += 1;
        stats.cid_cmaps_removed += 1;
        stats.cid_cmap_decoded_bytes_removed += removed_decoded_bytes;
        stats.original_encoded_bytes += info.raw_len;
        stats.optimized_encoded_bytes += encoded.len();
        replace_current_stream_data(document, info.program, encoded, plain_flate)?;
        if info.had_length1 {
            set_font_program_length1(document, info.program, candidate.len())?;
        }
    }
    Ok(stats)
}

/// Subset rendering outlines only after exact deduplication and sparse CID
/// union have canonicalized equivalent embedded programs.
///
/// Usage-dependent subsetting before those canonicalization stages can turn
/// byte-identical programs into per-caller variants and lose a much larger
/// deduplication win. At this point every canonical program naturally receives
/// the union of glyph use from all font dictionaries that reference it.
pub fn subset_font_outlines_after_dedup(
    document: &mut EditDocument,
    flate_level: crate::FlateLevel,
    graph: &FontUsageGraph,
) -> Result<FontOptimizationStats> {
    let program_usage = graph.indirect_program_usage();
    if program_usage.is_empty() {
        return Ok(FontOptimizationStats::default());
    }

    let (identity_glyph_usage, _) = font_glyph_usage(document)?;
    if identity_glyph_usage.is_empty() {
        return Ok(FontOptimizationStats::default());
    }

    let mut ordered_programs = program_usage.into_iter().collect::<Vec<_>>();
    ordered_programs.sort_unstable_by_key(|(program, _)| *program);
    let mut stats = FontOptimizationStats::default();

    for (program, usage) in ordered_programs {
        let Some(gids) = identity_glyph_usage.get(&program) else {
            continue;
        };
        if gids.is_empty() {
            continue;
        }

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
        let candidate = single_font_ttc_to_sfnt(&decoded).unwrap_or(decoded);
        let subset = if usage.cidfont_type0_only() {
            crate::cff_cid::subset_cid_font(&candidate, gids)
        } else if usage.cidfont_type2_only() {
            sfnt_blank_unused_glyph_outlines(&candidate, gids)
        } else {
            None
        };
        let Some((mut candidate, glyph_subset_removed_bytes)) = subset else {
            continue;
        };

        let removed_decoded_bytes =
            if let Some((trimmed, removed)) = sfnt_for_pdf_rendering(&candidate) {
                candidate = trimmed;
                removed
            } else {
                0
            };
        let Ok(encoded) = encode_flate(&candidate, flate_level) else {
            continue;
        };
        if encoded.len() >= raw.len() {
            continue;
        }

        stats.programs_optimized += 1;
        stats.programs_glyph_subset += 1;
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

    fn append_pdf_object(pdf: &mut Vec<u8>, offsets: &mut Vec<usize>, body: &[u8]) {
        offsets.push(pdf.len());
        pdf.extend_from_slice(body);
    }

    fn finish_test_pdf(mut pdf: Vec<u8>, offsets: Vec<usize>, root: usize) -> Vec<u8> {
        let size = offsets.len() + 1;
        let xref_offset = pdf.len();
        pdf.extend_from_slice(format!("xref\n0 {size}\n").as_bytes());
        pdf.extend_from_slice(b"0000000000 65535 f \n");
        for offset in offsets {
            pdf.extend_from_slice(format!("{offset:010} 00000 n \n").as_bytes());
        }
        pdf.extend_from_slice(
            format!(
                "trailer\n<< /Size {size} /Root {root} 0 R >>\nstartxref\n{xref_offset}\n%%EOF\n"
            )
            .as_bytes(),
        );
        pdf
    }

    fn pattern_font_usage_fixture() -> Vec<u8> {
        let page_content = b"/Pattern cs /P1 scn 0 0 100 100 re f";
        let pattern_content = b"BT /F1 10 Tf <0002> Tj ET";
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
            b"3 0 obj\n<< /Type /Page /Parent 2 0 R /MediaBox [0 0 100 100] /Resources << /Pattern << /P1 5 0 R >> >> /Contents 4 0 R >>\nendobj\n",
        );
        append_pdf_object(
            &mut pdf,
            &mut offsets,
            format!(
                "4 0 obj\n<< /Length {} >>\nstream\n{}\nendstream\nendobj\n",
                page_content.len(),
                String::from_utf8_lossy(page_content)
            )
            .as_bytes(),
        );
        append_pdf_object(
            &mut pdf,
            &mut offsets,
            format!(
                "5 0 obj\n<< /Type /Pattern /PatternType 1 /PaintType 1 /TilingType 1 /BBox [0 0 10 10] /XStep 10 /YStep 10 /Resources << /Font << /F1 6 0 R >> >> /Length {} >>\nstream\n{}\nendstream\nendobj\n",
                pattern_content.len(),
                String::from_utf8_lossy(pattern_content)
            )
            .as_bytes(),
        );
        append_pdf_object(
            &mut pdf,
            &mut offsets,
            b"6 0 obj\n<< /Type /Font /Subtype /Type0 /BaseFont /PatternCID /Encoding /Identity-H /DescendantFonts [7 0 R] >>\nendobj\n",
        );
        append_pdf_object(
            &mut pdf,
            &mut offsets,
            b"7 0 obj\n<< /Type /Font /Subtype /CIDFontType2 /BaseFont /PatternCID /CIDSystemInfo << /Registry (Adobe) /Ordering (Identity) /Supplement 0 >> /FontDescriptor 8 0 R >>\nendobj\n",
        );
        append_pdf_object(
            &mut pdf,
            &mut offsets,
            b"8 0 obj\n<< /Type /FontDescriptor /FontName /PatternCID /FontFile2 9 0 R >>\nendobj\n",
        );
        append_pdf_object(
            &mut pdf,
            &mut offsets,
            b"9 0 obj\n<< /Length 1 >>\nstream\nx\nendstream\nendobj\n",
        );
        finish_test_pdf(pdf, offsets, 1)
    }

    fn type3_font_usage_fixture(local_resources: bool) -> Vec<u8> {
        let page_content = b"BT /T3 10 Tf (A) Tj ET";
        let glyph_content = b"500 0 d0 BT /F1 10 Tf <0003> Tj ET";
        let page_fonts = if local_resources {
            b"/T3 5 0 R".as_slice()
        } else {
            b"/T3 5 0 R /F1 6 0 R".as_slice()
        };
        let type3_resources = if local_resources {
            b"/Resources << /Font << /F1 6 0 R >> >>".as_slice()
        } else {
            b"".as_slice()
        };

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
            format!(
                "3 0 obj\n<< /Type /Page /Parent 2 0 R /MediaBox [0 0 100 100] /Resources << /Font << {} >> >> /Contents 4 0 R >>\nendobj\n",
                String::from_utf8_lossy(page_fonts)
            )
            .as_bytes(),
        );
        append_pdf_object(
            &mut pdf,
            &mut offsets,
            format!(
                "4 0 obj\n<< /Length {} >>\nstream\n{}\nendstream\nendobj\n",
                page_content.len(),
                String::from_utf8_lossy(page_content)
            )
            .as_bytes(),
        );
        append_pdf_object(
            &mut pdf,
            &mut offsets,
            format!(
                "5 0 obj\n<< /Type /Font /Subtype /Type3 /Name /T3 /FontBBox [0 0 500 500] /FontMatrix [0.001 0 0 0.001 0 0] /CharProcs << /A 10 0 R >> /Encoding << /Type /Encoding /Differences [65 /A] >> /FirstChar 65 /LastChar 65 /Widths [500] {} >>\nendobj\n",
                String::from_utf8_lossy(type3_resources)
            )
            .as_bytes(),
        );
        append_pdf_object(
            &mut pdf,
            &mut offsets,
            b"6 0 obj\n<< /Type /Font /Subtype /Type0 /BaseFont /NestedCID /Encoding /Identity-H /DescendantFonts [7 0 R] >>\nendobj\n",
        );
        append_pdf_object(
            &mut pdf,
            &mut offsets,
            b"7 0 obj\n<< /Type /Font /Subtype /CIDFontType2 /BaseFont /NestedCID /CIDSystemInfo << /Registry (Adobe) /Ordering (Identity) /Supplement 0 >> /FontDescriptor 8 0 R >>\nendobj\n",
        );
        append_pdf_object(
            &mut pdf,
            &mut offsets,
            b"8 0 obj\n<< /Type /FontDescriptor /FontName /NestedCID /FontFile2 9 0 R >>\nendobj\n",
        );
        append_pdf_object(
            &mut pdf,
            &mut offsets,
            b"9 0 obj\n<< /Length 1 >>\nstream\nx\nendstream\nendobj\n",
        );
        append_pdf_object(
            &mut pdf,
            &mut offsets,
            format!(
                "10 0 obj\n<< /Length {} >>\nstream\n{}\nendstream\nendobj\n",
                glyph_content.len(),
                String::from_utf8_lossy(glyph_content)
            )
            .as_bytes(),
        );
        finish_test_pdf(pdf, offsets, 1)
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
        let encoded = crate::stream_codec::encode_flate(&source_font, crate::FlateLevel::SIZE)?;

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
            b"4 0 obj\n<< /Length 25 >>\nstream\nBT /CID 1 Tf <0000> Tj ET\nendstream\nendobj\n",
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
        let encoded = crate::stream_codec::encode_flate(&source_font, crate::FlateLevel::SIZE)?;

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
        let first = dense_compact_cidfont_type2_programs(&mut document, crate::FlateLevel::SIZE)?;
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
        let second = dense_compact_cidfont_type2_programs(&mut reparsed, crate::FlateLevel::SIZE)?;
        assert_eq!(second.programs_dense_remapped, 0);
        assert_eq!(second.cid_to_gid_maps_rewritten, 0);
        Ok(())
    }

    fn cid_cmap_fixture(
        mixed_simple_usage: bool,
        extra_unknown_owner: bool,
        cmap_len: usize,
    ) -> Result<Vec<u8>> {
        let mut head = [0_u8; 54];
        head[50..52].copy_from_slice(&1_i16.to_be_bytes()); // long loca
        let mut maxp = vec![0, 1, 0, 0, 0, 1];
        maxp.resize(32, 0);
        let mut hhea = vec![0_u8; 36];
        hhea[34..36].copy_from_slice(&1_u16.to_be_bytes());
        let hmtx = [0_u8; 4];
        let glyf = [0_u8; 12];
        let loca = [0_u8, 0, 0, 0, 0, 0, 0, 12];
        let cmap_len = u32::try_from(cmap_len)
            .map_err(|_| Error::Invalid("cmap fixture length exceeds u32".to_owned()))?;
        let cmap = (0..cmap_len)
            .map(|index| index.wrapping_mul(73).to_le_bytes()[0] ^ (index >> 3).to_le_bytes()[0])
            .collect::<Vec<_>>();
        let source_font = sfnt(&[
            (*b"head", &head),
            (*b"maxp", &maxp),
            (*b"hhea", &hhea),
            (*b"hmtx", &hmtx),
            (*b"loca", &loca),
            (*b"glyf", &glyf),
            (*b"cmap", cmap.as_slice()),
            (*b"post", b"post-data"),
        ]);
        let encoded = crate::stream_codec::encode_flate(&source_font, crate::FlateLevel::SIZE)?;

        let resources = if mixed_simple_usage {
            b"<< /Font << /CID 5 0 R /Simple 8 0 R >> >>".as_slice()
        } else {
            b"<< /Font << /CID 5 0 R >> >>".as_slice()
        };
        let unknown_program_entry = if extra_unknown_owner {
            b" /UnknownProgram 10 0 R".as_slice()
        } else {
            b"".as_slice()
        };
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
            format!(
                "3 0 obj\n<< /Type /Page /Parent 2 0 R /MediaBox [0 0 10 10] /Resources {} /Contents 4 0 R{} >>\nendobj\n",
                String::from_utf8_lossy(resources),
                String::from_utf8_lossy(unknown_program_entry)
            )
            .as_bytes(),
        );
        append_pdf_object(
            &mut pdf,
            &mut offsets,
            b"4 0 obj\n<< /Length 3 >>\nstream\nq Q\nendstream\nendobj\n",
        );
        append_pdf_object(
            &mut pdf,
            &mut offsets,
            b"5 0 obj\n<< /Type /Font /Subtype /Type0 /BaseFont /CidMapTest /Encoding /Identity-H /DescendantFonts [6 0 R] >>\nendobj\n",
        );
        append_pdf_object(
            &mut pdf,
            &mut offsets,
            b"6 0 obj\n<< /Type /Font /Subtype /CIDFontType2 /BaseFont /CidMapTest /CIDSystemInfo << /Registry (Adobe) /Ordering (Identity) /Supplement 0 >> /FontDescriptor 7 0 R /CIDToGIDMap /Identity >>\nendobj\n",
        );
        append_pdf_object(
            &mut pdf,
            &mut offsets,
            b"7 0 obj\n<< /Type /FontDescriptor /FontName /CidMapTest /FontFile2 10 0 R >>\nendobj\n",
        );
        append_pdf_object(
            &mut pdf,
            &mut offsets,
            b"8 0 obj\n<< /Type /Font /Subtype /TrueType /BaseFont /CidMapTest /FontDescriptor 9 0 R >>\nendobj\n",
        );
        append_pdf_object(
            &mut pdf,
            &mut offsets,
            b"9 0 obj\n<< /Type /FontDescriptor /FontName /CidMapTest /FontFile2 10 0 R >>\nendobj\n",
        );
        let mut font_stream = format!(
            "10 0 obj\n<< /Length {} /Length1 {} /Filter /FlateDecode >>\nstream\n",
            encoded.len(),
            source_font.len()
        )
        .into_bytes();
        font_stream.extend_from_slice(&encoded);
        font_stream.extend_from_slice(b"\nendstream\nendobj\n");
        append_pdf_object(&mut pdf, &mut offsets, &font_stream);
        Ok(finish_test_pdf(pdf, offsets, 1))
    }

    #[test]
    fn cidfont_type2_only_cmap_is_removed_and_idempotent() -> Result<()> {
        let mut document = EditDocument::from_bytes(cid_cmap_fixture(false, false, 8192)?)?;
        let program = CowObjectHandle::Existing(ObjectId::new(10, 0));
        assert!(tags(document.decoded_stream_data(program)?.as_ref()).contains(b"cmap"));

        let first = strip_cidfont_type2_cmaps_after_dedup(&mut document, crate::FlateLevel::SIZE)?;
        assert_eq!(first.programs_optimized, 1);
        assert_eq!(first.cid_cmaps_removed, 1);
        assert_eq!(first.cid_cmap_decoded_bytes_removed, 8192);
        assert!(first.optimized_encoded_bytes < first.original_encoded_bytes);
        let decoded = document.decoded_stream_data(program)?;
        assert!(!tags(decoded.as_ref()).contains(b"cmap"));
        let program_object = document
            .current_owned_object(program)?
            .ok_or_else(|| Error::Invalid("cmap-stripped program vanished".to_owned()))?;
        let length1 = program_object
            .as_dictionary()
            .and_then(|dictionary| dictionary.get(b"Length1".as_slice()))
            .and_then(|value| match value {
                OwnedObject::Integer(value) => Some(*value),
                _ => None,
            })
            .ok_or_else(|| Error::Invalid("cmap-stripped program lost Length1".to_owned()))?;
        assert_eq!(usize::try_from(length1).ok(), Some(decoded.len()));

        let output = document.write_compact()?;
        let mut reparsed = EditDocument::from_bytes(output)?;
        let second = strip_cidfont_type2_cmaps_after_dedup(&mut reparsed, crate::FlateLevel::SIZE)?;
        assert_eq!(second.programs_optimized, 0);
        Ok(())
    }

    #[test]
    fn optimizer_preserves_cmap_when_font_editing_support_is_enabled() -> Result<()> {
        let input = cid_cmap_fixture(false, false, 8192)?;
        let (_, report) = crate::optimize_pdf(&input, &crate::Config::optimize_only())?;
        assert_eq!(report.font_cid_cmaps_removed, 0);
        Ok(())
    }

    #[test]
    fn cmap_is_preserved_when_program_is_shared_with_simple_truetype() -> Result<()> {
        let mut document = EditDocument::from_bytes(cid_cmap_fixture(true, false, 8192)?)?;
        let program = CowObjectHandle::Existing(ObjectId::new(10, 0));
        let first = strip_cidfont_type2_cmaps_after_dedup(&mut document, crate::FlateLevel::SIZE)?;
        assert_eq!(first.programs_optimized, 0);
        assert!(tags(document.decoded_stream_data(program)?.as_ref()).contains(b"cmap"));
        Ok(())
    }

    #[test]
    fn small_cidfont_cmap_opportunity_is_skipped() -> Result<()> {
        let mut document = EditDocument::from_bytes(cid_cmap_fixture(false, false, 1024)?)?;
        let program = CowObjectHandle::Existing(ObjectId::new(10, 0));
        let first = strip_cidfont_type2_cmaps_after_dedup(&mut document, crate::FlateLevel::SIZE)?;
        assert_eq!(first.cid_cmaps_removed, 0);
        assert!(tags(document.decoded_stream_data(program)?.as_ref()).contains(b"cmap"));
        Ok(())
    }

    #[test]
    fn cmap_is_preserved_when_program_has_unproven_owner() -> Result<()> {
        let mut document = EditDocument::from_bytes(cid_cmap_fixture(false, true, 8192)?)?;
        let program = CowObjectHandle::Existing(ObjectId::new(10, 0));
        let first = strip_cidfont_type2_cmaps_after_dedup(&mut document, crate::FlateLevel::SIZE)?;
        assert_eq!(first.cid_cmaps_removed, 0);
        assert!(tags(document.decoded_stream_data(program)?.as_ref()).contains(b"cmap"));
        Ok(())
    }

    #[test]
    fn hayro_direct_cid_descriptor_font_is_optimized() -> Result<()> {
        let input = hayro_direct_cid_font_fixture()?;
        let mut document = EditDocument::from_bytes(input)?;
        let first = strip_font_editing_tables(&mut document, crate::FlateLevel::SIZE)?;
        assert_eq!(first.programs_optimized, 1);
        assert_eq!(first.programs_glyph_subset, 0);
        assert!(first.optimized_encoded_bytes < first.original_encoded_bytes);
        assert_eq!(first.decoded_table_bytes_removed, 8192);

        let output = document.write_compact()?;
        let mut reparsed = EditDocument::from_bytes(output)?;
        let second = strip_font_editing_tables(&mut reparsed, crate::FlateLevel::SIZE)?;
        assert_eq!(second.programs_optimized, 0);
        Ok(())
    }

    #[test]
    fn hayro_font_table_strip_accepts_single_flate_filter_array() -> Result<()> {
        let input = hayro_font_fixture_with_filter_array(true)?;
        let mut document = EditDocument::from_bytes(input)?;
        let first = strip_font_editing_tables(&mut document, crate::FlateLevel::SIZE)?;
        assert_eq!(first.programs_optimized, 1);
        assert!(first.optimized_encoded_bytes < first.original_encoded_bytes);
        assert_eq!(first.decoded_table_bytes_removed, 8192);

        let output = document.write_compact()?;
        let mut reparsed = EditDocument::from_bytes(output)?;
        let second = strip_font_editing_tables(&mut reparsed, crate::FlateLevel::SIZE)?;
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

    #[test]
    fn glyph_usage_scans_text_inside_tiling_patterns() -> Result<()> {
        let document = EditDocument::from_bytes(pattern_font_usage_fixture())?;
        let (identity, winansi) = font_glyph_usage(&document)?;
        let program = CowObjectHandle::Existing(ObjectId::new(9, 0));
        assert_eq!(identity.get(&program), Some(&BTreeSet::from([2_u16])));
        assert!(winansi.is_empty());
        Ok(())
    }

    #[test]
    fn glyph_usage_scans_text_inside_type3_charprocs() -> Result<()> {
        let document = EditDocument::from_bytes(type3_font_usage_fixture(true))?;
        let (identity, winansi) = font_glyph_usage(&document)?;
        let program = CowObjectHandle::Existing(ObjectId::new(9, 0));
        assert_eq!(identity.get(&program), Some(&BTreeSet::from([3_u16])));
        assert!(winansi.is_empty());
        Ok(())
    }

    #[test]
    fn resource_less_type3_font_fails_glyph_usage_closed() -> Result<()> {
        let document = EditDocument::from_bytes(type3_font_usage_fixture(false))?;
        let (identity, winansi) = font_glyph_usage(&document)?;
        assert!(identity.is_empty());
        assert!(winansi.is_empty());
        Ok(())
    }

    #[test]
    fn identity_usage_scanner_restores_font_across_graphics_state() -> Result<()> {
        let first = CowObjectHandle::Existing(ObjectId::new(20, 0));
        let second = CowObjectHandle::Existing(ObjectId::new(21, 0));
        let fonts = HashMap::from([
            (
                b"F1".to_vec(),
                CidFontGlyphSpec {
                    program: first,
                    mapping: CidToGidMapping::Identity,
                },
            ),
            (
                b"F2".to_vec(),
                CidFontGlyphSpec {
                    program: second,
                    mapping: CidToGidMapping::Identity,
                },
            ),
        ]);
        let neutral_ext_gstates = BTreeSet::new();
        let mut scanner = IdentityCidGlyphScanner {
            fonts: &fonts,
            font_neutral_ext_gstates: &neutral_ext_gstates,
            selection: FontSelectionState::default(),
            used: HashMap::new(),
            unsafe_programs: BTreeSet::new(),
            unknown_font_text: false,
        };
        let content = b"/F1 10 Tf <0001> Tj q /F2 10 Tf <0002> Tj Q <0003> Tj";
        let stopped = crate::content_stream::visit_instructions(content, |instruction| {
            scanner.instruction(content, instruction);
            Ok(())
        })?;
        assert!(!stopped);
        assert_eq!(scanner.used.get(&first), Some(&BTreeSet::from([1_u16, 3])));
        assert_eq!(scanner.used.get(&second), Some(&BTreeSet::from([2_u16])));
        assert!(!scanner.unknown_font_text);
        Ok(())
    }

    #[test]
    fn identity_usage_scanner_keeps_font_across_font_neutral_ext_gstate() -> Result<()> {
        let program = CowObjectHandle::Existing(ObjectId::new(20, 0));
        let fonts = HashMap::from([(
            b"F1".to_vec(),
            CidFontGlyphSpec {
                program,
                mapping: CidToGidMapping::Identity,
            },
        )]);
        let neutral_ext_gstates = BTreeSet::from([b"GS1".to_vec()]);
        let mut scanner = IdentityCidGlyphScanner {
            fonts: &fonts,
            font_neutral_ext_gstates: &neutral_ext_gstates,
            selection: FontSelectionState::default(),
            used: HashMap::new(),
            unsafe_programs: BTreeSet::new(),
            unknown_font_text: false,
        };
        let content = b"/F1 10 Tf <0001> Tj /GS1 gs <0002> Tj";
        let stopped = crate::content_stream::visit_instructions(content, |instruction| {
            scanner.instruction(content, instruction);
            Ok(())
        })?;
        assert!(!stopped);
        assert!(!scanner.unknown_font_text);
        assert_eq!(
            scanner.used.get(&program),
            Some(&BTreeSet::from([1_u16, 2]))
        );
        Ok(())
    }

    #[test]
    fn identity_usage_scanner_fails_closed_after_unknown_ext_gstate_font() -> Result<()> {
        let program = CowObjectHandle::Existing(ObjectId::new(20, 0));
        let fonts = HashMap::from([(
            b"F1".to_vec(),
            CidFontGlyphSpec {
                program,
                mapping: CidToGidMapping::Identity,
            },
        )]);
        let neutral_ext_gstates = BTreeSet::new();
        let mut scanner = IdentityCidGlyphScanner {
            fonts: &fonts,
            font_neutral_ext_gstates: &neutral_ext_gstates,
            selection: FontSelectionState::default(),
            used: HashMap::new(),
            unsafe_programs: BTreeSet::new(),
            unknown_font_text: false,
        };
        let content = b"/F1 10 Tf <0001> Tj /GS1 gs <0002> Tj";
        let stopped = crate::content_stream::visit_instructions(content, |instruction| {
            scanner.instruction(content, instruction);
            Ok(())
        })?;
        assert!(!stopped);
        assert!(scanner.unknown_font_text);
        assert_eq!(scanner.used.get(&program), Some(&BTreeSet::from([1_u16])));
        Ok(())
    }

    #[test]
    fn identity_usage_scanner_unions_raw_and_normalized_carriage_return_codes() -> Result<()> {
        let program = CowObjectHandle::Existing(ObjectId::new(20, 0));
        let fonts = HashMap::from([(
            b"F1".to_vec(),
            CidFontGlyphSpec {
                program,
                mapping: CidToGidMapping::Identity,
            },
        )]);
        let neutral_ext_gstates = BTreeSet::new();
        let mut scanner = IdentityCidGlyphScanner {
            fonts: &fonts,
            font_neutral_ext_gstates: &neutral_ext_gstates,
            selection: FontSelectionState::default(),
            used: HashMap::new(),
            unsafe_programs: BTreeSet::new(),
            unknown_font_text: false,
        };
        let content = b"/F1 10 Tf <0001> Tj (\x02\r) Tj <0002> Tj";
        let stopped = crate::content_stream::visit_instructions(content, |instruction| {
            scanner.instruction(content, instruction);
            Ok(())
        })?;
        assert!(!stopped);
        assert_eq!(
            scanner.used.get(&program),
            Some(&BTreeSet::from([1_u16, 2, 0x020a, 0x020d]))
        );
        assert!(!scanner.unsafe_programs.contains(&program));
        Ok(())
    }

    #[test]
    fn identity_usage_scanner_unions_raw_cr_codes_inside_tj_array() -> Result<()> {
        let program = CowObjectHandle::Existing(ObjectId::new(20, 0));
        let fonts = HashMap::from([(
            b"F1".to_vec(),
            CidFontGlyphSpec {
                program,
                mapping: CidToGidMapping::Identity,
            },
        )]);
        let neutral_ext_gstates = BTreeSet::new();
        let mut scanner = IdentityCidGlyphScanner {
            fonts: &fonts,
            font_neutral_ext_gstates: &neutral_ext_gstates,
            selection: FontSelectionState::default(),
            used: HashMap::new(),
            unsafe_programs: BTreeSet::new(),
            unknown_font_text: false,
        };
        let content = b"/F1 10 Tf [<0001> 20 (\x02\r) -10 <0002>] TJ";
        let stopped = crate::content_stream::visit_instructions(content, |instruction| {
            scanner.instruction(content, instruction);
            Ok(())
        })?;
        assert!(!stopped);
        assert_eq!(
            scanner.used.get(&program),
            Some(&BTreeSet::from([1_u16, 2, 0x020a, 0x020d]))
        );
        assert!(!scanner.unsafe_programs.contains(&program));
        Ok(())
    }

    #[test]
    fn identity_usage_scanner_unions_raw_cr_codes_in_double_quote_operator() -> Result<()> {
        let program = CowObjectHandle::Existing(ObjectId::new(20, 0));
        let fonts = HashMap::from([(
            b"F1".to_vec(),
            CidFontGlyphSpec {
                program,
                mapping: CidToGidMapping::Identity,
            },
        )]);
        let neutral_ext_gstates = BTreeSet::new();
        let mut scanner = IdentityCidGlyphScanner {
            fonts: &fonts,
            font_neutral_ext_gstates: &neutral_ext_gstates,
            selection: FontSelectionState::default(),
            used: HashMap::new(),
            unsafe_programs: BTreeSet::new(),
            unknown_font_text: false,
        };
        let content = b"/F1 10 Tf 0 0 (\x02\r) \"";
        let stopped = crate::content_stream::visit_instructions(content, |instruction| {
            scanner.instruction(content, instruction);
            Ok(())
        })?;
        assert!(!stopped);
        assert_eq!(
            scanner.used.get(&program),
            Some(&BTreeSet::from([0x020a_u16, 0x020d]))
        );
        assert!(!scanner.unsafe_programs.contains(&program));
        Ok(())
    }

    #[test]
    fn identity_usage_scanner_fails_closed_on_raw_crlf_literal() -> Result<()> {
        let program = CowObjectHandle::Existing(ObjectId::new(20, 0));
        let fonts = HashMap::from([(
            b"F1".to_vec(),
            CidFontGlyphSpec {
                program,
                mapping: CidToGidMapping::Identity,
            },
        )]);
        let neutral_ext_gstates = BTreeSet::new();
        let mut scanner = IdentityCidGlyphScanner {
            fonts: &fonts,
            font_neutral_ext_gstates: &neutral_ext_gstates,
            selection: FontSelectionState::default(),
            used: HashMap::new(),
            unsafe_programs: BTreeSet::new(),
            unknown_font_text: false,
        };
        let content = b"/F1 10 Tf (\x02\r\n\x03) Tj";
        let stopped = crate::content_stream::visit_instructions(content, |instruction| {
            scanner.instruction(content, instruction);
            Ok(())
        })?;
        assert!(!stopped);
        assert!(scanner.unsafe_programs.contains(&program));
        Ok(())
    }

    #[test]
    fn raw_literal_cr_decoder_preserves_raw_bytes_and_pdf_escapes() {
        let raw = b"[(A\\(B\\)) 10 (\x02\r) (\\101\\n)]";
        assert!(raw_operand_contains_unescaped_literal_cr(raw));
        assert_eq!(
            decode_raw_literal_strings_preserving_cr(raw),
            Some(vec![b"A(B)".to_vec(), b"\x02\r".to_vec(), b"A\n".to_vec()])
        );
        assert!(!raw_operand_contains_unescaped_literal_cr(b"(\\rB)"));
        assert!(!raw_operand_contains_unescaped_literal_cr(b"(A)\r(B)"));
        assert!(!raw_operand_contains_unescaped_literal_cr(b"(A\\\rB)"));
        assert_eq!(decode_raw_literal_strings_preserving_cr(b"(A\r\nB)"), None);
        assert_eq!(decode_raw_literal_strings_preserving_cr(b"(\\777)"), None);
    }

    #[test]
    fn winansi_usage_scanner_restores_font_across_graphics_state() -> Result<()> {
        let first = CowObjectHandle::Existing(ObjectId::new(20, 0));
        let second = CowObjectHandle::Existing(ObjectId::new(21, 0));
        let fonts = HashMap::from([(b"F1".to_vec(), first), (b"F2".to_vec(), second)]);
        let neutral_ext_gstates = BTreeSet::new();
        let mut scanner = WinAnsiCodeScanner {
            fonts: &fonts,
            font_neutral_ext_gstates: &neutral_ext_gstates,
            selection: FontSelectionState::default(),
            used: HashMap::new(),
            unsafe_programs: BTreeSet::new(),
            unknown_font_text: false,
        };
        let stopped = crate::content_stream::visit_instructions(
            b"/F1 10 Tf (A) Tj q /F2 10 Tf (B) Tj Q (C) Tj",
            |instruction| {
                scanner.instruction(instruction);
                Ok(())
            },
        )?;
        assert!(!stopped);
        assert_eq!(scanner.used.get(&first), Some(&BTreeSet::from(*b"AC")));
        assert_eq!(scanner.used.get(&second), Some(&BTreeSet::from(*b"B")));
        assert!(!scanner.unknown_font_text);
        Ok(())
    }

    #[test]
    fn winansi_usage_scanner_fails_closed_after_unknown_ext_gstate_font() -> Result<()> {
        let program = CowObjectHandle::Existing(ObjectId::new(20, 0));
        let fonts = HashMap::from([(b"F1".to_vec(), program)]);
        let neutral_ext_gstates = BTreeSet::new();
        let mut scanner = WinAnsiCodeScanner {
            fonts: &fonts,
            font_neutral_ext_gstates: &neutral_ext_gstates,
            selection: FontSelectionState::default(),
            used: HashMap::new(),
            unsafe_programs: BTreeSet::new(),
            unknown_font_text: false,
        };
        let stopped = crate::content_stream::visit_instructions(
            b"/F1 10 Tf (A) Tj /GS1 gs (B) Tj",
            |instruction| {
                scanner.instruction(instruction);
                Ok(())
            },
        )?;
        assert!(!stopped);
        assert!(scanner.unknown_font_text);
        assert_eq!(scanner.used.get(&program), Some(&BTreeSet::from(*b"A")));
        Ok(())
    }

    fn composite_test_glyph(component_gid: u16) -> Vec<u8> {
        let mut glyph = vec![0_u8; 18];
        glyph[0..2].copy_from_slice(&(-1_i16).to_be_bytes());
        // ARG_1_AND_2_ARE_WORDS; no MORE_COMPONENTS flag.
        glyph[10..12].copy_from_slice(&1_u16.to_be_bytes());
        glyph[12..14].copy_from_slice(&component_gid.to_be_bytes());
        glyph
    }

    #[test]
    fn blank_unused_glyph_outlines_keeps_notdef_composite_dependencies() -> Result<()> {
        use write_fonts::read::{FontRef, TableProvider};

        let component = simple_test_glyph(0x31);
        let source = sparse_union_test_font(
            &[
                Some(composite_test_glyph(3)),
                Some(simple_test_glyph(0x11)),
                Some(simple_test_glyph(0x12)),
                Some(component.clone()),
            ],
            [0, 0, 100, 100],
        )?;
        let Some((pruned, _)) = sfnt_blank_unused_glyph_outlines(&source, &BTreeSet::from([2_u16]))
        else {
            return Err(Error::Invalid(
                "outline blanking should shrink composite fixture".to_owned(),
            ));
        };
        let font = FontRef::new(&pruned)
            .map_err(|_| Error::Invalid("pruned composite fixture should parse".to_owned()))?;
        let offsets = sfnt_glyph_offsets(&pruned)
            .ok_or_else(|| Error::Invalid("pruned composite loca should parse".to_owned()))?;
        let glyf_table = font
            .glyf()
            .map_err(|_| Error::Invalid("pruned composite glyf should parse".to_owned()))?;
        let glyf = glyf_table.offset_data().as_bytes();
        assert_eq!(&glyf[offsets[3]..offsets[4]], component.as_slice());
        Ok(())
    }

    #[test]
    fn blank_unused_glyph_outlines_preserves_gid_space_and_metrics() -> Result<()> {
        use write_fonts::read::{FontRef, TableProvider};

        let glyph0 = simple_test_glyph(0x10);
        let glyph2 = simple_test_glyph(0x12);
        let glyph3 = simple_test_glyph(0x13);
        let source = sparse_union_test_font(
            &[
                Some(glyph0.clone()),
                None,
                Some(glyph2.clone()),
                Some(glyph3),
            ],
            [0, 0, 100, 100],
        )?;
        let Some((pruned, _)) =
            sfnt_blank_unused_glyph_outlines(&source, &BTreeSet::from([1_u16, 2_u16]))
        else {
            return Err(Error::Invalid(
                "outline blanking should shrink fixture".to_owned(),
            ));
        };
        let source_font = FontRef::new(&source)
            .map_err(|_| Error::Invalid("source fixture should parse".to_owned()))?;
        let pruned_font = FontRef::new(&pruned)
            .map_err(|_| Error::Invalid("pruned fixture should parse".to_owned()))?;
        let source_maxp = source_font
            .maxp()
            .map_err(|_| Error::Invalid("source maxp should parse".to_owned()))?;
        let pruned_maxp = pruned_font
            .maxp()
            .map_err(|_| Error::Invalid("pruned maxp should parse".to_owned()))?;
        assert_eq!(source_maxp.num_glyphs(), pruned_maxp.num_glyphs());
        let source_hmtx = source_font
            .hmtx()
            .map_err(|_| Error::Invalid("source hmtx should parse".to_owned()))?;
        let pruned_hmtx = pruned_font
            .hmtx()
            .map_err(|_| Error::Invalid("pruned hmtx should parse".to_owned()))?;
        assert_eq!(
            source_hmtx.offset_data().as_bytes(),
            pruned_hmtx.offset_data().as_bytes()
        );
        let offsets = sfnt_glyph_offsets(&pruned)
            .ok_or_else(|| Error::Invalid("pruned loca should parse".to_owned()))?;
        let pruned_glyf = pruned_font
            .glyf()
            .map_err(|_| Error::Invalid("pruned glyf should parse".to_owned()))?;
        let glyf = pruned_glyf.offset_data().as_bytes();
        assert_eq!(&glyf[offsets[0]..offsets[1]], glyph0.as_slice());
        assert_eq!(&glyf[offsets[1]..offsets[2]], &[] as &[u8]);
        assert_eq!(&glyf[offsets[2]..offsets[3]], glyph2.as_slice());
        assert_eq!(&glyf[offsets[3]..offsets[4]], &[] as &[u8]);
        Ok(())
    }

    fn identity_cid_outline_fixture() -> Result<Vec<u8>> {
        let glyph0 = simple_test_glyph(0x10);
        let glyph2 = simple_test_glyph(0x12);
        let glyph3 = simple_test_glyph(0x13);
        let font = sparse_union_test_font(
            &[Some(glyph0), None, Some(glyph2), Some(glyph3)],
            [0, 0, 100, 100],
        )?;
        let encoded = crate::stream_codec::encode_flate(&font, crate::FlateLevel::SIZE)?;
        let content = b"BT /F1 10 Tf <00010002> Tj ET";

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
            "5 0 obj\n<< /Length {} /Length1 {} /Filter /FlateDecode >>\nstream\n",
            encoded.len(),
            font.len()
        )
        .into_bytes();
        font_stream.extend_from_slice(&encoded);
        font_stream.extend_from_slice(b"\nendstream\nendobj\n");
        append_pdf_object(&mut pdf, &mut offsets, &font_stream);
        append_pdf_object(
            &mut pdf,
            &mut offsets,
            b"6 0 obj\n<< /Type /Font /Subtype /Type0 /BaseFont /OutlineTest /Encoding /Identity-H /DescendantFonts [ << /Type /Font /Subtype /CIDFontType2 /BaseFont /OutlineTest /CIDSystemInfo << /Registry (Adobe) /Ordering (Identity) /Supplement 0 >> /FontDescriptor 7 0 R /DW 1000 /W [1 [500 500]] >> ] >>\nendobj\n",
        );
        append_pdf_object(
            &mut pdf,
            &mut offsets,
            b"7 0 obj\n<< /Type /FontDescriptor /FontName /OutlineTest /Flags 4 /FontBBox [0 0 100 100] /ItalicAngle 0 /Ascent 100 /Descent 0 /CapHeight 100 /StemV 80 /FontFile2 5 0 R >>\nendobj\n",
        );

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

    #[test]
    fn identity_cidfont_outline_blanking_is_wired_and_idempotent() -> Result<()> {
        use write_fonts::read::{FontRef, TableProvider};

        let mut document = EditDocument::from_bytes(identity_cid_outline_fixture()?)?;
        let stripped = strip_font_editing_tables(&mut document, crate::FlateLevel::SIZE)?;
        assert_eq!(stripped.programs_glyph_subset, 0);
        let graph = collect_font_usage_graph(&document)?;
        let first =
            subset_font_outlines_after_dedup(&mut document, crate::FlateLevel::SIZE, &graph)?;
        assert_eq!(first.programs_optimized, 1);
        assert_eq!(first.programs_glyph_subset, 1);
        assert!(first.glyph_subset_decoded_bytes_removed > 0);

        let program = CowObjectHandle::Existing(ObjectId::new(5, 0));
        let decoded = document.decoded_stream_data(program)?;
        let font = FontRef::new(decoded.as_ref())
            .map_err(|_| Error::Invalid("outline-blanked fixture should parse".to_owned()))?;
        let maxp = font
            .maxp()
            .map_err(|_| Error::Invalid("outline-blanked maxp should parse".to_owned()))?;
        assert_eq!(maxp.num_glyphs(), 4);
        let offsets = sfnt_glyph_offsets(decoded.as_ref())
            .ok_or_else(|| Error::Invalid("outline-blanked loca should parse".to_owned()))?;
        let glyf_table = font
            .glyf()
            .map_err(|_| Error::Invalid("outline-blanked glyf should parse".to_owned()))?;
        let glyf = glyf_table.offset_data().as_bytes();
        assert_eq!(&glyf[offsets[1]..offsets[2]], &[] as &[u8]);
        assert_ne!(&glyf[offsets[2]..offsets[3]], &[] as &[u8]);
        assert_eq!(&glyf[offsets[3]..offsets[4]], &[] as &[u8]);

        let output = document.write_compact()?;
        let mut reparsed = EditDocument::from_bytes(output)?;
        let graph = collect_font_usage_graph(&reparsed)?;
        let second =
            subset_font_outlines_after_dedup(&mut reparsed, crate::FlateLevel::SIZE, &graph)?;
        assert_eq!(second.programs_optimized, 0);
        assert_eq!(second.programs_glyph_subset, 0);
        Ok(())
    }

    fn identity_cid_duplicate_program_fixture() -> Result<Vec<u8>> {
        let font = sparse_union_test_font(
            &[
                Some(simple_test_glyph(0x10)),
                Some(simple_test_glyph(0x11)),
                Some(simple_test_glyph(0x12)),
                Some(simple_test_glyph(0x13)),
            ],
            [0, 0, 100, 100],
        )?;
        let encoded = crate::stream_codec::encode_flate(&font, crate::FlateLevel::SIZE)?;
        let contents = [
            b"BT /F1 10 Tf <0001> Tj ET".as_slice(),
            b"BT /F2 10 Tf <0002> Tj ET".as_slice(),
        ];

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
            b"2 0 obj\n<< /Type /Pages /Kids [3 0 R 8 0 R] /Count 2 >>\nendobj\n",
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
                contents[0].len(),
                String::from_utf8_lossy(contents[0])
            )
            .as_bytes(),
        );
        let mut first_stream = format!(
            "5 0 obj\n<< /Length {} /Length1 {} /Filter /FlateDecode >>\nstream\n",
            encoded.len(),
            font.len()
        )
        .into_bytes();
        first_stream.extend_from_slice(&encoded);
        first_stream.extend_from_slice(b"\nendstream\nendobj\n");
        append_pdf_object(&mut pdf, &mut offsets, &first_stream);
        append_pdf_object(
            &mut pdf,
            &mut offsets,
            b"6 0 obj\n<< /Type /Font /Subtype /Type0 /BaseFont /OutlineShared /Encoding /Identity-H /DescendantFonts [ << /Type /Font /Subtype /CIDFontType2 /BaseFont /OutlineShared /CIDSystemInfo << /Registry (Adobe) /Ordering (Identity) /Supplement 0 >> /FontDescriptor 7 0 R /DW 1000 /W [1 [500 500]] >> ] >>\nendobj\n",
        );
        append_pdf_object(
            &mut pdf,
            &mut offsets,
            b"7 0 obj\n<< /Type /FontDescriptor /FontName /OutlineShared /Flags 4 /FontBBox [0 0 100 100] /ItalicAngle 0 /Ascent 100 /Descent 0 /CapHeight 100 /StemV 80 /FontFile2 5 0 R >>\nendobj\n",
        );
        append_pdf_object(
            &mut pdf,
            &mut offsets,
            b"8 0 obj\n<< /Type /Page /Parent 2 0 R /MediaBox [0 0 100 100] /Resources << /Font << /F2 11 0 R >> >> /Contents 9 0 R >>\nendobj\n",
        );
        append_pdf_object(
            &mut pdf,
            &mut offsets,
            format!(
                "9 0 obj\n<< /Length {} >>\nstream\n{}\nendstream\nendobj\n",
                contents[1].len(),
                String::from_utf8_lossy(contents[1])
            )
            .as_bytes(),
        );
        let mut second_stream = format!(
            "10 0 obj\n<< /Length {} /Length1 {} /Filter /FlateDecode >>\nstream\n",
            encoded.len(),
            font.len()
        )
        .into_bytes();
        second_stream.extend_from_slice(&encoded);
        second_stream.extend_from_slice(b"\nendstream\nendobj\n");
        append_pdf_object(&mut pdf, &mut offsets, &second_stream);
        append_pdf_object(
            &mut pdf,
            &mut offsets,
            b"11 0 obj\n<< /Type /Font /Subtype /Type0 /BaseFont /OutlineShared /Encoding /Identity-H /DescendantFonts [ << /Type /Font /Subtype /CIDFontType2 /BaseFont /OutlineShared /CIDSystemInfo << /Registry (Adobe) /Ordering (Identity) /Supplement 0 >> /FontDescriptor 12 0 R /DW 1000 /W [2 [500 500]] >> ] >>\nendobj\n",
        );
        append_pdf_object(
            &mut pdf,
            &mut offsets,
            b"12 0 obj\n<< /Type /FontDescriptor /FontName /OutlineShared /Flags 4 /FontBBox [0 0 100 100] /ItalicAngle 0 /Ascent 100 /Descent 0 /CapHeight 100 /StemV 80 /FontFile2 10 0 R >>\nendobj\n",
        );
        Ok(finish_test_pdf(pdf, offsets, 1))
    }

    #[test]
    fn outline_blanking_preserves_dedup_and_unions_shared_program_usage() -> Result<()> {
        use write_fonts::read::{FontRef, TableProvider};

        let mut document = EditDocument::from_bytes(identity_cid_duplicate_program_fixture()?)?;
        let _ = strip_font_editing_tables(&mut document, crate::FlateLevel::SIZE)?;
        let dedup = crate::dedup::canonicalize_font_program_streams(&mut document)?;
        assert_eq!(dedup.duplicate_streams_detected, 1);
        assert_eq!(dedup.references_canonicalized, 1);

        let first_descriptor = document
            .current_owned_object(CowObjectHandle::Existing(ObjectId::new(7, 0)))?
            .ok_or_else(|| Error::Invalid("first descriptor is missing".to_owned()))?;
        let second_descriptor = document
            .current_owned_object(CowObjectHandle::Existing(ObjectId::new(12, 0)))?
            .ok_or_else(|| Error::Invalid("second descriptor is missing".to_owned()))?;
        let first_program = first_descriptor
            .as_dictionary()
            .and_then(|dictionary| direct_owned_reference(dictionary.get(b"FontFile2".as_slice())));
        let second_program = second_descriptor
            .as_dictionary()
            .and_then(|dictionary| direct_owned_reference(dictionary.get(b"FontFile2".as_slice())));
        assert_eq!(first_program, second_program);
        let program = first_program
            .ok_or_else(|| Error::Invalid("deduplicated font program is missing".to_owned()))?;

        let graph = collect_font_usage_graph(&document)?;
        let subset =
            subset_font_outlines_after_dedup(&mut document, crate::FlateLevel::SIZE, &graph)?;
        assert_eq!(subset.programs_glyph_subset, 1);
        let decoded = document.decoded_stream_data(program)?;
        let font = FontRef::new(decoded.as_ref())
            .map_err(|_| Error::Invalid("deduplicated outline font should parse".to_owned()))?;
        let offsets = sfnt_glyph_offsets(decoded.as_ref())
            .ok_or_else(|| Error::Invalid("deduplicated outline loca should parse".to_owned()))?;
        let glyf_table = font
            .glyf()
            .map_err(|_| Error::Invalid("deduplicated outline glyf should parse".to_owned()))?;
        let glyf = glyf_table.offset_data().as_bytes();
        assert_ne!(&glyf[offsets[1]..offsets[2]], &[] as &[u8]);
        assert_ne!(&glyf[offsets[2]..offsets[3]], &[] as &[u8]);
        assert_eq!(&glyf[offsets[3]..offsets[4]], &[] as &[u8]);
        Ok(())
    }

    #[test]
    fn shared_post_dedup_graph_survives_sparse_union_redirects() -> Result<()> {
        use write_fonts::read::{FontRef, TableProvider};

        let mut document = EditDocument::from_bytes(identity_cid_duplicate_program_fixture()?)?;
        let _ = strip_font_editing_tables(&mut document, crate::FlateLevel::SIZE)?;

        // Turn the two otherwise-identical programs into complementary sparse
        // variants. Sparse union must redirect the second descriptor to the
        // first program; the pre-union usage graph must remain a valid subtype
        // proof for the outline pass that follows. Glyph usage itself is
        // intentionally rescanned after the redirect.
        for (object_number, live_gid) in [(5_i32, 1_u16), (10_i32, 2_u16)] {
            let program = CowObjectHandle::Existing(ObjectId::new(object_number, 0));
            let decoded = document.decoded_stream_data(program)?;
            let Some((sparse, _)) =
                sfnt_blank_unused_glyph_outlines(decoded.as_ref(), &BTreeSet::from([live_gid]))
            else {
                return Err(Error::Invalid(
                    "sparse-union fixture should blank unused outlines".to_owned(),
                ));
            };
            let encoded = crate::stream_codec::encode_flate(&sparse, crate::FlateLevel::SIZE)
                .map_err(|error| Error::Invalid(format!("fixture flate encode failed: {error}")))?;
            replace_current_stream_data(&mut document, program, encoded, true)?;
            set_font_program_length1(&mut document, program, sparse.len())?;
        }

        let dedup = crate::dedup::canonicalize_font_program_streams(&mut document)?;
        assert_eq!(dedup.duplicate_streams_detected, 0);
        let graph = collect_font_usage_graph(&document)?;
        let union = union_sparse_cid_font_programs_after_dedup(
            &mut document,
            crate::FlateLevel::SIZE,
            &graph,
        )?;
        assert_eq!(union.programs_optimized, 2);

        let first_descriptor = document
            .current_owned_object(CowObjectHandle::Existing(ObjectId::new(7, 0)))?
            .ok_or_else(|| Error::Invalid("first descriptor is missing".to_owned()))?;
        let second_descriptor = document
            .current_owned_object(CowObjectHandle::Existing(ObjectId::new(12, 0)))?
            .ok_or_else(|| Error::Invalid("second descriptor is missing".to_owned()))?;
        let first_program = first_descriptor
            .as_dictionary()
            .and_then(|dictionary| direct_owned_reference(dictionary.get(b"FontFile2".as_slice())));
        let second_program = second_descriptor
            .as_dictionary()
            .and_then(|dictionary| direct_owned_reference(dictionary.get(b"FontFile2".as_slice())));
        assert_eq!(first_program, second_program);
        let program = first_program
            .ok_or_else(|| Error::Invalid("sparse-union program is missing".to_owned()))?;

        let _ = subset_font_outlines_after_dedup(&mut document, crate::FlateLevel::SIZE, &graph)?;
        let decoded = document.decoded_stream_data(program)?;
        let font = FontRef::new(decoded.as_ref())
            .map_err(|_| Error::Invalid("sparse-union font should parse".to_owned()))?;
        let offsets = sfnt_glyph_offsets(decoded.as_ref())
            .ok_or_else(|| Error::Invalid("sparse-union loca should parse".to_owned()))?;
        let glyf_table = font
            .glyf()
            .map_err(|_| Error::Invalid("sparse-union glyf should parse".to_owned()))?;
        let glyf = glyf_table.offset_data().as_bytes();
        assert_ne!(&glyf[offsets[1]..offsets[2]], &[] as &[u8]);
        assert_ne!(&glyf[offsets[2]..offsets[3]], &[] as &[u8]);
        assert_eq!(&glyf[offsets[3]..offsets[4]], &[] as &[u8]);
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
        let Some((trimmed, removed)) = sfnt_for_pdf_rendering(&source) else {
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
    fn rendering_sfnt_preserves_required_identity_tables() -> Result<()> {
        let head = [0_u8; 54];
        let source = sfnt(&[
            (*b"head", &head),
            (*b"glyf", b"glyphs"),
            (*b"cmap", b"mapping"),
            (*b"post", b"names"),
            (*b"name", b"editing metadata"),
            (*b"OS/2", b"metrics metadata"),
            (*b"GSUB", b"substitution"),
        ]);
        let Some((trimmed, removed)) = sfnt_for_pdf_rendering(&source) else {
            return Err(Error::Invalid("expected optional table removal".to_owned()));
        };
        let remaining = tags(&trimmed);
        assert!(remaining.contains(b"cmap"));
        assert!(remaining.contains(b"post"));
        assert!(remaining.contains(b"name"));
        assert!(remaining.contains(b"OS/2"));
        assert!(!remaining.contains(b"GSUB"));
        assert_eq!(removed, b"substitution".len());
        Ok(())
    }

    #[test]
    fn rendering_sfnt_is_noop_without_removable_tables() {
        let head = [0_u8; 54];
        let source = sfnt(&[(*b"head", &head), (*b"glyf", b"glyphs")]);
        assert!(sfnt_for_pdf_rendering(&source).is_none());
    }
}
