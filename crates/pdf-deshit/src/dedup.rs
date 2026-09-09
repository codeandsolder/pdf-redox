use crate::Result;
use flpdf::{ObjectHandle, ObjectRef, Pdf};
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::io::{Read, Seek};

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) struct MetadataDedupStats {
    pub duplicate_streams_detected: usize,
    pub duplicate_raw_bytes: usize,
    pub references_canonicalized: usize,
}

fn metadata_fingerprint(object: &ObjectHandle) -> Result<Option<[u8; 32]>> {
    let Some(dict) = object.as_stream_dict() else {
        return Ok(None);
    };
    let type_object = dict.try_get_key(b"/Type")?;
    if !type_object.try_is_name_and_equals(b"Metadata")? {
        return Ok(None);
    }

    // Require byte-identical encoded payload and an identical resolved stream
    // dictionary. This deliberately refuses broader "same decoded XML"
    // equivalence: differing filters, decode parameters, or metadata stream
    // attributes stay as separate objects.
    let raw = object.get_raw_stream_data()?;
    let dictionary = dict.unparse_resolved();
    let mut hasher = Sha256::new();
    hasher.update((raw.len() as u64).to_le_bytes());
    hasher.update(raw.as_ref());
    hasher.update((dictionary.len() as u64).to_le_bytes());
    hasher.update(dictionary);
    Ok(Some(hasher.finalize().into()))
}

pub(crate) fn canonicalize_metadata_streams<R: Read + Seek + 'static>(
    pdf: &mut Pdf<R>,
) -> Result<MetadataDedupStats> {
    let objects = pdf.get_all_objects()?;
    let mut canonical_by_fingerprint: HashMap<[u8; 32], ObjectRef> = HashMap::new();
    let mut redirects: HashMap<ObjectRef, ObjectRef> = HashMap::new();
    let mut duplicate_raw_bytes = 0_usize;

    for object in &objects {
        let Some(object_ref) = object.object_ref() else {
            continue;
        };
        let Some(fingerprint) = metadata_fingerprint(object)? else {
            continue;
        };
        if let Some(canonical_ref) = canonical_by_fingerprint.get(&fingerprint).copied() {
            redirects.insert(object_ref, canonical_ref);
            duplicate_raw_bytes += object.get_raw_stream_data()?.len();
        } else {
            canonical_by_fingerprint.insert(fingerprint, object_ref);
        }
    }

    let mut references_canonicalized = 0_usize;
    for object in &objects {
        let dict = if let Some(dict) = object.as_stream_dict() {
            dict
        } else if object.try_is_dictionary()? {
            object.clone()
        } else {
            continue;
        };
        let metadata = dict.try_get_key(b"/Metadata")?;
        let Some(metadata_ref) = metadata.object_ref() else {
            continue;
        };
        let Some(canonical_ref) = redirects.get(&metadata_ref).copied() else {
            continue;
        };

        dict.replace_key(b"/Metadata", pdf.get_object_handle(canonical_ref))?;
        pdf.mark_object_handle_dirty(&dict)?;
        references_canonicalized += 1;
    }

    Ok(MetadataDedupStats {
        duplicate_streams_detected: redirects.len(),
        duplicate_raw_bytes,
        references_canonicalized,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Error;
    use std::rc::Rc;

    fn metadata_stream(
        pdf: &mut Pdf<std::io::Cursor<Vec<u8>>>,
        data: &[u8],
    ) -> Result<ObjectHandle> {
        let stream = pdf.new_stream_with_data(Rc::new(data.to_vec()))?;
        let dict = stream
            .as_stream_dict()
            .ok_or_else(|| Error::Invalid("new metadata stream has no dictionary".to_owned()))?;
        dict.replace_key(b"/Type", ObjectHandle::name(b"Metadata".to_vec()))?;
        dict.replace_key(b"/Subtype", ObjectHandle::name(b"XML".to_vec()))?;
        pdf.mark_object_handle_dirty(&dict)?;
        Ok(stream)
    }

    fn holder(
        pdf: &mut Pdf<std::io::Cursor<Vec<u8>>>,
        metadata: ObjectHandle,
    ) -> Result<ObjectHandle> {
        Ok(
            pdf.make_indirect_object_handle(ObjectHandle::dictionary(vec![(
                b"/Metadata".to_vec(),
                metadata,
            )]))?,
        )
    }

    #[test]
    fn canonicalizes_only_exact_duplicate_metadata_streams() -> Result<()> {
        let mut pdf = Pdf::empty()?;
        let first = metadata_stream(&mut pdf, b"<x:xmpmeta>same</x:xmpmeta>")?;
        let second = metadata_stream(&mut pdf, b"<x:xmpmeta>same</x:xmpmeta>")?;
        let different_dict = metadata_stream(&mut pdf, b"<x:xmpmeta>same</x:xmpmeta>")?;
        let different_dict_handle = different_dict
            .as_stream_dict()
            .ok_or_else(|| Error::Invalid("metadata stream has no dictionary".to_owned()))?;
        different_dict_handle.replace_key(b"/Custom", ObjectHandle::integer(1))?;
        pdf.mark_object_handle_dirty(&different_dict_handle)?;

        let first_holder = holder(&mut pdf, first)?;
        let second_holder = holder(&mut pdf, second)?;
        let different_holder = holder(&mut pdf, different_dict)?;
        let root = pdf.root_handle()?;
        root.replace_key(b"/TestMetadataA", first_holder.clone())?;
        root.replace_key(b"/TestMetadataB", second_holder.clone())?;
        root.replace_key(b"/TestMetadataC", different_holder.clone())?;
        pdf.mark_object_handle_dirty(&root)?;

        let stats = canonicalize_metadata_streams(&mut pdf)?;
        assert_eq!(stats.duplicate_streams_detected, 1);
        assert_eq!(stats.references_canonicalized, 1);

        let first_ref = first_holder.try_get_key(b"/Metadata")?.object_ref();
        let second_ref = second_holder.try_get_key(b"/Metadata")?.object_ref();
        let different_ref = different_holder.try_get_key(b"/Metadata")?.object_ref();
        assert_eq!(first_ref, second_ref);
        assert_ne!(first_ref, different_ref);
        Ok(())
    }
}
