//! Shared parsing of the graphics-state facts used by content transforms.

use crate::{
    EditDocument, OwnedDictionary, OwnedObject, Result,
    content::{resolved_bool_value, resolved_dictionary, resolved_number_value},
};
use std::collections::BTreeMap;

const STENCIL_RASTER_SUPPORTED_KEYS: &[&[u8]] = &[
    b"Type", b"CA", b"ca", b"BM", b"SMask", b"OP", b"op", b"OPM", b"AIS", b"SA",
];

#[derive(Debug, Clone, Copy, Default)]
pub struct ExtGStateSemantics {
    pub stencil_raster_supported: bool,
    pub stroke_alpha_opaque: Option<bool>,
    pub fill_alpha_opaque: Option<bool>,
    pub normal_blend: Option<bool>,
    pub no_soft_mask: Option<bool>,
    pub stroke_overprint_disabled: Option<bool>,
    pub fill_overprint_disabled: Option<bool>,
}

fn opaque_number(document: &EditDocument, value: Option<&OwnedObject>) -> Result<Option<bool>> {
    let Some(value) = value else {
        return Ok(None);
    };
    Ok(Some(resolved_number_value(document, value)?.is_some_and(
        |value| value.is_finite() && (value - 1.0).abs() <= 1.0e-12,
    )))
}

fn disabled_bool(document: &EditDocument, value: Option<&OwnedObject>) -> Result<Option<bool>> {
    let Some(value) = value else {
        return Ok(None);
    };
    Ok(Some(resolved_bool_value(document, value)? == Some(false)))
}

pub fn ext_gstate_semantics(
    document: &EditDocument,
    resources: &OwnedDictionary,
) -> Result<BTreeMap<Vec<u8>, ExtGStateSemantics>> {
    let Some(states) = resolved_dictionary(document, resources.get(b"ExtGState".as_slice()))?
    else {
        return Ok(BTreeMap::new());
    };
    let mut out = BTreeMap::new();
    for (name, value) in states {
        let Some(state) = resolved_dictionary(document, Some(&value))? else {
            continue;
        };
        let normal_blend = match state.get(b"BM".as_slice()) {
            None => None,
            Some(value) => Some(matches!(
                document.resolve_owned_value(value)?,
                Some(OwnedObject::Name(name)) if name == b"Normal"
            )),
        };
        let no_soft_mask = match state.get(b"SMask".as_slice()) {
            None => None,
            Some(value) => Some(matches!(
                document.resolve_owned_value(value)?,
                Some(OwnedObject::Name(name)) if name == b"None"
            )),
        };
        let supported_keys = state
            .keys()
            .all(|key| STENCIL_RASTER_SUPPORTED_KEYS.contains(&key.as_slice()));
        let alpha_is_shape_safe = match state.get(b"AIS".as_slice()) {
            None => true,
            Some(value) => resolved_bool_value(document, value)? == Some(false),
        };
        // Stroke adjustment is a device-space rendering hint. Either boolean value is
        // acceptable when the caller is explicitly freezing vector paint into a raster;
        // reject only malformed/non-boolean values.
        let stroke_adjust_supported = match state.get(b"SA".as_slice()) {
            None => true,
            Some(value) => resolved_bool_value(document, value)?.is_some(),
        };
        out.insert(
            name,
            ExtGStateSemantics {
                stencil_raster_supported: supported_keys
                    && alpha_is_shape_safe
                    && stroke_adjust_supported,
                stroke_alpha_opaque: opaque_number(document, state.get(b"CA".as_slice()))?,
                fill_alpha_opaque: opaque_number(document, state.get(b"ca".as_slice()))?,
                normal_blend,
                no_soft_mask,
                stroke_overprint_disabled: disabled_bool(document, state.get(b"OP".as_slice()))?,
                fill_overprint_disabled: disabled_bool(document, state.get(b"op".as_slice()))?,
            },
        );
    }
    Ok(out)
}
