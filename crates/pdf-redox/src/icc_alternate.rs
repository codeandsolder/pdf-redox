use crate::{EditDocument, ObjectHandle, OwnedDictionary, OwnedObject, Result, StreamData};
use std::collections::{BTreeMap, BTreeSet};

const MIN_ICC_ALTERNATE_ELISION_BYTES: usize = 32 * 1024;

#[derive(Debug, Clone, Default)]
pub struct IccAlternateElisionStats {
    pub profiles_eligible: usize,
    pub profiles_elided: usize,
    pub references_rewritten: usize,
    pub encoded_profile_bytes_elided: usize,
}

const fn direct_name(value: Option<&OwnedObject>) -> Option<&[u8]> {
    match value {
        Some(OwnedObject::Name(name)) => Some(name.as_slice()),
        _ => None,
    }
}

const fn direct_integer(value: Option<&OwnedObject>) -> Option<i64> {
    match value {
        Some(OwnedObject::Integer(value)) => Some(*value),
        _ => None,
    }
}

fn profile_alternate(
    dictionary: &OwnedDictionary,
    encoded_len: usize,
    minimum_encoded_len: usize,
) -> Option<Vec<u8>> {
    if encoded_len < minimum_encoded_len || dictionary.contains_key(b"Range".as_slice()) {
        return None;
    }
    let components = direct_integer(dictionary.get(b"N".as_slice()))?;
    let alternate = direct_name(dictionary.get(b"Alternate".as_slice()))?;
    let expected = match alternate {
        b"DeviceGray" => 1,
        b"DeviceRGB" => 3,
        b"DeviceCMYK" => 4,
        _ => return None,
    };
    (components == expected).then(|| alternate.to_vec())
}

fn rewrite_iccbased_values(
    value: &mut OwnedObject,
    eligible: &BTreeMap<ObjectHandle, Vec<u8>>,
) -> usize {
    match value {
        OwnedObject::Array(values) => {
            if values.len() == 2
                && matches!(&values[0], OwnedObject::Name(name) if name == b"ICCBased")
                && let OwnedObject::Reference(profile) = values[1]
                && let Some(alternate) = eligible.get(&profile)
            {
                *value = OwnedObject::Name(alternate.clone());
                return 1;
            }
            values
                .iter_mut()
                .map(|value| rewrite_iccbased_values(value, eligible))
                .sum()
        }
        OwnedObject::Dictionary(dictionary) | OwnedObject::Stream { dictionary, .. } => dictionary
            .values_mut()
            .map(|value| rewrite_iccbased_values(value, eligible))
            .sum(),
        _ => 0,
    }
}

fn replace_object(
    document: &mut EditDocument,
    handle: ObjectHandle,
    value: OwnedObject,
) -> Result<()> {
    match handle {
        ObjectHandle::Existing(id) => *document.edit_object(id)? = value,
        ObjectHandle::New(id) => *document.edit_added_object(id)? = value,
    }
    Ok(())
}

