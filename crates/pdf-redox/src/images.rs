use crate::{
    EditDocument, Error, ObjectHandle, OwnedDictionary, OwnedObject, Result, StreamData,
    content::resolved_number,
    paper_indexed,
    stream_codec::{decode_image_stream, decode_stream, encode_flate, is_unfiltered_or_lone_flate},
};
use fast_image_resize::{
    FilterType, PixelType, ResizeAlg, ResizeOptions, Resizer, images::Image as ResizeImage,
};
use libjpeg_turbo_rs::{
    PixelFormat, Subsampling, compress,
    encode::tables::{
        STD_CHROMINANCE_QUANT_TABLE, STD_LUMINANCE_QUANT_TABLE, quality_scale_quant_table,
    },
    read_coefficients,
};
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet, VecDeque};

const LOCAL_PALETTE_LAYER_OVERHEAD_BYTES: usize = 512;
const LOCAL_PALETTE_FORM_OVERHEAD_BYTES: usize = 256;
const LOCAL_PALETTE_ALLOWED_SOURCE_KEYS: [&[u8]; 10] = [
    b"Type",
    b"Subtype",
    b"Width",
    b"Height",
    b"ColorSpace",
    b"BitsPerComponent",
    b"Filter",
    b"DecodeParms",
    b"SMask",
    b"Length",
];
const OPAQUE_SMASK_ALLOWED_KEYS: [&[u8]; 9] = [
    b"Type",
    b"Subtype",
    b"Width",
    b"Height",
    b"ColorSpace",
    b"BitsPerComponent",
    b"Filter",
    b"DecodeParms",
    b"Length",
];

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ImageResizeEncoding {
    Jpeg,
    Flate,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ImageResizeTarget {
    pub width: u32,
    pub height: u32,
    pub encoding: ImageResizeEncoding,
}

impl ImageResizeTarget {
    pub const fn jpeg(width: u32, height: u32) -> Self {
        Self {
            width,
            height,
            encoding: ImageResizeEncoding::Jpeg,
        }
    }

    pub const fn flate(width: u32, height: u32) -> Self {
        Self {
            width,
            height,
            encoding: ImageResizeEncoding::Flate,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ImageOptimizationOptions {
    pub min_width: u32,
    pub min_height: u32,
    pub min_area: u32,
    pub inline_min_bytes: usize,
    pub keep_inline_images: bool,
    pub jpeg_quality: u8,
    pub min_savings_bytes: u64,
    pub min_savings_percent: u8,
}

impl Default for ImageOptimizationOptions {
    fn default() -> Self {
        Self {
            min_width: 128,
            min_height: 128,
            min_area: 16_384,
            inline_min_bytes: 1_024,
            keep_inline_images: false,
            jpeg_quality: 75,
            min_savings_bytes: 1,
            min_savings_percent: 0,
        }
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ImageOptimizationStats {
    pub images_optimized: usize,
    pub paper_indexed_images: usize,
    pub paper_indexed_layered_images: usize,
    pub paper_indexed_residual_images: usize,
    pub images_resized: usize,
    pub jpeg_images_resized: usize,
    pub flate_images_resized: usize,
    pub original_encoded_bytes: u64,
    pub optimized_encoded_bytes: u64,
    pub original_pixels: u64,
    pub optimized_pixels: u64,
    pub references_reused: usize,
}

impl ImageOptimizationStats {
    pub const fn saved_bytes(self) -> u64 {
        self.original_encoded_bytes
            .saturating_sub(self.optimized_encoded_bytes)
    }
}

#[derive(Debug)]
struct LayeredIndexedLayerTransform {
    encoded: Vec<u8>,
    palette: Vec<u8>,
}

#[derive(Debug)]
struct ResidualIndexedTransform {
    base_encoded: Vec<u8>,
    base_palette: Vec<u8>,
    residual_encoded: Vec<u8>,
    residual_palette: Vec<u8>,
    base_color_space: OwnedObject,
}

#[derive(Debug)]
enum LayeredIndexedTransform {
    Local {
        layers: Vec<LayeredIndexedLayerTransform>,
        base_color_space: OwnedObject,
    },
    Residual(ResidualIndexedTransform),
}

#[derive(Debug)]
struct ImageTransform {
    encoded: Vec<u8>,
    width: u32,
    height: u32,
    filter: Vec<u8>,
    color_space: Option<OwnedObject>,
    layered_indexed: Option<LayeredIndexedTransform>,
    original_encoded_bytes: u64,
    optimized_encoded_bytes: u64,
    original_pixels: u64,
    optimized_pixels: u64,
    resized: bool,
    paper_indexed: bool,
    encoding: ImageResizeEncoding,
}

pub fn encode_jpeg_raster(
    width: u32,
    height: u32,
    components: usize,
    data: &[u8],
    jpeg_quality: u8,
) -> Result<Vec<u8>> {
    let pixel_format = match components {
        1 => PixelFormat::Grayscale,
        3 => PixelFormat::Rgb,
        4 => PixelFormat::Cmyk,
        _ => {
            return Err(Error::Invalid(format!(
                "JPEG raster encoding supports 1, 3, or 4 components, got {components}"
            )));
        }
    };
    let width_usize = usize::try_from(width)
        .map_err(|_| Error::Invalid("JPEG width exceeds usize".to_owned()))?;
    let height_usize = usize::try_from(height)
        .map_err(|_| Error::Invalid("JPEG height exceeds usize".to_owned()))?;
    let expected = width_usize
        .checked_mul(height_usize)
        .and_then(|pixels| pixels.checked_mul(components))
        .ok_or_else(|| Error::Invalid("JPEG raster dimensions overflow".to_owned()))?;
    if data.len() != expected {
        return Err(Error::Invalid(format!(
            "JPEG raster buffer length {} does not match expected {expected}",
            data.len()
        )));
    }
    let subsampling = if pixel_format == PixelFormat::Cmyk {
        Subsampling::S444
    } else {
        Subsampling::S420
    };
    compress(
        data,
        width_usize,
        height_usize,
        pixel_format,
        jpeg_quality.clamp(1, 100),
        subsampling,
    )
    .map_err(|error| Error::Invalid(format!("JPEG encode failed: {error}")))
}

fn is_image(document: &EditDocument, handle: ObjectHandle) -> Result<bool> {
    let Some(object) = document.current_owned_object(handle)? else {
        return Ok(false);
    };
    let Some(dictionary) = object.as_dictionary() else {
        return Ok(false);
    };
    let Some(subtype) = dictionary.get(b"Subtype".as_slice()) else {
        return Ok(false);
    };
    Ok(
        matches!(document.resolve_owned_value(subtype)?, Some(OwnedObject::Name(name)) if name == b"Image")
            && matches!(object, OwnedObject::Stream { .. }),
    )
}

fn object_subtype(document: &EditDocument, handle: ObjectHandle) -> Result<Option<Vec<u8>>> {
    let Some(object) = document.current_owned_object(handle)? else {
        return Ok(None);
    };
    let Some(dictionary) = object.as_dictionary() else {
        return Ok(None);
    };
    let Some(subtype) = dictionary.get(b"Subtype".as_slice()) else {
        return Ok(None);
    };
    Ok(match document.resolve_owned_value(subtype)? {
        Some(OwnedObject::Name(name)) => Some(name),
        _ => None,
    })
}

fn xobject_bindings(
    document: &EditDocument,
    resources: Option<OwnedObject>,
) -> Result<BTreeMap<Vec<u8>, ObjectHandle>> {
    let Some(resources) = resources else {
        return Ok(BTreeMap::new());
    };
    let Some(resources) = document.resolve_owned_value(&resources)? else {
        return Ok(BTreeMap::new());
    };
    let Some(resources) = resources.as_dictionary() else {
        return Ok(BTreeMap::new());
    };
    let Some(xobjects) = resources.get(b"XObject".as_slice()) else {
        return Ok(BTreeMap::new());
    };
    let Some(xobjects) = document.resolve_owned_value(xobjects)? else {
        return Ok(BTreeMap::new());
    };
    let Some(xobjects) = xobjects.as_dictionary() else {
        return Ok(BTreeMap::new());
    };
    Ok(xobjects
        .iter()
        .filter_map(|(name, value)| match value {
            OwnedObject::Reference(handle) => Some((name.clone(), *handle)),
            _ => None,
        })
        .collect())
}

fn accumulate_xobject_bindings(
    document: &EditDocument,
    xobjects: &BTreeMap<Vec<u8>, ObjectHandle>,
    counts: &mut BTreeMap<ObjectHandle, usize>,
    forms: &mut VecDeque<(ObjectHandle, BTreeMap<Vec<u8>, ObjectHandle>)>,
) -> Result<()> {
    for handle in xobjects.values().copied() {
        match object_subtype(document, handle)?.as_deref() {
            Some(b"Image") => *counts.entry(handle).or_default() += 1,
            Some(b"Form") => forms.push_back((handle, xobjects.clone())),
            _ => {}
        }
    }
    Ok(())
}

fn image_binding_counts(document: &EditDocument) -> Result<BTreeMap<ObjectHandle, usize>> {
    let mut counts = BTreeMap::new();
    for page in document.page_handles()? {
        let page_xobjects =
            xobject_bindings(document, document.inherited_page_value(page, b"Resources")?)?;
        let mut forms = VecDeque::new();
        accumulate_xobject_bindings(document, &page_xobjects, &mut counts, &mut forms)?;
        let mut seen_forms = HashSet::new();
        while let Some((form, inherited_xobjects)) = forms.pop_front() {
            if !seen_forms.insert(form) {
                continue;
            }
            let Some(object) = document.current_owned_object(form)? else {
                continue;
            };
            let Some(dictionary) = object.as_dictionary() else {
                continue;
            };
            let xobjects = if let Some(resources) = dictionary.get(b"Resources".as_slice()) {
                xobject_bindings(document, Some(resources.clone()))?
            } else {
                inherited_xobjects
            };
            accumulate_xobject_bindings(document, &xobjects, &mut counts, &mut forms)?;
        }
    }
    Ok(counts)
}

#[expect(
    clippy::cast_possible_truncation,
    reason = "PDF dimensions are clamped to the complete u32 range before conversion"
)]
#[expect(
    clippy::cast_sign_loss,
    reason = "PDF dimensions are clamped to the nonnegative u32 range before conversion"
)]
fn dimension(
    document: &EditDocument,
    dictionary: &OwnedDictionary,
    key: &[u8],
) -> Result<Option<u32>> {
    let Some(value) = resolved_number(document, dictionary.get(key))? else {
        return Ok(None);
    };
    if value.is_nan() {
        return Ok(Some(0));
    }
    Ok(Some(value.clamp(0.0, f64::from(u32::MAX)) as u32))
}

fn direct_resource_owner(object: &OwnedObject) -> bool {
    let Some(dictionary) = object.as_dictionary() else {
        return false;
    };
    matches!(
        dictionary.get(b"Type".as_slice()),
        Some(OwnedObject::Name(name)) if matches!(name.as_slice(), b"Page" | b"Pages")
    ) || matches!(
        dictionary.get(b"Subtype".as_slice()),
        Some(OwnedObject::Name(name)) if name.as_slice() == b"Form"
    )
}

fn collect_resource_references(resources: &OwnedDictionary, out: &mut HashSet<ObjectHandle>) {
    for (key, value) in resources {
        if key.as_slice() == b"XObject"
            && let OwnedObject::Dictionary(xobjects) = value
        {
            for entry in xobjects.values() {
                if !matches!(entry, OwnedObject::Reference(_)) {
                    collect_non_xobject_references(entry, false, out);
                }
            }
            continue;
        }
        collect_non_xobject_references(value, false, out);
    }
}

fn collect_non_xobject_references(
    value: &OwnedObject,
    allow_direct_resources: bool,
    out: &mut HashSet<ObjectHandle>,
) {
    match value {
        OwnedObject::Reference(handle) => {
            out.insert(*handle);
        }
        OwnedObject::Array(values) => {
            for value in values {
                collect_non_xobject_references(value, false, out);
            }
        }
        OwnedObject::Dictionary(dictionary) | OwnedObject::Stream { dictionary, .. } => {
            for (key, value) in dictionary {
                if allow_direct_resources
                    && key.as_slice() == b"Resources"
                    && let OwnedObject::Dictionary(resources) = value
                {
                    collect_resource_references(resources, out);
                    continue;
                }
                collect_non_xobject_references(value, false, out);
            }
        }
        _ => {}
    }
}

fn non_xobject_reference_targets(document: &EditDocument) -> Result<HashSet<ObjectHandle>> {
    let mut unsafe_targets = document.output_roots().into_iter().collect::<HashSet<_>>();
    for handle in document.reachable_output_objects()? {
        let Some(object) = document.current_owned_object(handle)? else {
            continue;
        };
        let allow_direct_resources = direct_resource_owner(&object);
        collect_non_xobject_references(&object, allow_direct_resources, &mut unsafe_targets);
    }
    Ok(unsafe_targets)
}

fn resolved_name(document: &EditDocument, value: Option<&OwnedObject>) -> Result<Option<Vec<u8>>> {
    let Some(value) = value else {
        return Ok(None);
    };
    Ok(match document.resolve_owned_value(value)? {
        Some(OwnedObject::Name(name)) => Some(name),
        _ => None,
    })
}

fn bpc_is_8(document: &EditDocument, dictionary: &OwnedDictionary) -> Result<bool> {
    Ok(matches!(
        document.resolve_owned_value(
            dictionary
                .get(b"BitsPerComponent".as_slice())
                .unwrap_or(&OwnedObject::Null)
        )?,
        Some(OwnedObject::Integer(8))
    ))
}

fn color_info(
    document: &EditDocument,
    dictionary: &OwnedDictionary,
) -> Result<Option<(usize, PixelFormat, PixelType)>> {
    let Some(name) = resolved_name(document, dictionary.get(b"ColorSpace".as_slice()))? else {
        return Ok(None);
    };
    Ok(match name.as_slice() {
        b"DeviceGray" => Some((1, PixelFormat::Grayscale, PixelType::U8)),
        b"DeviceRGB" => Some((3, PixelFormat::Rgb, PixelType::U8x3)),
        b"DeviceCMYK" => Some((4, PixelFormat::Cmyk, PixelType::U8x4)),
        _ => None,
    })
}

fn indexed_rgb_base_color_space(
    document: &EditDocument,
    dictionary: &OwnedDictionary,
) -> Result<Option<OwnedObject>> {
    let Some(source) = dictionary.get(b"ColorSpace".as_slice()) else {
        return Ok(None);
    };
    let Some(resolved_source) = document.resolve_owned_value(source)? else {
        return Ok(None);
    };
    if matches!(&resolved_source, OwnedObject::Name(name) if name.as_slice() == b"DeviceRGB") {
        return Ok(Some(source.clone()));
    }
    let OwnedObject::Array(values) = resolved_source else {
        return Ok(None);
    };
    if values.len() != 2 {
        return Ok(None);
    }
    let Some(OwnedObject::Name(kind)) = document.resolve_owned_value(&values[0])? else {
        return Ok(None);
    };
    if kind.as_slice() != b"ICCBased" {
        return Ok(None);
    }
    let Some(profile_value) = values.get(1) else {
        return Ok(None);
    };
    let Some(OwnedObject::Stream {
        dictionary: profile_dictionary,
        data,
    }) = document.resolve_owned_value(profile_value)?
    else {
        return Ok(None);
    };
    if profile_dictionary.contains_key(b"Range".as_slice())
        || resolved_number(document, profile_dictionary.get(b"N".as_slice()))? != Some(3.0)
    {
        return Ok(None);
    }
    let encoded = data.bytes(document.source())?;
    let Ok(profile) = decode_stream(document, &profile_dictionary, encoded.as_ref()) else {
        return Ok(None);
    };
    if profile.len() < 40
        || profile.get(16..20) != Some(b"RGB ".as_slice())
        || profile.get(36..40) != Some(b"acsp".as_slice())
    {
        return Ok(None);
    }
    Ok(Some(source.clone()))
}

fn is_non_null(document: &EditDocument, value: Option<&OwnedObject>) -> Result<bool> {
    let Some(value) = value else {
        return Ok(false);
    };
    Ok(!matches!(
        document.resolve_owned_value(value)?,
        Some(OwnedObject::Null) | None
    ))
}

fn source_filter(document: &EditDocument, dictionary: &OwnedDictionary) -> Result<Option<Vec<u8>>> {
    resolved_name(document, dictionary.get(b"Filter".as_slice()))
}

fn indexed_color_space(base_color_space: OwnedObject, palette: &[u8]) -> Result<OwnedObject> {
    if palette.is_empty() || !palette.len().is_multiple_of(3) {
        return Err(Error::Invalid(
            "Indexed palette is empty or not RGB-aligned".to_owned(),
        ));
    }
    let colors = palette.len() / 3;
    if colors > 256 {
        return Err(Error::Invalid(
            "Indexed palette exceeds 256 colors".to_owned(),
        ));
    }
    let hival = i64::try_from(colors - 1)
        .map_err(|_| Error::Invalid("Indexed palette size exceeds i64".to_owned()))?;
    Ok(OwnedObject::Array(vec![
        OwnedObject::Name(b"Indexed".to_vec()),
        base_color_space,
        OwnedObject::Integer(hival),
        OwnedObject::String(palette.to_vec()),
    ]))
}

fn indexed_color_space_with_transparent_zero(
    base_color_space: OwnedObject,
    palette: &[u8],
) -> Result<OwnedObject> {
    let mut lookup = vec![0_u8; 3];
    lookup.extend_from_slice(palette);
    let colors = lookup.len() / 3;
    let hival = colors
        .checked_sub(1)
        .and_then(|value| i64::try_from(value).ok())
        .ok_or_else(|| Error::Invalid("Indexed palette is empty or too large".to_owned()))?;
    if colors > 256 {
        return Err(Error::Invalid(
            "Indexed palette exceeds 256 colors".to_owned(),
        ));
    }
    Ok(OwnedObject::Array(vec![
        OwnedObject::Name(b"Indexed".to_vec()),
        base_color_space,
        OwnedObject::Integer(hival),
        OwnedObject::String(lookup),
    ]))
}

fn install_local_layered_indexed_transform(
    document: &mut EditDocument,
    handle: ObjectHandle,
    width: u32,
    height: u32,
    layers: Vec<LayeredIndexedLayerTransform>,
    base_color_space: &OwnedObject,
) -> Result<()> {
    let mut resources = OwnedDictionary::new();
    let mut content = Vec::new();
    for (index, layer) in layers.into_iter().enumerate() {
        let name = format!("PdfRedoxLocalPalette{index}").into_bytes();
        let color_space =
            indexed_color_space_with_transparent_zero(base_color_space.clone(), &layer.palette)?;
        let child = ObjectHandle::New(document.add_object(OwnedObject::Stream {
            dictionary: OwnedDictionary::from([
                (b"Type".to_vec(), OwnedObject::Name(b"XObject".to_vec())),
                (b"Subtype".to_vec(), OwnedObject::Name(b"Image".to_vec())),
                (b"Width".to_vec(), OwnedObject::Integer(i64::from(width))),
                (b"Height".to_vec(), OwnedObject::Integer(i64::from(height))),
                (b"ColorSpace".to_vec(), color_space),
                (b"BitsPerComponent".to_vec(), OwnedObject::Integer(8)),
                (
                    b"Filter".to_vec(),
                    OwnedObject::Name(b"FlateDecode".to_vec()),
                ),
                (
                    b"Mask".to_vec(),
                    OwnedObject::Array(vec![OwnedObject::Integer(0), OwnedObject::Integer(0)]),
                ),
            ]),
            data: StreamData::Owned(layer.encoded),
        }));
        resources.insert(name.clone(), OwnedObject::Reference(child));
        content.push(b'/');
        content.extend_from_slice(&name);
        content.extend_from_slice(b" Do\n");
    }

    let object = document.edit_handle(handle)?;
    *object = OwnedObject::Stream {
        dictionary: OwnedDictionary::from([
            (b"Type".to_vec(), OwnedObject::Name(b"XObject".to_vec())),
            (b"Subtype".to_vec(), OwnedObject::Name(b"Form".to_vec())),
            (b"FormType".to_vec(), OwnedObject::Integer(1)),
            (
                b"BBox".to_vec(),
                OwnedObject::Array(vec![
                    OwnedObject::Integer(0),
                    OwnedObject::Integer(0),
                    OwnedObject::Integer(1),
                    OwnedObject::Integer(1),
                ]),
            ),
            (
                b"Resources".to_vec(),
                OwnedObject::Dictionary(OwnedDictionary::from([(
                    b"XObject".to_vec(),
                    OwnedObject::Dictionary(resources),
                )])),
            ),
        ]),
        data: StreamData::Owned(content),
    };
    Ok(())
}

fn install_residual_indexed_transform(
    document: &mut EditDocument,
    handle: ObjectHandle,
    width: u32,
    height: u32,
    transform: ResidualIndexedTransform,
) -> Result<()> {
    let base_name = b"PdfRedoxResidualBase".to_vec();
    let residual_name = b"PdfRedoxResidualCorrection".to_vec();
    let base_color_space =
        indexed_color_space(transform.base_color_space.clone(), &transform.base_palette)?;
    let residual_color_space = indexed_color_space_with_transparent_zero(
        transform.base_color_space,
        &transform.residual_palette,
    )?;
    let base = ObjectHandle::New(document.add_object(OwnedObject::Stream {
        dictionary: OwnedDictionary::from([
            (b"Type".to_vec(), OwnedObject::Name(b"XObject".to_vec())),
            (b"Subtype".to_vec(), OwnedObject::Name(b"Image".to_vec())),
            (b"Width".to_vec(), OwnedObject::Integer(i64::from(width))),
            (b"Height".to_vec(), OwnedObject::Integer(i64::from(height))),
            (b"ColorSpace".to_vec(), base_color_space),
            (b"BitsPerComponent".to_vec(), OwnedObject::Integer(8)),
            (
                b"Filter".to_vec(),
                OwnedObject::Name(b"FlateDecode".to_vec()),
            ),
        ]),
        data: StreamData::Owned(transform.base_encoded),
    }));
    let residual = ObjectHandle::New(document.add_object(OwnedObject::Stream {
        dictionary: OwnedDictionary::from([
            (b"Type".to_vec(), OwnedObject::Name(b"XObject".to_vec())),
            (b"Subtype".to_vec(), OwnedObject::Name(b"Image".to_vec())),
            (b"Width".to_vec(), OwnedObject::Integer(i64::from(width))),
            (b"Height".to_vec(), OwnedObject::Integer(i64::from(height))),
            (b"ColorSpace".to_vec(), residual_color_space),
            (b"BitsPerComponent".to_vec(), OwnedObject::Integer(8)),
            (
                b"Filter".to_vec(),
                OwnedObject::Name(b"FlateDecode".to_vec()),
            ),
            (
                b"Mask".to_vec(),
                OwnedObject::Array(vec![OwnedObject::Integer(0), OwnedObject::Integer(0)]),
            ),
        ]),
        data: StreamData::Owned(transform.residual_encoded),
    }));
    let resources = OwnedDictionary::from([
        (base_name.clone(), OwnedObject::Reference(base)),
        (residual_name.clone(), OwnedObject::Reference(residual)),
    ]);
    let mut content = Vec::new();
    for name in [&base_name, &residual_name] {
        content.push(b'/');
        content.extend_from_slice(name);
        content.extend_from_slice(b" Do\n");
    }
    *document.edit_handle(handle)? = OwnedObject::Stream {
        dictionary: OwnedDictionary::from([
            (b"Type".to_vec(), OwnedObject::Name(b"XObject".to_vec())),
            (b"Subtype".to_vec(), OwnedObject::Name(b"Form".to_vec())),
            (b"FormType".to_vec(), OwnedObject::Integer(1)),
            (
                b"BBox".to_vec(),
                OwnedObject::Array(vec![
                    OwnedObject::Integer(0),
                    OwnedObject::Integer(0),
                    OwnedObject::Integer(1),
                    OwnedObject::Integer(1),
                ]),
            ),
            (
                b"Resources".to_vec(),
                OwnedObject::Dictionary(OwnedDictionary::from([(
                    b"XObject".to_vec(),
                    OwnedObject::Dictionary(resources),
                )])),
            ),
        ]),
        data: StreamData::Owned(content),
    };
    Ok(())
}

fn install_layered_indexed_transform(
    document: &mut EditDocument,
    handle: ObjectHandle,
    width: u32,
    height: u32,
    transform: LayeredIndexedTransform,
) -> Result<()> {
    match transform {
        LayeredIndexedTransform::Local {
            layers,
            base_color_space,
        } => install_local_layered_indexed_transform(
            document,
            handle,
            width,
            height,
            layers,
            &base_color_space,
        ),
        LayeredIndexedTransform::Residual(transform) => {
            install_residual_indexed_transform(document, handle, width, height, transform)
        }
    }
}

fn install_transform(
    document: &mut EditDocument,
    handle: ObjectHandle,
    transform: ImageTransform,
) -> Result<()> {
    if let Some(layered) = transform.layered_indexed {
        return install_layered_indexed_transform(
            document,
            handle,
            transform.width,
            transform.height,
            layered,
        );
    }
    let object = document.edit_handle(handle)?;
    let OwnedObject::Stream { dictionary, data } = object else {
        return Ok(());
    };
    dictionary.insert(
        b"Width".to_vec(),
        OwnedObject::Integer(i64::from(transform.width)),
    );
    dictionary.insert(
        b"Height".to_vec(),
        OwnedObject::Integer(i64::from(transform.height)),
    );
    dictionary.insert(b"Filter".to_vec(), OwnedObject::Name(transform.filter));
    if let Some(color_space) = transform.color_space {
        dictionary.insert(b"ColorSpace".to_vec(), color_space);
    }
    dictionary.remove(b"DecodeParms".as_slice());
    dictionary.remove(b"Length".as_slice());
    *data = StreamData::Owned(transform.encoded);
    Ok(())
}

fn passes_savings(options: ImageOptimizationOptions, original: u64, optimized: usize) -> bool {
    let optimized = u64::try_from(optimized).unwrap_or(u64::MAX);
    let savings = original.saturating_sub(optimized);
    optimized < original
        && savings >= options.min_savings_bytes
        && u128::from(savings) * 100
            >= u128::from(original) * u128::from(options.min_savings_percent)
}

fn finalize_jpeg_candidate(
    encoded: Vec<u8>,
    original: u64,
    options: ImageOptimizationOptions,
) -> Option<Vec<u8>> {
    // Lossy eligibility is decided on the raw encoder output. Coefficient-preserving
    // entropy optimization may shrink an already-admitted JPEG, but must never make
    // a visually marginal candidate cross the material-savings threshold.
    if !passes_savings(options, original, encoded.len()) {
        return None;
    }
    Some(
        crate::jpeg_optimize::optimized_jpeg_bytes(&encoded)
            .filter(|optimized| optimized.len() < encoded.len())
            .unwrap_or(encoded),
    )
}

fn jpeg_transcode_candidate(
    decoded: &[u8],
    width: u32,
    height: u32,
    components: usize,
    original: u64,
    options: ImageOptimizationOptions,
) -> Result<Option<ImageTransform>> {
    let encoded = encode_jpeg_raster(width, height, components, decoded, options.jpeg_quality)?;
    let Some(encoded) = finalize_jpeg_candidate(encoded, original, options) else {
        return Ok(None);
    };
    let optimized_encoded_bytes = u64::try_from(encoded.len()).unwrap_or(u64::MAX);
    let pixels = u64::from(width) * u64::from(height);
    Ok(Some(ImageTransform {
        encoded,
        width,
        height,
        filter: b"DCTDecode".to_vec(),
        color_space: None,
        layered_indexed: None,
        original_encoded_bytes: original,
        optimized_encoded_bytes,
        original_pixels: pixels,
        optimized_pixels: pixels,
        resized: false,
        paper_indexed: false,
        encoding: ImageResizeEncoding::Jpeg,
    }))
}

fn paper_indexed_source_safe(
    document: &EditDocument,
    dictionary: &OwnedDictionary,
    source_filter: Option<&[u8]>,
) -> Result<bool> {
    if !matches!(source_filter, Some(b"FlateDecode" | b"Fl")) {
        return Ok(false);
    }
    for key in [
        b"Decode".as_slice(),
        b"Mask".as_slice(),
        b"SMask".as_slice(),
        b"SMaskInData".as_slice(),
    ] {
        if is_non_null(document, dictionary.get(key))? {
            return Ok(false);
        }
    }
    Ok(true)
}

fn redundant_opaque_smask(document: &EditDocument, dictionary: &OwnedDictionary) -> Result<bool> {
    let Some(smask) = dictionary.get(b"SMask".as_slice()) else {
        return Ok(true);
    };
    let Some(smask) = document.resolve_owned_value(smask)? else {
        return Ok(false);
    };
    let OwnedObject::Stream {
        dictionary: mask_dictionary,
        data,
    } = smask
    else {
        return Ok(match smask {
            OwnedObject::Null => true,
            OwnedObject::Name(name) => name.as_slice() == b"None",
            _ => false,
        });
    };
    if !mask_dictionary
        .keys()
        .all(|key| OPAQUE_SMASK_ALLOWED_KEYS.contains(&key.as_slice()))
        || resolved_name(document, mask_dictionary.get(b"Subtype".as_slice()))?.as_deref()
            != Some(b"Image")
        || resolved_name(document, mask_dictionary.get(b"ColorSpace".as_slice()))?.as_deref()
            != Some(b"DeviceGray")
        || !bpc_is_8(document, &mask_dictionary)?
        || !mask_dictionary.contains_key(b"Filter".as_slice())
        || !is_unfiltered_or_lone_flate(document, &mask_dictionary)?
    {
        return Ok(false);
    }
    let Some(width) = dimension(document, &mask_dictionary, b"Width")? else {
        return Ok(false);
    };
    let Some(height) = dimension(document, &mask_dictionary, b"Height")? else {
        return Ok(false);
    };
    let encoded = data.bytes(document.source())?;
    let Ok(decoded) = decode_image_stream(
        document,
        &mask_dictionary,
        encoded.as_ref(),
        width,
        height,
        8,
        1,
    ) else {
        return Ok(false);
    };
    let Some(expected) = usize::try_from(width)
        .ok()
        .and_then(|w| usize::try_from(height).ok().and_then(|h| w.checked_mul(h)))
    else {
        return Ok(false);
    };
    Ok(decoded.len() == expected && decoded.iter().all(|&sample| sample == u8::MAX))
}

fn paper_indexed_layered_source_safe(
    document: &EditDocument,
    dictionary: &OwnedDictionary,
) -> Result<bool> {
    if !dictionary.contains_key(b"Filter".as_slice())
        || !is_unfiltered_or_lone_flate(document, dictionary)?
    {
        return Ok(false);
    }
    for key in [
        b"Decode".as_slice(),
        b"Mask".as_slice(),
        b"SMaskInData".as_slice(),
    ] {
        if is_non_null(document, dictionary.get(key))? {
            return Ok(false);
        }
    }
    if !redundant_opaque_smask(document, dictionary)? {
        return Ok(false);
    }
    Ok(dictionary
        .keys()
        .all(|key| LOCAL_PALETTE_ALLOWED_SOURCE_KEYS.contains(&key.as_slice())))
}

fn paper_indexed_candidate_transform(
    candidate: paper_indexed::IndexedPaperImage,
    width: u32,
    height: u32,
    original: u64,
    options: ImageOptimizationOptions,
    flate_level: crate::FlateLevel,
    base_color_space: OwnedObject,
) -> Result<Option<ImageTransform>> {
    let encoded = encode_flate(&candidate.indices, flate_level)?;
    // Binary lookup strings are serialized as hexadecimal by the compact writer.
    // Account for that representation conservatively in both admission and stats.
    let palette_cost = candidate.palette.len().saturating_mul(2).saturating_add(2);
    let optimized_payload = encoded.len().saturating_add(palette_cost);
    if !passes_savings(options, original, optimized_payload) {
        return Ok(None);
    }
    let hival = candidate.palette.len() / 3;
    let Some(hival) = hival
        .checked_sub(1)
        .and_then(|value| i64::try_from(value).ok())
    else {
        return Ok(None);
    };
    let color_space = OwnedObject::Array(vec![
        OwnedObject::Name(b"Indexed".to_vec()),
        base_color_space,
        OwnedObject::Integer(hival),
        OwnedObject::String(candidate.palette),
    ]);
    let pixels = u64::from(width) * u64::from(height);
    Ok(Some(ImageTransform {
        encoded,
        width,
        height,
        filter: b"FlateDecode".to_vec(),
        color_space: Some(color_space),
        layered_indexed: None,
        original_encoded_bytes: original,
        optimized_encoded_bytes: u64::try_from(optimized_payload).unwrap_or(u64::MAX),
        original_pixels: pixels,
        optimized_pixels: pixels,
        resized: false,
        paper_indexed: true,
        encoding: ImageResizeEncoding::Flate,
    }))
}

fn paper_indexed_transform(
    decoded: &[u8],
    width: u32,
    height: u32,
    original: u64,
    options: ImageOptimizationOptions,
    flate_level: crate::FlateLevel,
    base_color_space: OwnedObject,
) -> Result<Option<ImageTransform>> {
    let Some(candidate) = paper_indexed::quantize_rgb(decoded) else {
        return Ok(None);
    };
    paper_indexed_candidate_transform(
        candidate,
        width,
        height,
        original,
        options,
        flate_level,
        base_color_space,
    )
}

fn paper_indexed_foreground_transform(
    decoded: &[u8],
    width: u32,
    height: u32,
    original: u64,
    options: ImageOptimizationOptions,
    flate_level: crate::FlateLevel,
    base_color_space: OwnedObject,
) -> Result<Option<ImageTransform>> {
    let Some(candidate) = paper_indexed::quantize_rgb_foreground_weighted_or_residual(decoded)
    else {
        return Ok(None);
    };
    match candidate {
        paper_indexed::ForegroundIndexedPaperImage::Weighted(candidate) => {
            paper_indexed_candidate_transform(
                candidate,
                width,
                height,
                original,
                options,
                flate_level,
                base_color_space,
            )
        }
        paper_indexed::ForegroundIndexedPaperImage::Residual(candidate) => {
            let pixel_count = usize::try_from(width)
                .ok()
                .and_then(|width| {
                    usize::try_from(height)
                        .ok()
                        .and_then(|height| width.checked_mul(height))
                })
                .ok_or_else(|| Error::Invalid("residual palette dimensions overflow".to_owned()))?;
            if candidate.base_indices.len() != pixel_count
                || candidate.residual_indices.len() != pixel_count
                || candidate.base_palette.len() != 256 * 3
                || candidate.residual_palette.is_empty()
                || !candidate.residual_palette.len().is_multiple_of(3)
                || candidate.residual_palette.len() > 255 * 3
            {
                return Ok(None);
            }
            let base_encoded = encode_flate(&candidate.base_indices, flate_level)?;
            let residual_encoded = encode_flate(&candidate.residual_indices, flate_level)?;
            let base_palette_cost = candidate
                .base_palette
                .len()
                .saturating_mul(2)
                .saturating_add(2);
            let residual_palette_bytes = candidate.residual_palette.len().saturating_add(3);
            let residual_palette_cost = residual_palette_bytes.saturating_mul(2).saturating_add(2);
            let optimized_payload = LOCAL_PALETTE_FORM_OVERHEAD_BYTES
                .saturating_add(base_encoded.len())
                .saturating_add(base_palette_cost)
                .saturating_add(LOCAL_PALETTE_LAYER_OVERHEAD_BYTES)
                .saturating_add(residual_encoded.len())
                .saturating_add(residual_palette_cost)
                .saturating_add(LOCAL_PALETTE_LAYER_OVERHEAD_BYTES);
            if !passes_savings(options, original, optimized_payload) {
                return Ok(None);
            }
            let pixels = u64::from(width) * u64::from(height);
            Ok(Some(ImageTransform {
                encoded: Vec::new(),
                width,
                height,
                filter: Vec::new(),
                color_space: None,
                layered_indexed: Some(LayeredIndexedTransform::Residual(
                    ResidualIndexedTransform {
                        base_encoded,
                        base_palette: candidate.base_palette,
                        residual_encoded,
                        residual_palette: candidate.residual_palette,
                        base_color_space,
                    },
                )),
                original_encoded_bytes: original,
                optimized_encoded_bytes: u64::try_from(optimized_payload).unwrap_or(u64::MAX),
                original_pixels: pixels,
                optimized_pixels: pixels,
                resized: false,
                paper_indexed: true,
                encoding: ImageResizeEncoding::Flate,
            }))
        }
    }
}

fn paper_indexed_layered_transform(
    decoded: &[u8],
    width: u32,
    height: u32,
    original: u64,
    options: ImageOptimizationOptions,
    flate_level: crate::FlateLevel,
    base_color_space: OwnedObject,
) -> Result<Option<ImageTransform>> {
    let width_usize = usize::try_from(width)
        .map_err(|_| Error::Invalid("local palette width exceeds usize".to_owned()))?;
    let height_usize = usize::try_from(height)
        .map_err(|_| Error::Invalid("local palette height exceeds usize".to_owned()))?;
    let pixels = width_usize
        .checked_mul(height_usize)
        .ok_or_else(|| Error::Invalid("local palette dimensions overflow".to_owned()))?;

    for layer_count in paper_indexed::LOCAL_LAYER_COUNTS {
        for sample_factor in paper_indexed::LOCAL_QUANTIZER_SAMPLE_FACTORS {
            let Some(candidate) = paper_indexed::quantize_rgb_layer_count_with_sample_factor(
                decoded,
                width,
                height,
                layer_count,
                sample_factor,
            ) else {
                continue;
            };
            let mut layers = Vec::with_capacity(candidate.layers.len());
            let mut optimized_payload = LOCAL_PALETTE_FORM_OVERHEAD_BYTES;
            for layer in candidate.layers {
                let row_pixels = layer
                    .row_end
                    .checked_sub(layer.row_start)
                    .and_then(|rows| rows.checked_mul(width_usize))
                    .ok_or_else(|| Error::Invalid("local palette row range overflow".to_owned()))?;
                if layer.indices.len() != row_pixels {
                    return Ok(None);
                }
                let start = layer
                    .row_start
                    .checked_mul(width_usize)
                    .ok_or_else(|| Error::Invalid("local palette start overflow".to_owned()))?;
                let end = start
                    .checked_add(row_pixels)
                    .ok_or_else(|| Error::Invalid("local palette end overflow".to_owned()))?;
                let mut full_grid = vec![0_u8; pixels];
                for (output, &index) in full_grid
                    .get_mut(start..end)
                    .ok_or_else(|| Error::Invalid("local palette row range is invalid".to_owned()))?
                    .iter_mut()
                    .zip(&layer.indices)
                {
                    *output = index.checked_add(1).ok_or_else(|| {
                        Error::Invalid(
                            "local palette index exceeds reserved 8-bit range".to_owned(),
                        )
                    })?;
                }
                let encoded = encode_flate(&full_grid, flate_level)?;
                let palette_bytes = layer.palette.len().saturating_add(3);
                optimized_payload = optimized_payload
                    .saturating_add(encoded.len())
                    .saturating_add(palette_bytes.saturating_mul(2).saturating_add(2))
                    .saturating_add(LOCAL_PALETTE_LAYER_OVERHEAD_BYTES);
                layers.push(LayeredIndexedLayerTransform {
                    encoded,
                    palette: layer.palette,
                });
            }
            if !passes_savings(options, original, optimized_payload) {
                continue;
            }
            let pixel_count = u64::from(width) * u64::from(height);
            return Ok(Some(ImageTransform {
                encoded: Vec::new(),
                width,
                height,
                filter: Vec::new(),
                color_space: None,
                layered_indexed: Some(LayeredIndexedTransform::Local {
                    layers,
                    base_color_space,
                }),
                original_encoded_bytes: original,
                optimized_encoded_bytes: u64::try_from(optimized_payload).unwrap_or(u64::MAX),
                original_pixels: pixel_count,
                optimized_pixels: pixel_count,
                resized: false,
                paper_indexed: true,
                encoding: ImageResizeEncoding::Flate,
            }));
        }
    }
    Ok(None)
}

fn decoded_pixels(
    document: &EditDocument,
    image: &OwnedObject,
    width: u32,
    height: u32,
    components: usize,
) -> Result<Option<Vec<u8>>> {
    let OwnedObject::Stream { dictionary, data } = image else {
        return Ok(None);
    };
    let encoded = data.bytes(document.source())?;
    let components_u8 = u8::try_from(components)
        .map_err(|_| Error::Invalid("image component count exceeds u8".to_owned()))?;
    let Ok(decoded) = decode_image_stream(
        document,
        dictionary,
        encoded.as_ref(),
        width,
        height,
        8,
        components_u8,
    ) else {
        return Ok(None);
    };
    let expected = usize::try_from(width)
        .ok()
        .and_then(|w| usize::try_from(height).ok().and_then(|h| w.checked_mul(h)))
        .and_then(|pixels| pixels.checked_mul(components));
    Ok((expected == Some(decoded.len())).then_some(decoded))
}

fn direct_jpeg_is_strictly_finer_than_target(
    encoded: &[u8],
    expected_components: usize,
    target_quality: u8,
) -> bool {
    if !matches!(expected_components, 1 | 3) {
        return false;
    }
    let Ok(coefficients) = read_coefficients(encoded) else {
        return false;
    };
    if coefficients.effective_precision() != 8
        || coefficients.components.len() != expected_components
    {
        return false;
    }

    let target_luma =
        quality_scale_quant_table(&STD_LUMINANCE_QUANT_TABLE, target_quality.clamp(1, 100));
    let target_chroma =
        quality_scale_quant_table(&STD_CHROMINANCE_QUANT_TABLE, target_quality.clamp(1, 100));
    let mut strictly_finer = false;
    for (component_index, component) in coefficients.components.iter().enumerate() {
        let Some(source) = coefficients
            .quant_tables
            .get(usize::from(component.quant_table_index))
        else {
            return false;
        };
        let target = if expected_components == 1 || component_index == 0 {
            &target_luma
        } else {
            &target_chroma
        };
        for (&source_value, &target_value) in source.iter().zip(target.iter()) {
            if source_value > target_value {
                return false;
            }
            strictly_finer |= source_value < target_value;
        }
    }
    strictly_finer
}

const fn meets_image_optimization_size(
    width: u32,
    height: u32,
    options: ImageOptimizationOptions,
) -> bool {
    let area = width.wrapping_mul(height);
    (options.min_width == 0 || width > options.min_width)
        && (options.min_height == 0 || height > options.min_height)
        && (options.min_area == 0 || area > options.min_area)
}

fn transcode_image(
    document: &EditDocument,
    image: &OwnedObject,
    options: ImageOptimizationOptions,
    flate_level: crate::FlateLevel,
) -> Result<Option<ImageTransform>> {
    let OwnedObject::Stream { dictionary, data } = image else {
        return Ok(None);
    };
    let Some(width) = dimension(document, dictionary, b"Width")? else {
        return Ok(None);
    };
    let Some(height) = dimension(document, dictionary, b"Height")? else {
        return Ok(None);
    };
    if !bpc_is_8(document, dictionary)? {
        return Ok(None);
    }
    let device_color_info = color_info(document, dictionary)?;
    let indexed_base_color_space = indexed_rgb_base_color_space(document, dictionary)?;
    let components = match device_color_info.as_ref() {
        Some((components, _, _)) => *components,
        None if indexed_base_color_space.is_some() => 3,
        None => return Ok(None),
    };
    if !meets_image_optimization_size(width, height, options) {
        return Ok(None);
    }
    let source_filter = source_filter(document, dictionary)?;
    let source_bytes = data.bytes(document.source())?;
    if matches!(source_filter.as_deref(), Some(b"DCTDecode" | b"DCT")) {
        if device_color_info.is_none() {
            return Ok(None);
        }
        for key in [
            b"DecodeParms".as_slice(),
            b"Decode".as_slice(),
            b"Mask".as_slice(),
        ] {
            if is_non_null(document, dictionary.get(key))? {
                return Ok(None);
            }
        }
        if !direct_jpeg_is_strictly_finer_than_target(
            source_bytes.as_ref(),
            components,
            options.jpeg_quality,
        ) {
            return Ok(None);
        }
    }
    let Some(decoded) = decoded_pixels(document, image, width, height, components)? else {
        return Ok(None);
    };
    let original = u64::try_from(source_bytes.len()).unwrap_or(u64::MAX);
    if device_color_info.is_some()
        && let Some(candidate) =
            jpeg_transcode_candidate(&decoded, width, height, components, original, options)?
    {
        return Ok(Some(candidate));
    }
    if components != 3 {
        return Ok(None);
    }
    let Some(indexed_base_color_space) = indexed_base_color_space else {
        return Ok(None);
    };
    if paper_indexed_source_safe(document, dictionary, source_filter.as_deref())?
        && let Some(candidate) = paper_indexed_transform(
            &decoded,
            width,
            height,
            original,
            options,
            flate_level,
            indexed_base_color_space.clone(),
        )?
    {
        return Ok(Some(candidate));
    }
    if !paper_indexed_layered_source_safe(document, dictionary)? {
        return Ok(None);
    }
    if let Some(candidate) = paper_indexed_layered_transform(
        &decoded,
        width,
        height,
        original,
        options,
        flate_level,
        indexed_base_color_space.clone(),
    )? {
        return Ok(Some(candidate));
    }
    paper_indexed_foreground_transform(
        &decoded,
        width,
        height,
        original,
        options,
        flate_level,
        indexed_base_color_space,
    )
}

fn resize_source_safe(
    document: &EditDocument,
    dictionary: &OwnedDictionary,
    encoding: ImageResizeEncoding,
) -> Result<bool> {
    if !bpc_is_8(document, dictionary)? {
        return Ok(false);
    }
    for key in [
        b"SMask".as_slice(),
        b"Mask".as_slice(),
        b"Decode".as_slice(),
    ] {
        if is_non_null(document, dictionary.get(key))? {
            return Ok(false);
        }
    }
    if encoding == ImageResizeEncoding::Jpeg
        && is_non_null(document, dictionary.get(b"DecodeParms".as_slice()))?
    {
        return Ok(false);
    }
    let Some(color) = resolved_name(document, dictionary.get(b"ColorSpace".as_slice()))? else {
        return Ok(false);
    };
    if !matches!(color.as_slice(), b"DeviceGray" | b"DeviceRGB") {
        return Ok(false);
    }
    let Some(filter) = source_filter(document, dictionary)? else {
        return Ok(false);
    };
    Ok(match encoding {
        ImageResizeEncoding::Jpeg => matches!(filter.as_slice(), b"DCTDecode" | b"DCT"),
        ImageResizeEncoding::Flate => matches!(filter.as_slice(), b"FlateDecode" | b"Fl"),
    })
}

fn resize_image(
    document: &EditDocument,
    image: &OwnedObject,
    options: ImageOptimizationOptions,
    target: ImageResizeTarget,
    flate_level: crate::FlateLevel,
) -> Result<Option<ImageTransform>> {
    let OwnedObject::Stream { dictionary, data } = image else {
        return Ok(None);
    };
    if !resize_source_safe(document, dictionary, target.encoding)? {
        return Ok(None);
    }
    let Some(width) = dimension(document, dictionary, b"Width")? else {
        return Ok(None);
    };
    let Some(height) = dimension(document, dictionary, b"Height")? else {
        return Ok(None);
    };
    let Some((components, _, pixel_type)) = color_info(document, dictionary)? else {
        return Ok(None);
    };
    let target_width = target.width.max(1).min(width);
    let target_height = target.height.max(1).min(height);
    if target_width == width && target_height == height {
        return Ok(None);
    }
    let Some(decoded) = decoded_pixels(document, image, width, height, components)? else {
        return Ok(None);
    };
    let source = ResizeImage::from_vec_u8(width, height, decoded, pixel_type)
        .map_err(|error| Error::Invalid(format!("invalid decoded image buffer: {error}")))?;
    let mut destination = ResizeImage::new(target_width, target_height, pixel_type);
    Resizer::new()
        .resize(
            &source,
            &mut destination,
            &ResizeOptions::new().resize_alg(ResizeAlg::Convolution(FilterType::Lanczos3)),
        )
        .map_err(|error| Error::Invalid(format!("unable to resize image: {error}")))?;
    let (encoded, filter) = match target.encoding {
        ImageResizeEncoding::Jpeg => (
            encode_jpeg_raster(
                target_width,
                target_height,
                components,
                destination.buffer(),
                options.jpeg_quality,
            )?,
            b"DCTDecode".to_vec(),
        ),
        ImageResizeEncoding::Flate => (
            encode_flate(destination.buffer(), flate_level)?,
            b"FlateDecode".to_vec(),
        ),
    };
    let original = u64::try_from(data.bytes(document.source())?.len()).unwrap_or(u64::MAX);
    if !passes_savings(options, original, encoded.len()) {
        return Ok(None);
    }
    let optimized_encoded_bytes = u64::try_from(encoded.len()).unwrap_or(u64::MAX);
    Ok(Some(ImageTransform {
        encoded,
        width: target_width,
        height: target_height,
        filter,
        color_space: None,
        layered_indexed: None,
        original_encoded_bytes: original,
        optimized_encoded_bytes,
        original_pixels: u64::from(width) * u64::from(height),
        optimized_pixels: u64::from(target_width) * u64::from(target_height),
        resized: true,
        paper_indexed: false,
        encoding: target.encoding,
    }))
}

struct LayeredTransformSafety<'a> {
    binding_count: usize,
    non_xobject_references: &'a mut Option<HashSet<ObjectHandle>>,
}

fn transform_one(
    document: &mut EditDocument,
    handle: ObjectHandle,
    options: ImageOptimizationOptions,
    target: Option<ImageResizeTarget>,
    flate_level: crate::FlateLevel,
    layered_safety: &mut LayeredTransformSafety<'_>,
    stats: &mut ImageOptimizationStats,
) -> Result<bool> {
    if !is_image(document, handle)? {
        return Ok(false);
    }
    let Some(image) = document.current_owned_object(handle)? else {
        return Ok(false);
    };
    let transform = match target {
        Some(target) => resize_image(document, &image, options, target, flate_level)?,
        None => transcode_image(document, &image, options, flate_level)?,
    };
    let Some(transform) = transform else {
        return Ok(false);
    };
    if transform.layered_indexed.is_some() {
        if layered_safety.binding_count == 0 {
            return Ok(false);
        }
        if layered_safety.non_xobject_references.is_none() {
            *layered_safety.non_xobject_references = Some(non_xobject_reference_targets(document)?);
        }
        if layered_safety
            .non_xobject_references
            .as_ref()
            .is_some_and(|targets| targets.contains(&handle))
        {
            return Ok(false);
        }
    }
    stats.images_optimized = stats.images_optimized.saturating_add(1);
    if transform.paper_indexed {
        stats.paper_indexed_images = stats.paper_indexed_images.saturating_add(1);
        match transform.layered_indexed.as_ref() {
            Some(LayeredIndexedTransform::Local { .. }) => {
                stats.paper_indexed_layered_images =
                    stats.paper_indexed_layered_images.saturating_add(1);
            }
            Some(LayeredIndexedTransform::Residual(_)) => {
                stats.paper_indexed_residual_images =
                    stats.paper_indexed_residual_images.saturating_add(1);
            }
            None => {}
        }
    }
    if transform.resized {
        stats.images_resized = stats.images_resized.saturating_add(1);
        match transform.encoding {
            ImageResizeEncoding::Jpeg => {
                stats.jpeg_images_resized = stats.jpeg_images_resized.saturating_add(1);
            }
            ImageResizeEncoding::Flate => {
                stats.flate_images_resized = stats.flate_images_resized.saturating_add(1);
            }
        }
    }
    stats.original_encoded_bytes = stats
        .original_encoded_bytes
        .saturating_add(transform.original_encoded_bytes);
    stats.optimized_encoded_bytes = stats
        .optimized_encoded_bytes
        .saturating_add(transform.optimized_encoded_bytes);
    stats.original_pixels = stats
        .original_pixels
        .saturating_add(transform.original_pixels);
    stats.optimized_pixels = stats
        .optimized_pixels
        .saturating_add(transform.optimized_pixels);
    install_transform(document, handle, transform)?;
    Ok(true)
}

pub fn optimize_images(
    document: &mut EditDocument,
    options: ImageOptimizationOptions,
    flate_level: crate::FlateLevel,
) -> Result<ImageOptimizationStats> {
    let binding_counts = image_binding_counts(document)?;
    let mut non_xobject_references = None;
    let mut stats = ImageOptimizationStats::default();
    let images = document
        .reachable_streams_with_subtype(b"Image")?
        .into_iter()
        .collect::<BTreeSet<_>>();
    for image in images {
        let binding_count = binding_counts.get(&image).copied().unwrap_or(0);
        let mut layered_safety = LayeredTransformSafety {
            binding_count,
            non_xobject_references: &mut non_xobject_references,
        };
        if transform_one(
            document,
            image,
            options,
            None,
            flate_level,
            &mut layered_safety,
            &mut stats,
        )? {
            stats.references_reused = stats.references_reused.saturating_add(
                binding_counts
                    .get(&image)
                    .copied()
                    .unwrap_or(1)
                    .saturating_sub(1),
            );
        }
    }
    Ok(stats)
}

pub fn optimize_images_with_resize_targets(
    document: &mut EditDocument,
    options: ImageOptimizationOptions,
    flate_level: crate::FlateLevel,
    targets: &HashMap<(ObjectHandle, ObjectHandle), ImageResizeTarget>,
) -> Result<ImageOptimizationStats> {
    let mut by_image = BTreeMap::<ObjectHandle, ImageResizeTarget>::new();
    let mut binding_counts = BTreeMap::<ObjectHandle, usize>::new();
    for (&(_page, image), &target) in targets {
        match by_image.get(&image).copied() {
            None => {
                by_image.insert(image, target);
            }
            Some(existing) if existing == target => {}
            Some(existing) => {
                by_image.insert(
                    image,
                    ImageResizeTarget {
                        width: existing.width.max(target.width),
                        height: existing.height.max(target.height),
                        encoding: existing.encoding,
                    },
                );
            }
        }
        *binding_counts.entry(image).or_default() += 1;
    }
    let mut stats = ImageOptimizationStats::default();
    let mut non_xobject_references = None;
    for (image, target) in by_image {
        let mut layered_safety = LayeredTransformSafety {
            binding_count: 0,
            non_xobject_references: &mut non_xobject_references,
        };
        if transform_one(
            document,
            image,
            options,
            Some(target),
            flate_level,
            &mut layered_safety,
            &mut stats,
        )? {
            stats.references_reused = stats.references_reused.saturating_add(
                binding_counts
                    .get(&image)
                    .copied()
                    .unwrap_or(1)
                    .saturating_sub(1),
            );
        }
    }
    Ok(stats)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rgb_jpeg(quality: u8) -> Result<Vec<u8>> {
        let width = 64usize;
        let height = 64usize;
        let mut pixels = Vec::with_capacity(width * height * 3);
        for y in 0..height {
            for x in 0..width {
                pixels.push(
                    u8::try_from(x * 255 / width)
                        .map_err(|_| Error::Invalid("test x gradient exceeds u8".to_owned()))?,
                );
                pixels.push(
                    u8::try_from(y * 255 / height)
                        .map_err(|_| Error::Invalid("test y gradient exceeds u8".to_owned()))?,
                );
                pixels.push(
                    u8::try_from((x + y) * 255 / (width + height)).map_err(|_| {
                        Error::Invalid("test diagonal gradient exceeds u8".to_owned())
                    })?,
                );
            }
        }
        compress(
            &pixels,
            width,
            height,
            PixelFormat::Rgb,
            quality,
            Subsampling::S420,
        )
        .map_err(|error| Error::Invalid(format!("test JPEG encode failed: {error}")))
    }

    fn direct_jpeg_pdf(jpeg: &[u8], extra_image_dictionary: &[u8]) -> Result<Vec<u8>> {
        use crate::test_support::ClassicPdfBuilder;

        let mut pdf = ClassicPdfBuilder::new();
        pdf.object(1, b"<< /Type /Catalog /Pages 2 0 R >>")?;
        pdf.object(2, b"<< /Type /Pages /Kids [3 0 R] /Count 1 >>")?;
        pdf.object(
            3,
            b"<< /Type /Page /Parent 2 0 R /MediaBox [0 0 64 64] /Resources << /XObject << /Im0 5 0 R >> >> /Contents 4 0 R >>",
        )?;
        pdf.stream(4, b"", b"q 64 0 0 64 0 0 cm /Im0 Do Q")?;
        let mut dictionary = b"/Type /XObject /Subtype /Image /Width 64 /Height 64 /ColorSpace /DeviceRGB /BitsPerComponent 8 /Filter /DCTDecode".to_vec();
        if !extra_image_dictionary.is_empty() {
            dictionary.push(b' ');
            dictionary.extend_from_slice(extra_image_dictionary);
        }
        pdf.stream(5, &dictionary, jpeg)?;
        pdf.finish(1)
    }

    fn unrelated_xobject_key_pdf(jpeg: &[u8]) -> Result<Vec<u8>> {
        use crate::test_support::ClassicPdfBuilder;

        let mut pdf = ClassicPdfBuilder::new();
        pdf.object(
            1,
            b"<< /Type /Catalog /Pages 2 0 R /Private << /XObject << /Alias 5 0 R >> >> >>",
        )?;
        pdf.object(2, b"<< /Type /Pages /Kids [3 0 R] /Count 1 >>")?;
        pdf.object(
            3,
            b"<< /Type /Page /Parent 2 0 R /MediaBox [0 0 64 64] /Resources << /XObject << /Im0 5 0 R >> >> /Contents 4 0 R >>",
        )?;
        pdf.stream(4, b"", b"q 64 0 0 64 0 0 cm /Im0 Do Q")?;
        pdf.stream(
            5,
            b"/Type /XObject /Subtype /Image /Width 64 /Height 64 /ColorSpace /DeviceRGB /BitsPerComponent 8 /Filter /DCTDecode",
            jpeg,
        )?;
        pdf.finish(1)
    }

    fn indirect_xobject_map_pdf(jpeg: &[u8]) -> Result<Vec<u8>> {
        use crate::test_support::ClassicPdfBuilder;

        let mut pdf = ClassicPdfBuilder::new();
        pdf.object(1, b"<< /Type /Catalog /Pages 2 0 R >>")?;
        pdf.object(2, b"<< /Type /Pages /Kids [3 0 R] /Count 1 >>")?;
        pdf.object(
            3,
            b"<< /Type /Page /Parent 2 0 R /MediaBox [0 0 64 64] /Resources << /XObject 7 0 R >> /Contents 4 0 R >>",
        )?;
        pdf.stream(4, b"", b"q 64 0 0 64 0 0 cm /Im0 Do Q")?;
        pdf.stream(
            5,
            b"/Type /XObject /Subtype /Image /Width 64 /Height 64 /ColorSpace /DeviceRGB /BitsPerComponent 8 /Filter /DCTDecode",
            jpeg,
        )?;
        pdf.object(7, b"<< /Im0 5 0 R >>")?;
        pdf.finish(1)
    }

    fn image_with_smask_pdf(jpeg: &[u8], mask_sample: u8) -> Result<Vec<u8>> {
        use crate::test_support::ClassicPdfBuilder;

        let mut pdf = ClassicPdfBuilder::new();
        pdf.object(1, b"<< /Type /Catalog /Pages 2 0 R >>")?;
        pdf.object(2, b"<< /Type /Pages /Kids [3 0 R] /Count 1 >>")?;
        pdf.object(
            3,
            b"<< /Type /Page /Parent 2 0 R /MediaBox [0 0 64 64] /Resources << /XObject << /Im0 5 0 R /Sm0 6 0 R >> >> /Contents 4 0 R >>",
        )?;
        pdf.stream(4, b"", b"q 64 0 0 64 0 0 cm /Im0 Do Q")?;
        pdf.stream(
            5,
            b"/Type /XObject /Subtype /Image /Width 64 /Height 64 /ColorSpace /DeviceRGB /BitsPerComponent 8 /Filter /DCTDecode /SMask 6 0 R",
            jpeg,
        )?;
        let mask = encode_flate(&vec![mask_sample; 64 * 64], crate::FlateLevel::SIZE)?;
        pdf.stream(
            6,
            b"/Type /XObject /Subtype /Image /Width 64 /Height 64 /ColorSpace /DeviceGray /BitsPerComponent 8 /Filter /FlateDecode",
            &mask,
        )?;
        pdf.finish(1)
    }

    fn icc_profile(data_space: [u8; 4]) -> Vec<u8> {
        let mut profile = vec![0_u8; 128];
        profile[0..4].copy_from_slice(&128_u32.to_be_bytes());
        profile[16..20].copy_from_slice(&data_space);
        profile[20..24].copy_from_slice(b"XYZ ");
        profile[36..40].copy_from_slice(b"acsp");
        profile
    }

    fn iccbased_flate_pdf(data_space: [u8; 4], indirect: bool) -> Result<Vec<u8>> {
        use crate::test_support::ClassicPdfBuilder;

        let width = 800_usize;
        let height = 700_usize;
        let rgb = paper_rgb(width, height);
        let encoded = encode_flate(&rgb, crate::FlateLevel::SIZE)?;
        let mut pdf = ClassicPdfBuilder::new();
        pdf.object(1, b"<< /Type /Catalog /Pages 2 0 R >>")?;
        pdf.object(2, b"<< /Type /Pages /Kids [3 0 R] /Count 1 >>")?;
        pdf.object(
            3,
            b"<< /Type /Page /Parent 2 0 R /MediaBox [0 0 800 700] /Resources << /XObject << /Im0 5 0 R >> >> /Contents 4 0 R >>",
        )?;
        pdf.stream(4, b"", b"q 800 0 0 700 0 0 cm /Im0 Do Q")?;
        let color_space = if indirect {
            b"7 0 R".as_slice()
        } else {
            b"[/ICCBased 6 0 R]".as_slice()
        };
        let mut image_dictionary =
            b"/Type /XObject /Subtype /Image /Width 800 /Height 700 /ColorSpace ".to_vec();
        image_dictionary.extend_from_slice(color_space);
        image_dictionary.extend_from_slice(b" /BitsPerComponent 8 /Filter /FlateDecode");
        pdf.stream(5, &image_dictionary, &encoded)?;
        pdf.stream(6, b"/N 3", &icc_profile(data_space))?;
        if indirect {
            pdf.object(7, b"[/ICCBased 6 0 R]")?;
        }
        pdf.finish(1)
    }

    fn iccbased_flate_transform(
        data_space: [u8; 4],
        indirect: bool,
    ) -> Result<Option<ImageTransform>> {
        let document = EditDocument::from_bytes(iccbased_flate_pdf(data_space, indirect)?)?;
        let Some(handle) = document
            .reachable_streams_with_subtype(b"Image")?
            .into_iter()
            .next()
        else {
            return Err(Error::Invalid("ICC test image is unreachable".to_owned()));
        };
        let Some(image) = document.current_owned_object(handle)? else {
            return Err(Error::Invalid(
                "ICC test image object is missing".to_owned(),
            ));
        };
        transcode_image(
            &document,
            &image,
            ImageOptimizationOptions {
                min_width: 0,
                min_height: 0,
                min_area: 0,
                jpeg_quality: 85,
                min_savings_bytes: 1,
                min_savings_percent: 20,
                ..ImageOptimizationOptions::default()
            },
            crate::FlateLevel::SIZE,
        )
    }

    fn direct_jpeg_transform(
        jpeg: &[u8],
        extra_image_dictionary: &[u8],
    ) -> Result<Option<ImageTransform>> {
        let pdf = direct_jpeg_pdf(jpeg, extra_image_dictionary)?;
        let document = EditDocument::from_bytes(pdf)?;
        let Some(handle) = document
            .reachable_streams_with_subtype(b"Image")?
            .into_iter()
            .next()
        else {
            return Err(Error::Invalid("test image is unreachable".to_owned()));
        };
        let Some(image) = document.current_owned_object(handle)? else {
            return Err(Error::Invalid("test image object is missing".to_owned()));
        };
        transcode_image(
            &document,
            &image,
            ImageOptimizationOptions {
                min_width: 0,
                min_height: 0,
                min_area: 0,
                jpeg_quality: 85,
                min_savings_bytes: 1,
                min_savings_percent: 0,
                ..ImageOptimizationOptions::default()
            },
            crate::FlateLevel::SIZE,
        )
    }

    fn paper_rgb(width: usize, height: usize) -> Vec<u8> {
        let mut rgb = vec![248_u8; width * height * 3];
        for y in 0..height {
            for x in 0..width {
                let offset = (y * width + x) * 3;
                if x % 97 < 4 || y % 113 < 3 {
                    let shade = u8::try_from((x + y) % 80).unwrap_or_default();
                    rgb[offset] = shade;
                    rgb[offset + 1] = shade;
                    rgb[offset + 2] = shade;
                } else if x % 211 < 8 && y % 173 < 12 {
                    rgb[offset] = 30;
                    rgb[offset + 1] = 80;
                    rgb[offset + 2] = 170;
                }
            }
        }
        rgb
    }

    #[test]
    fn entropy_optimization_cannot_admit_lossy_candidate() -> Result<()> {
        let raw = rgb_jpeg(85)?;
        let Some(entropy_optimized) = crate::jpeg_optimize::optimized_jpeg_bytes(&raw)
            .filter(|optimized| optimized.len() < raw.len())
        else {
            return Err(Error::Invalid(
                "test JPEG has no entropy-optimization opportunity".to_owned(),
            ));
        };
        let options = ImageOptimizationOptions {
            min_savings_bytes: 1,
            min_savings_percent: 20,
            ..ImageOptimizationOptions::default()
        };
        let raw_len = u64::try_from(raw.len())
            .map_err(|_| Error::Invalid("test JPEG length exceeds u64".to_owned()))?;
        let entropy_len = u64::try_from(entropy_optimized.len())
            .map_err(|_| Error::Invalid("optimized test JPEG length exceeds u64".to_owned()))?;
        let Some(original) =
            (raw_len.saturating_add(1)..=raw_len.saturating_mul(2)).find(|&size| {
                !passes_savings(options, size, raw.len())
                    && passes_savings(options, size, entropy_optimized.len())
            })
        else {
            return Err(Error::Invalid(format!(
                "unable to construct admission boundary for raw={raw_len}, entropy={entropy_len}"
            )));
        };

        assert!(!passes_savings(options, original, raw.len()));
        assert!(passes_savings(options, original, entropy_optimized.len()));
        assert!(finalize_jpeg_candidate(raw, original, options).is_none());
        Ok(())
    }

    #[test]
    fn direct_jpeg_quality_gate_is_monotone_and_idempotent() -> Result<()> {
        let q100 = rgb_jpeg(100)?;
        let q90 = rgb_jpeg(90)?;
        let q85 = rgb_jpeg(85)?;
        let q75 = rgb_jpeg(75)?;

        assert!(direct_jpeg_is_strictly_finer_than_target(&q100, 3, 85));
        assert!(direct_jpeg_is_strictly_finer_than_target(&q90, 3, 85));
        assert!(!direct_jpeg_is_strictly_finer_than_target(&q85, 3, 85));
        assert!(!direct_jpeg_is_strictly_finer_than_target(&q75, 3, 85));
        assert!(!direct_jpeg_is_strictly_finer_than_target(&q100, 4, 85));
        assert!(!direct_jpeg_is_strictly_finer_than_target(
            b"not jpeg",
            3,
            85
        ));
        Ok(())
    }

    #[test]
    fn layered_indexed_install_uses_unit_square_form_and_full_grid_children() -> Result<()> {
        let jpeg = rgb_jpeg(85)?;
        let mut document = EditDocument::from_bytes(direct_jpeg_pdf(&jpeg, b"")?)?;
        let images = document.reachable_streams_with_subtype(b"Image")?;
        let Some(&image) = images.first() else {
            return Err(Error::Invalid("test image is unreachable".to_owned()));
        };
        let pixels = 64_usize * 64;
        let first = encode_flate(&vec![1_u8; pixels], crate::FlateLevel::SIZE)?;
        let second = encode_flate(&vec![0_u8; pixels], crate::FlateLevel::SIZE)?;
        install_layered_indexed_transform(
            &mut document,
            image,
            64,
            64,
            LayeredIndexedTransform::Local {
                layers: vec![
                    LayeredIndexedLayerTransform {
                        encoded: first,
                        palette: vec![0_u8; 255 * 3],
                    },
                    LayeredIndexedLayerTransform {
                        encoded: second,
                        palette: vec![255_u8; 255 * 3],
                    },
                ],
                base_color_space: OwnedObject::Name(b"DeviceRGB".to_vec()),
            },
        )?;
        let Some(object) = document.current_owned_object(image)? else {
            return Err(Error::Invalid("installed Form is missing".to_owned()));
        };
        let OwnedObject::Stream { dictionary, data } = object else {
            return Err(Error::Invalid(
                "installed object is not a stream".to_owned(),
            ));
        };
        assert_eq!(
            resolved_name(&document, dictionary.get(b"Subtype".as_slice()))?.as_deref(),
            Some(b"Form".as_slice())
        );
        let Some(OwnedObject::Array(bbox)) = dictionary.get(b"BBox".as_slice()) else {
            return Err(Error::Invalid("installed Form has no BBox".to_owned()));
        };
        assert_eq!(
            bbox.as_slice(),
            &[
                OwnedObject::Integer(0),
                OwnedObject::Integer(0),
                OwnedObject::Integer(1),
                OwnedObject::Integer(1),
            ]
        );
        let StreamData::Owned(content) = data else {
            return Err(Error::Invalid(
                "installed Form content is not owned".to_owned(),
            ));
        };
        assert!(content.windows(3).any(|window| window == b" Do"));
        let Some(OwnedObject::Dictionary(resources)) = dictionary.get(b"Resources".as_slice())
        else {
            return Err(Error::Invalid("installed Form has no Resources".to_owned()));
        };
        let Some(OwnedObject::Dictionary(xobjects)) = resources.get(b"XObject".as_slice()) else {
            return Err(Error::Invalid(
                "installed Form has no XObject resources".to_owned(),
            ));
        };
        assert_eq!(xobjects.len(), 2);
        for child in xobjects.values() {
            let Some(OwnedObject::Stream { dictionary, .. }) =
                document.resolve_owned_value(child)?
            else {
                return Err(Error::Invalid(
                    "local palette child is not an image stream".to_owned(),
                ));
            };
            assert_eq!(
                resolved_name(&document, dictionary.get(b"Subtype".as_slice()))?.as_deref(),
                Some(b"Image".as_slice())
            );
            assert_eq!(dimension(&document, &dictionary, b"Width")?, Some(64));
            assert_eq!(dimension(&document, &dictionary, b"Height")?, Some(64));
            let Some(OwnedObject::Array(mask)) = dictionary.get(b"Mask".as_slice()) else {
                return Err(Error::Invalid(
                    "local palette child has no color-key mask".to_owned(),
                ));
            };
            assert_eq!(
                mask.as_slice(),
                &[OwnedObject::Integer(0), OwnedObject::Integer(0)]
            );
        }
        Ok(())
    }

    #[test]
    fn residual_indexed_install_paints_opaque_base_then_transparent_correction() -> Result<()> {
        let jpeg = rgb_jpeg(85)?;
        let mut document = EditDocument::from_bytes(direct_jpeg_pdf(&jpeg, b"")?)?;
        let images = document.reachable_streams_with_subtype(b"Image")?;
        let Some(&image) = images.first() else {
            return Err(Error::Invalid("test image is unreachable".to_owned()));
        };
        let pixels = 64_usize * 64;
        let base_encoded = encode_flate(&vec![0_u8; pixels], crate::FlateLevel::SIZE)?;
        let mut residual_indices = vec![0_u8; pixels];
        residual_indices[pixels / 2] = 1;
        let residual_encoded = encode_flate(&residual_indices, crate::FlateLevel::SIZE)?;
        install_layered_indexed_transform(
            &mut document,
            image,
            64,
            64,
            LayeredIndexedTransform::Residual(ResidualIndexedTransform {
                base_encoded,
                base_palette: vec![248_u8; 256 * 3],
                residual_encoded,
                residual_palette: vec![10, 80, 200],
                base_color_space: OwnedObject::Name(b"DeviceRGB".to_vec()),
            }),
        )?;
        let Some(OwnedObject::Stream { dictionary, data }) =
            document.current_owned_object(image)?
        else {
            return Err(Error::Invalid("residual Form is missing".to_owned()));
        };
        let StreamData::Owned(content) = data else {
            return Err(Error::Invalid(
                "residual Form content is not owned".to_owned(),
            ));
        };
        assert_eq!(
            content,
            b"/PdfRedoxResidualBase Do\n/PdfRedoxResidualCorrection Do\n"
        );
        let Some(OwnedObject::Dictionary(resources)) = dictionary.get(b"Resources".as_slice())
        else {
            return Err(Error::Invalid("residual Form has no Resources".to_owned()));
        };
        let Some(OwnedObject::Dictionary(xobjects)) = resources.get(b"XObject".as_slice()) else {
            return Err(Error::Invalid("residual Form has no XObjects".to_owned()));
        };
        let Some(base) = xobjects.get(b"PdfRedoxResidualBase".as_slice()) else {
            return Err(Error::Invalid("residual Form has no base child".to_owned()));
        };
        let Some(OwnedObject::Stream {
            dictionary: base_dictionary,
            ..
        }) = document.resolve_owned_value(base)?
        else {
            return Err(Error::Invalid("residual base is not an image".to_owned()));
        };
        assert!(!base_dictionary.contains_key(b"Mask".as_slice()));
        let Some(correction) = xobjects.get(b"PdfRedoxResidualCorrection".as_slice()) else {
            return Err(Error::Invalid(
                "residual Form has no correction child".to_owned(),
            ));
        };
        let Some(OwnedObject::Stream {
            dictionary: correction_dictionary,
            ..
        }) = document.resolve_owned_value(correction)?
        else {
            return Err(Error::Invalid(
                "residual correction is not an image".to_owned(),
            ));
        };
        let Some(OwnedObject::Array(mask)) = correction_dictionary.get(b"Mask".as_slice()) else {
            return Err(Error::Invalid(
                "residual correction has no color-key mask".to_owned(),
            ));
        };
        assert_eq!(
            mask.as_slice(),
            &[OwnedObject::Integer(0), OwnedObject::Integer(0)]
        );
        Ok(())
    }

    #[test]
    fn layered_indexed_only_discards_proven_opaque_smask() -> Result<()> {
        let jpeg = rgb_jpeg(85)?;
        for (sample, expected) in [(u8::MAX, true), (u8::MAX - 1, false)] {
            let document = EditDocument::from_bytes(image_with_smask_pdf(&jpeg, sample)?)?;
            let images = document.reachable_streams_with_subtype(b"Image")?;
            let color = images.into_iter().find(|&handle| {
                let Ok(Some(object)) = document.current_owned_object(handle) else {
                    return false;
                };
                let Some(dictionary) = object.as_dictionary() else {
                    return false;
                };
                matches!(
                    resolved_name(&document, dictionary.get(b"ColorSpace".as_slice())),
                    Ok(Some(name)) if name.as_slice() == b"DeviceRGB"
                )
            });
            let Some(color) = color else {
                return Err(Error::Invalid("test color image is unreachable".to_owned()));
            };
            let Some(object) = document.current_owned_object(color)? else {
                return Err(Error::Invalid("test color image is missing".to_owned()));
            };
            let Some(dictionary) = object.as_dictionary() else {
                return Err(Error::Invalid(
                    "test color image has no dictionary".to_owned(),
                ));
            };
            assert_eq!(redundant_opaque_smask(&document, dictionary)?, expected);
        }
        Ok(())
    }

    #[test]
    fn single_indexed_install_preserves_existing_opaque_smask() -> Result<()> {
        let jpeg = rgb_jpeg(85)?;
        let mut document = EditDocument::from_bytes(image_with_smask_pdf(&jpeg, u8::MAX)?)?;
        let images = document.reachable_streams_with_subtype(b"Image")?;
        let color = images.into_iter().find(|&handle| {
            let Ok(Some(object)) = document.current_owned_object(handle) else {
                return false;
            };
            let Some(dictionary) = object.as_dictionary() else {
                return false;
            };
            matches!(
                resolved_name(&document, dictionary.get(b"ColorSpace".as_slice())),
                Ok(Some(name)) if name.as_slice() == b"DeviceRGB"
            )
        });
        let Some(color) = color else {
            return Err(Error::Invalid("test color image is unreachable".to_owned()));
        };
        let Some(before) = document.current_owned_object(color)? else {
            return Err(Error::Invalid("test color image is missing".to_owned()));
        };
        let Some(before_dictionary) = before.as_dictionary() else {
            return Err(Error::Invalid(
                "test color image has no dictionary".to_owned(),
            ));
        };
        let Some(smask) = before_dictionary.get(b"SMask".as_slice()).cloned() else {
            return Err(Error::Invalid("test color image has no SMask".to_owned()));
        };
        assert!(redundant_opaque_smask(&document, before_dictionary)?);

        let indices = vec![0_u8; 64 * 64];
        let encoded = encode_flate(&indices, crate::FlateLevel::SIZE)?;
        install_transform(
            &mut document,
            color,
            ImageTransform {
                encoded,
                width: 64,
                height: 64,
                filter: b"FlateDecode".to_vec(),
                color_space: Some(OwnedObject::Array(vec![
                    OwnedObject::Name(b"Indexed".to_vec()),
                    OwnedObject::Name(b"DeviceRGB".to_vec()),
                    OwnedObject::Integer(0),
                    OwnedObject::String(vec![0, 0, 0]),
                ])),
                layered_indexed: None,
                original_encoded_bytes: u64::try_from(jpeg.len()).unwrap_or(u64::MAX),
                optimized_encoded_bytes: 0,
                original_pixels: 64 * 64,
                optimized_pixels: 64 * 64,
                resized: false,
                paper_indexed: true,
                encoding: ImageResizeEncoding::Flate,
            },
        )?;
        let Some(after) = document.current_owned_object(color)? else {
            return Err(Error::Invalid(
                "rewritten color image is missing".to_owned(),
            ));
        };
        let Some(after_dictionary) = after.as_dictionary() else {
            return Err(Error::Invalid(
                "rewritten color image has no dictionary".to_owned(),
            ));
        };
        assert_eq!(after_dictionary.get(b"SMask".as_slice()), Some(&smask));
        assert!(redundant_opaque_smask(&document, after_dictionary)?);
        Ok(())
    }

    #[test]
    fn layered_indexed_reference_guard_allows_plain_xobject_binding() -> Result<()> {
        let jpeg = rgb_jpeg(85)?;
        let document = EditDocument::from_bytes(direct_jpeg_pdf(&jpeg, b"")?)?;
        let images = document.reachable_streams_with_subtype(b"Image")?;
        let Some(&image) = images.first() else {
            return Err(Error::Invalid("test image is unreachable".to_owned()));
        };
        let bindings = image_binding_counts(&document)?;
        let unsafe_targets = non_xobject_reference_targets(&document)?;
        assert!(bindings.get(&image).copied().unwrap_or(0) > 0);
        assert!(!unsafe_targets.contains(&image));
        Ok(())
    }

    #[test]
    fn layered_indexed_reference_guard_rejects_unrelated_xobject_key() -> Result<()> {
        let jpeg = rgb_jpeg(85)?;
        let document = EditDocument::from_bytes(unrelated_xobject_key_pdf(&jpeg)?)?;
        let images = document.reachable_streams_with_subtype(b"Image")?;
        let Some(&image) = images.first() else {
            return Err(Error::Invalid("test image is unreachable".to_owned()));
        };
        let bindings = image_binding_counts(&document)?;
        let unsafe_targets = non_xobject_reference_targets(&document)?;
        assert!(bindings.get(&image).copied().unwrap_or(0) > 0);
        assert!(unsafe_targets.contains(&image));
        Ok(())
    }

    #[test]
    fn layered_indexed_reference_guard_fails_closed_on_indirect_xobject_map() -> Result<()> {
        let jpeg = rgb_jpeg(85)?;
        let document = EditDocument::from_bytes(indirect_xobject_map_pdf(&jpeg)?)?;
        let images = document.reachable_streams_with_subtype(b"Image")?;
        let Some(&image) = images.first() else {
            return Err(Error::Invalid("test image is unreachable".to_owned()));
        };
        let bindings = image_binding_counts(&document)?;
        let unsafe_targets = non_xobject_reference_targets(&document)?;
        assert!(bindings.get(&image).copied().unwrap_or(0) > 0);
        assert!(unsafe_targets.contains(&image));
        Ok(())
    }

    #[test]
    fn layered_indexed_reference_guard_rejects_smask_target() -> Result<()> {
        let jpeg = rgb_jpeg(85)?;
        let document = EditDocument::from_bytes(image_with_smask_pdf(&jpeg, u8::MAX)?)?;
        let images = document.reachable_streams_with_subtype(b"Image")?;
        let mask = images.into_iter().find(|&handle| {
            let Ok(Some(object)) = document.current_owned_object(handle) else {
                return false;
            };
            let Some(dictionary) = object.as_dictionary() else {
                return false;
            };
            matches!(
                resolved_name(&document, dictionary.get(b"ColorSpace".as_slice())),
                Ok(Some(name)) if name.as_slice() == b"DeviceGray"
            )
        });
        let Some(mask) = mask else {
            return Err(Error::Invalid("test SMask image is unreachable".to_owned()));
        };
        let bindings = image_binding_counts(&document)?;
        let unsafe_targets = non_xobject_reference_targets(&document)?;
        assert!(bindings.get(&mask).copied().unwrap_or(0) > 0);
        assert!(unsafe_targets.contains(&mask));
        Ok(())
    }

    #[test]
    fn direct_jpeg_transcode_only_requantizes_safe_finer_sources() -> Result<()> {
        let q100 = rgb_jpeg(100)?;
        let q85 = rgb_jpeg(85)?;
        let q75 = rgb_jpeg(75)?;

        let Some(transform) = direct_jpeg_transform(&q100, b"")? else {
            return Err(Error::Invalid("q100 JPEG was not transcoded".to_owned()));
        };
        assert_eq!(transform.width, 64);
        assert_eq!(transform.height, 64);
        assert_eq!(transform.filter, b"DCTDecode");
        assert!(transform.encoded.len() < q100.len());
        assert!(
            crate::jpeg_optimize::optimized_jpeg_bytes(&transform.encoded)
                .is_none_or(|optimized| optimized.len() >= transform.encoded.len())
        );

        assert!(direct_jpeg_transform(&q85, b"")?.is_none());
        assert!(direct_jpeg_transform(&q75, b"")?.is_none());
        assert!(direct_jpeg_transform(&q100, b"/Mask [0 0 0 0 0 0]")?.is_none());
        assert!(direct_jpeg_transform(&q100, b"/Decode [0 1 0 1 0 1]")?.is_none());
        assert!(direct_jpeg_transform(&q100, b"/DecodeParms << /ColorTransform 1 >>")?.is_none());
        Ok(())
    }

    #[test]
    fn paper_indexed_transform_preserves_verified_iccbased_rgb_base() -> Result<()> {
        let Some(transform) = iccbased_flate_transform(*b"RGB ", false)? else {
            return Err(Error::Invalid(
                "verified ICCBased RGB image did not produce an Indexed candidate".to_owned(),
            ));
        };
        let Some(OwnedObject::Array(color_space)) = transform.color_space else {
            return Err(Error::Invalid(
                "ICCBased transform did not emit an Indexed color space".to_owned(),
            ));
        };
        assert!(matches!(
            color_space.first(),
            Some(OwnedObject::Name(name)) if name.as_slice() == b"Indexed"
        ));
        let Some(OwnedObject::Array(base)) = color_space.get(1) else {
            return Err(Error::Invalid(
                "Indexed transform did not preserve the ICCBased base".to_owned(),
            ));
        };
        assert!(matches!(
            base.first(),
            Some(OwnedObject::Name(name)) if name.as_slice() == b"ICCBased"
        ));
        assert!(matches!(base.get(1), Some(OwnedObject::Reference(_))));
        Ok(())
    }

    #[test]
    fn paper_indexed_transform_preserves_indirect_iccbased_rgb_base() -> Result<()> {
        let Some(transform) = iccbased_flate_transform(*b"RGB ", true)? else {
            return Err(Error::Invalid(
                "indirect ICCBased RGB image did not produce an Indexed candidate".to_owned(),
            ));
        };
        let Some(OwnedObject::Array(color_space)) = transform.color_space else {
            return Err(Error::Invalid(
                "indirect ICCBased transform did not emit an Indexed color space".to_owned(),
            ));
        };
        assert!(matches!(
            color_space.get(1),
            Some(OwnedObject::Reference(_))
        ));
        Ok(())
    }

    #[test]
    fn paper_indexed_transform_rejects_non_rgb_icc_header() -> Result<()> {
        assert!(iccbased_flate_transform(*b"LAB ", false)?.is_none());
        Ok(())
    }

    #[test]
    fn paper_indexed_transform_emits_complete_indexed_rgb_image() -> Result<()> {
        let width = 800_u32;
        let height = 700_u32;
        let rgb = paper_rgb(
            usize::try_from(width)
                .map_err(|_| Error::Invalid("test width exceeds usize".to_owned()))?,
            usize::try_from(height)
                .map_err(|_| Error::Invalid("test height exceeds usize".to_owned()))?,
        );
        let original = u64::try_from(rgb.len())
            .map_err(|_| Error::Invalid("test RGB payload exceeds u64".to_owned()))?;
        let Some(transform) = paper_indexed_transform(
            &rgb,
            width,
            height,
            original,
            ImageOptimizationOptions {
                min_width: 0,
                min_height: 0,
                min_area: 0,
                min_savings_bytes: 1,
                min_savings_percent: 0,
                ..ImageOptimizationOptions::default()
            },
            crate::FlateLevel::SIZE,
            OwnedObject::Name(b"DeviceRGB".to_vec()),
        )?
        else {
            return Err(Error::Invalid(
                "paper-like test image did not produce an Indexed candidate".to_owned(),
            ));
        };

        assert!(transform.paper_indexed);
        assert!(!transform.resized);
        assert_eq!(transform.width, width);
        assert_eq!(transform.height, height);
        assert_eq!(transform.filter, b"FlateDecode");
        assert_eq!(
            transform.original_pixels,
            u64::from(width) * u64::from(height)
        );
        assert_eq!(transform.optimized_pixels, transform.original_pixels);
        let Some(OwnedObject::Array(color_space)) = transform.color_space.as_ref() else {
            return Err(Error::Invalid(
                "paper Indexed candidate has no Indexed color space".to_owned(),
            ));
        };
        assert_eq!(color_space.len(), 4);
        assert_eq!(color_space[0], OwnedObject::Name(b"Indexed".to_vec()));
        assert_eq!(color_space[1], OwnedObject::Name(b"DeviceRGB".to_vec()));
        assert_eq!(color_space[2], OwnedObject::Integer(255));
        let OwnedObject::String(palette) = &color_space[3] else {
            return Err(Error::Invalid(
                "paper Indexed candidate palette is not a lookup string".to_owned(),
            ));
        };
        assert_eq!(palette.len(), 256 * 3);
        let expected_accounted = transform
            .encoded
            .len()
            .saturating_add(palette.len().saturating_mul(2).saturating_add(2));
        assert_eq!(
            transform.optimized_encoded_bytes,
            u64::try_from(expected_accounted)
                .map_err(|_| Error::Invalid("test optimized payload exceeds u64".to_owned()))?
        );
        assert!(transform.optimized_encoded_bytes < transform.original_encoded_bytes);
        Ok(())
    }
}
