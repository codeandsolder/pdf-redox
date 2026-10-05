use crate::{
    EditDocument, Error, ObjectHandle, OwnedDictionary, OwnedObject, Result, StreamData,
    stream_codec::{decode_image_stream, encode_flate},
};
use fast_image_resize::{
    FilterType, PixelType, ResizeAlg, ResizeOptions, Resizer, images::Image as ResizeImage,
};
use libjpeg_turbo_rs::{PixelFormat, Subsampling, compress};
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet, VecDeque};

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
    pub flate_level: i32,
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
            flate_level: -1,
            min_savings_bytes: 1,
            min_savings_percent: 0,
        }
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ImageOptimizationStats {
    pub images_optimized: usize,
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
struct ImageTransform {
    encoded: Vec<u8>,
    width: u32,
    height: u32,
    filter: Vec<u8>,
    original_encoded_bytes: u64,
    original_pixels: u64,
    optimized_pixels: u64,
    resized: bool,
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

fn resolved_number(document: &EditDocument, value: Option<&OwnedObject>) -> Result<Option<f64>> {
    let Some(value) = value else {
        return Ok(None);
    };
    Ok(match document.resolve_owned_value(value)? {
        Some(OwnedObject::Integer(value)) => crate::source::exact_i64_to_f64(value),
        Some(OwnedObject::Real(value)) => Some(value),
        _ => None,
    })
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

fn install_transform(
    document: &mut EditDocument,
    handle: ObjectHandle,
    transform: ImageTransform,
) -> Result<()> {
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

fn transcode_image(
    document: &EditDocument,
    image: &OwnedObject,
    options: ImageOptimizationOptions,
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
    let Some((components, _, _)) = color_info(document, dictionary)? else {
        return Ok(None);
    };
    if matches!(
        source_filter(document, dictionary)?.as_deref(),
        Some(b"DCTDecode" | b"DCT")
    ) {
        return Ok(None);
    }
    let area = width.wrapping_mul(height);
    if (options.min_width > 0 && width <= options.min_width)
        || (options.min_height > 0 && height <= options.min_height)
        || (options.min_area > 0 && area <= options.min_area)
    {
        return Ok(None);
    }
    let Some(decoded) = decoded_pixels(document, image, width, height, components)? else {
        return Ok(None);
    };
    let encoded = encode_jpeg_raster(width, height, components, &decoded, options.jpeg_quality)?;
    let original = u64::try_from(data.bytes(document.source())?.len()).unwrap_or(u64::MAX);
    if !passes_savings(options, original, encoded.len()) {
        return Ok(None);
    }
    let pixels = u64::from(width) * u64::from(height);
    Ok(Some(ImageTransform {
        encoded,
        width,
        height,
        filter: b"DCTDecode".to_vec(),
        original_encoded_bytes: original,
        original_pixels: pixels,
        optimized_pixels: pixels,
        resized: false,
        encoding: ImageResizeEncoding::Jpeg,
    }))
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
            encode_flate(destination.buffer(), options.flate_level)?,
            b"FlateDecode".to_vec(),
        ),
    };
    let original = u64::try_from(data.bytes(document.source())?.len()).unwrap_or(u64::MAX);
    if !passes_savings(options, original, encoded.len()) {
        return Ok(None);
    }
    Ok(Some(ImageTransform {
        encoded,
        width: target_width,
        height: target_height,
        filter,
        original_encoded_bytes: original,
        original_pixels: u64::from(width) * u64::from(height),
        optimized_pixels: u64::from(target_width) * u64::from(target_height),
        resized: true,
        encoding: target.encoding,
    }))
}

fn transform_one(
    document: &mut EditDocument,
    handle: ObjectHandle,
    options: ImageOptimizationOptions,
    target: Option<ImageResizeTarget>,
    stats: &mut ImageOptimizationStats,
) -> Result<bool> {
    if !is_image(document, handle)? {
        return Ok(false);
    }
    let Some(image) = document.current_owned_object(handle)? else {
        return Ok(false);
    };
    let transform = match target {
        Some(target) => resize_image(document, &image, options, target)?,
        None => transcode_image(document, &image, options)?,
    };
    let Some(transform) = transform else {
        return Ok(false);
    };
    stats.images_optimized = stats.images_optimized.saturating_add(1);
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
        .saturating_add(u64::try_from(transform.encoded.len()).unwrap_or(u64::MAX));
    stats.original_pixels = stats
        .original_pixels
        .saturating_add(transform.original_pixels);
    stats.optimized_pixels = stats
        .optimized_pixels
        .saturating_add(transform.optimized_pixels);
    install_transform(document, handle, transform)?;
    Ok(true)
}

pub fn optimize_images_hayro(
    document: &mut EditDocument,
    options: ImageOptimizationOptions,
) -> Result<ImageOptimizationStats> {
    let binding_counts = image_binding_counts(document)?;
    let mut stats = ImageOptimizationStats::default();
    let images = document
        .reachable_streams_with_subtype(b"Image")?
        .into_iter()
        .collect::<BTreeSet<_>>();
    for image in images {
        if transform_one(document, image, options, None, &mut stats)? {
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

pub fn optimize_images_with_resize_targets_hayro(
    document: &mut EditDocument,
    options: ImageOptimizationOptions,
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
    for (image, target) in by_image {
        if transform_one(document, image, options, Some(target), &mut stats)? {
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