pub fn elide_icc_profiles_to_alternates_hayro(
    document: &mut EditDocument,
) -> Result<IccAlternateElisionStats> {
    let reachable = document.reachable_output_objects()?;
    let minimum_encoded_len =
        MIN_ICC_ALTERNATE_ELISION_BYTES.max(document.source().bytes().len() / 50);
    let mut eligible = BTreeMap::<ObjectHandle, (Vec<u8>, usize)>::new();
    for &handle in &reachable {
        let Some(OwnedObject::Stream { dictionary, data }) =
            document.current_owned_object(handle)?
        else {
            continue;
        };
        let encoded_len = match data {
            StreamData::Source(id) => document.source().stream_data(id)?.len(),
            StreamData::Owned(bytes) => bytes.len(),
        };
        let Some(alternate) = profile_alternate(&dictionary, encoded_len, minimum_encoded_len)
        else {
            continue;
        };
        eligible.insert(handle, (alternate, encoded_len));
    }

    if eligible.is_empty() {
        return Ok(IccAlternateElisionStats::default());
    }
    let alternates = eligible
        .iter()
        .map(|(&handle, (alternate, _))| (handle, alternate.clone()))
        .collect::<BTreeMap<_, _>>();

    let mut stats = IccAlternateElisionStats {
        profiles_eligible: eligible.len(),
        ..IccAlternateElisionStats::default()
    };
    for handle in reachable {
        let Some(mut snapshot) = document.current_owned_object(handle)? else {
            continue;
        };
        let rewritten = rewrite_iccbased_values(&mut snapshot, &alternates);
        if rewritten == 0 {
            continue;
        }
        replace_object(document, handle, snapshot)?;
        stats.references_rewritten = stats.references_rewritten.saturating_add(rewritten);
    }

    if stats.references_rewritten == 0 {
        return Ok(stats);
    }
    let still_reachable = document
        .reachable_output_objects()?
        .into_iter()
        .collect::<BTreeSet<_>>();
    for (profile, (_, encoded_len)) in eligible {
        if !still_reachable.contains(&profile) {
            stats.profiles_elided = stats.profiles_elided.saturating_add(1);
            stats.encoded_profile_bytes_elided = stats
                .encoded_profile_bytes_elided
                .saturating_add(encoded_len);
        }
    }
    Ok(stats)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn profile_alternate_requires_large_matching_device_space() {
        let mut dictionary = OwnedDictionary::new();
        dictionary.insert(b"N".to_vec(), OwnedObject::Integer(4));
        dictionary.insert(
            b"Alternate".to_vec(),
            OwnedObject::Name(b"DeviceCMYK".to_vec()),
        );
        assert_eq!(
            profile_alternate(
                &dictionary,
                MIN_ICC_ALTERNATE_ELISION_BYTES,
                MIN_ICC_ALTERNATE_ELISION_BYTES,
            ),
            Some(b"DeviceCMYK".to_vec())
        );
        assert_eq!(
            profile_alternate(
                &dictionary,
                MIN_ICC_ALTERNATE_ELISION_BYTES - 1,
                MIN_ICC_ALTERNATE_ELISION_BYTES,
            ),
            None
        );
        dictionary.insert(b"N".to_vec(), OwnedObject::Integer(3));
        assert_eq!(
            profile_alternate(
                &dictionary,
                MIN_ICC_ALTERNATE_ELISION_BYTES,
                MIN_ICC_ALTERNATE_ELISION_BYTES,
            ),
            None
        );
        dictionary.insert(b"N".to_vec(), OwnedObject::Integer(4));
        dictionary.insert(
            b"Range".to_vec(),
            OwnedObject::Array(vec![OwnedObject::Integer(0), OwnedObject::Integer(1)]),
        );
        assert_eq!(
            profile_alternate(
                &dictionary,
                MIN_ICC_ALTERNATE_ELISION_BYTES,
                MIN_ICC_ALTERNATE_ELISION_BYTES,
            ),
            None
        );
    }

    #[test]
    fn rewrite_replaces_only_eligible_iccbased_arrays() {
        let profile = ObjectHandle::Existing(crate::ObjectId::new(7, 0));
        let other = ObjectHandle::Existing(crate::ObjectId::new(8, 0));
        let mut value = OwnedObject::Dictionary(BTreeMap::from([
            (
                b"CS".to_vec(),
                OwnedObject::Array(vec![
                    OwnedObject::Name(b"ICCBased".to_vec()),
                    OwnedObject::Reference(profile),
                ]),
            ),
            (
                b"Other".to_vec(),
                OwnedObject::Array(vec![
                    OwnedObject::Name(b"ICCBased".to_vec()),
                    OwnedObject::Reference(other),
                ]),
            ),
        ]));
        let eligible = BTreeMap::from([(profile, b"DeviceCMYK".to_vec())]);
        assert_eq!(rewrite_iccbased_values(&mut value, &eligible), 1);
        assert!(
            matches!(value, OwnedObject::Dictionary(_)),
            "dictionary expected"
        );
        let OwnedObject::Dictionary(dictionary) = value else {
            return;
        };
        assert_eq!(
            dictionary.get(b"CS".as_slice()),
            Some(&OwnedObject::Name(b"DeviceCMYK".to_vec()))
        );
        assert!(matches!(
            dictionary.get(b"Other".as_slice()),
            Some(OwnedObject::Array(_))
        ));
    }
}
